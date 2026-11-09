//! `flashcp sync`: make a directory of an EXISTING exFAT volume match a local tree, writing only
//! what differs, without ever overwriting a live cluster, and so that no crash can lose a file
//! or its name.
//!
//! Everything is parsed, verified and planned before the first byte is written (as `add`):
//! the whole volume's allocation is checked against the bitmap, and every directory under the
//! destination is kept in memory. Per directory, source and volume entries are matched by
//! up-cased name:
//!
//! - unchanged: the QUICK CHECK, same size and same LastModified at exFAT's 10 ms precision
//!   ([`same_mtime`]); nothing is written. A change that keeps both the size and the
//!   modification time (`cp -p`, `rsync -t`, some editors and tools restore the time) is NOT
//!   seen by it: `--checksum` compares the contents instead (read in physical order);
//! - changed: the new version goes into FRESH clusters (the pre-sync free space only), then the
//!   entry set is switched over to it (FirstCluster, flags, lengths, LastModified,
//!   SetChecksum); the old clusters are freed last;
//! - a name that differs only by case is renamed (same length and NameHash);
//! - new: appended (or written into a new directory) as `add` does;
//! - extraneous (on the volume, not in the source): kept, unless `--delete` (which needs
//!   `--yes`, and `--yes-many` past 1000 entries; `--dry-run` lists them without either);
//! - with `--delete`, a new file and a file that goes away with the same size and the same
//!   decoded LastModified, whose FINGERPRINTS (a hash of the first and last 4 KB and six 4 KB
//!   samples between, or of the whole file up to 32 KB) are equal, and (unless `--fast-moves`)
//!   whose full contents are then compared equal, is a MOVE: its new entry points at the old
//!   clusters, nothing is copied. Full comparisons are capped at [`VERIFY_BUDGET`] bytes per
//!   run; pairs past it are copied. `--checksum` already has the full hashes and reuses them.
//!
//! ## Entry-set edits, in place or relocated
//!
//! An existing entry set is edited IN PLACE only when every byte that changes lies in one
//! 512-byte sector (directory clusters are sector aligned, so offsets in a directory are
//! device offsets mod 512): sector writes are atomic, and the unchanged bytes around the change
//! are rewritten identically. A retime touches bytes 2..4 and 12..24 (always one sector); a
//! re-point or a directory's new length touches 2..64 (two sectors iff the set starts at
//! offset 480 of one); a case rename touches 2..4 and the name characters that change.
//! Otherwise the set is RELOCATED: a complete new set (the old one with the changes applied,
//! vendor secondaries and creation time kept, valid checksum) is appended at the directory's
//! end in `link`, and the old set's primary entry is marked deleted (0x85 → 0x05, one byte) in
//! `retire`; a relocated directory's two sets point at the same clusters. Unlinking any set
//! (a deletion, a move's source, a type-conflict loser, a duplicate) likewise clears only the
//! primary's InUse bit in `unlink`; the secondaries' InUse bits are cleared in `free`.
//!
//! ## Phases and crash states
//!
//! A sync runs between every phase (`FLASHCP_TEST_STOP_AFTER=<phase>` stops after it):
//!
//! | phase    | writes                                                                     |
//! |----------|----------------------------------------------------------------------------|
//! | `dirty`  | (after the UNDO journal) VolumeDirty set; nothing to do → nothing written   |
//! | `data`   | new files and versions, new directories, extension clusters (free space)   |
//! | `alloc`  | (after the REDO journal) bitmap bits of new clusters, chains of new objects |
//! | `fat`    | FAT links extending existing directory chains                               |
//! | `unlink` | primaries of deleted / moved-away / conflicting / duplicate sets → 0x05    |
//! | `link`   | appended sets (new, moved, relocated), lengths, in-place edits             |
//! | `retire` | primaries of relocated sets' old copies → 0x05                             |
//! | `free`   | those sets' secondaries and old orphans: InUse cleared; bitmap bits freed  |
//! | `clean`  | VolumeDirty cleared, PercentInUse; then the journal is removed             |
//!
//! The journal (see [`crate::journal`]) holds every write from `alloc` to `clean` with its
//! before and after bytes, on the local machine; the start of every sync (and
//! `flashcp sync --recover`) replays it, so with the journal every crash state below simply
//! completes. Without it (another machine, or the state directory lost) the volume is dirty and
//! is refused until `fsck.exfat -y` has run; what that leaves, and what the next sync does:
//!
//! | crash after | volume                                    | `fsck.exfat -n` / `-y`, then sync     |
//! |-------------|-------------------------------------------|----------------------------------------|
//! | dirty, data | old tree; data in free clusters (undo journal: the dirty flag is cleared) | clean / dirty flag cleared; sync redoes it |
//! | alloc, fat  | old tree, leaked clusters                 | clean; leaks stay allocated (as `add`) |
//! | unlink      | gone: deleted, moved-away entries (orphan secondaries left); changed files still OLD | "clean" with an "unknown entry type 0xc0" error per orphan; `-y` clears them; moved files no longer in SRC are lost only here and only without the journal |
//! | link        | the new tree, plus DUPLICATES: each relocated set's old copy is still live | re-point duplicate (same name, other clusters): corrupted, `-y` renames the later (new) set to `FILE0000000.CHK` (sync rewrites the name from SRC; `--delete` removes the .CHK); relocated rename (same clusters): "cluster is already allocated", `-y` truncates the later set to 0 bytes (a directory to DataLength 0); sync keeps the set with the data and renames it again |
//! | retire      | the new tree, orphan secondaries          | as after unlink                        |
//! | free        | done but dirty                            | clean                                  |
//! | torn link   | a relocated set half written (its old copy still live) | "checksum of a file is wrong", corrupted; `-y` deletes the torn set: the file is there with its OLD version, sync redoes it |
//!
//! No crash state loses a file that is in SRC, nor any name: a directory never has a name
//! missing between phases (a new set is live before its old copy dies). The parser accepts the
//! leftovers: ORPHAN secondaries (in-use secondaries right after a deleted 0x05 file entry,
//! within its SecondaryCount) are skipped by every command and cleared by sync's `free`
//! whenever it writes, or on a run that has nothing else to do; DUPLICATE names are accepted by
//! sync only (everyone else refuses them with a hint to re-run sync). Sync resolves each
//! duplicate before matching: the KEEPER is the set that matches the source entry best (a
//! directory owning clusters over a zero-length one, then the source's size and time, then its
//! exact name, then the higher offset, i.e. the newer set); the others are unlinked (no
//! `--delete` or `--yes` needed: "repaired N interrupted entries") and their clusters freed
//! unless the keeper shares them.
//!
//! Directory metadata is batched: each phase applies its changes to the in-memory copies of the
//! directories and writes, per directory, the touched byte ranges rounded to 512 bytes and
//! merged when less than 64 KB apart (the bytes between are rewritten unchanged, so every
//! sector still goes from one consistent state to the next). Appends go at the end of a
//! directory; deleted slots are not reused.

use crate::add::{
    AddStats, Patches, Seg, SegKind, carve, chain_entries, choose_extents, clean_flags, data_gap_policy,
    data_write_bytes, free_extents, run_clusters, stop_after, take_exact, working_flags, write_data, write_flags,
};
use crate::events::{Event, Op, Sink, Ticker, json_str};
use crate::exfat::{
    Node, TargetSpec, checksum16, entry_set, finish_set, scan_tree, set_bytes, set_direct, ts, validate_name,
};
use crate::journal::{self, JWrite, Journal, MoveRec};
use crate::util::human_bytes;
use crate::volume::*;
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub use crate::journal::Recovery;

#[derive(Clone, Debug, Default)]
pub struct SyncOpts {
    /// Directory inside the volume to sync into ("a/b/c"); missing parents are created.
    pub to: Option<String>,
    /// Delete volume entries that are not in the source (and detect moves).
    pub delete: bool,
    /// Compare file contents instead of size and modification time.
    pub checksum: bool,
    pub readers: usize,
    pub dry_run: bool,
    /// Confirm deletions (required when anything would be deleted).
    pub yes: bool,
    /// Confirm deleting more than [`MANY_DELETES`] entries.
    pub yes_many: bool,
    /// Accept a move on equal fingerprints, without comparing the full contents.
    pub fast_moves: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SyncStats {
    pub unchanged: u64,
    pub new: u64,
    /// Rewritten, or (with --checksum, same content) only retimed.
    pub changed: u64,
    /// Re-linked under a new path without copying, or renamed (case only).
    pub moved: u64,
    /// Files deleted, including those inside deleted directories.
    pub deleted: u64,
    pub deleted_dirs: u64,
    pub new_dirs: u64,
    /// File bytes written.
    pub data_written: u64,
    /// Bytes of unchanged and moved files (not written).
    pub data_skipped: u64,
    /// Everything written (data, metadata, boot sector).
    pub written: u64,
    pub cluster: u64,
    pub secs: f64,
    /// Entry sets rewritten at the end of their directory because an in-place edit would have
    /// crossed a sector boundary.
    pub relocated: u64,
    /// Duplicate entry sets an interrupted sync left, unlinked.
    pub repaired: u64,
    /// Orphan secondary entries cleared.
    pub orphans: u64,
    /// Moves whose full content was compared (or whose fingerprint is the full content).
    pub moves_verified: u64,
    /// Moves accepted on their fingerprint alone (--fast-moves).
    pub moves_fast: u64,
    /// Move candidates copied instead: past the verification budget, or not worth verifying.
    pub moves_copied_budget: u64,
}

/// Deleting more entries than this needs `--yes-many` as well as `--yes`.
pub const MANY_DELETES: u64 = 1000;
/// Bytes of full content comparisons of move candidates per run; candidates past it are copied.
pub const VERIFY_BUDGET: u64 = 2 << 30;
const TORN_ENV: &str = "FLASHCP_TEST_TORN";

pub fn sync(src: &Path, target: &Path, o: &SyncOpts, sink: Sink<'_>) -> Result<SyncStats> {
    sync_to(src, &TargetSpec::from(target), o, sink)
}

/// `flashcp sync --recover`: replays the journal of an interrupted sync of `target`, if any.
pub fn recover(target: &TargetSpec) -> Result<Recovery> {
    let tgt = target.open_rw()?;
    set_direct(&tgt.file, false);
    journal::recover(&tgt.file)
}

/// The note a recovery prints (None when there was nothing to do).
pub fn recovery_note(r: &Recovery) -> Option<String> {
    match r {
        Recovery::None => None,
        Recovery::Stale => Some("note: removed a stale sync journal (the volume is clean)".into()),
        Recovery::Replayed { undo: true, .. } => {
            Some("note: an interrupted sync had written only file data; its dirty flag is cleared".into())
        }
        Recovery::Replayed { writes, .. } => {
            Some(format!("note: completed an interrupted sync from its journal ({writes} metadata writes replayed)"))
        }
    }
}

/// Whether comparing a move candidate's content (a device read) is worth it against copying
/// it (a device write of the same size), given the device's rates in bytes/s; unknown → yes.
pub(crate) fn verify_pays(read_rate: Option<f64>, write_rate: Option<f64>) -> bool {
    match (read_rate, write_rate) {
        (Some(r), Some(w)) => r >= w,
        _ => true,
    }
}

// ───────────────────────── plan model ─────────────────────────

/// The changes to one existing entry set (applied in `link`, in place or relocated).
#[derive(Clone, Debug, Default)]
struct Edit {
    len: usize,
    /// The source node (new name, mtime, size, allocation).
    node: Option<usize>,
    /// Point the stream at the node's new version.
    repoint: bool,
    mtime: bool,
    name: bool,
    /// A directory's new DataLength (it grows), and whether it loses NoFatChain.
    dirlen: Option<u64>,
    clear_nfc: bool,
}

/// What a directory gets appended: a new set for a source node, or a relocated set (by its
/// old offset in the same directory).
#[derive(Clone, Copy, Debug)]
enum Item {
    Node(usize),
    Reloc(usize),
}

/// A volume file that goes away (and may be the source of a move).
struct Gone {
    path: String,
    ch: Child,
}

/// The fields a re-point writes into a file's existing entry set (see [`patch_set`]).
#[derive(Clone, Copy, Debug)]
struct Repoint {
    first: u32,
    contiguous: bool,
    len: u64,
}

/// Applies a re-point, a retime and/or a case rename to an entry set (vendor secondaries and
/// every other byte untouched) and recomputes its SetChecksum.
fn patch_set(set: &mut [u8], repoint: Option<Repoint>, mtime: Option<(i64, u32)>, name: Option<&[u16]>) {
    if let Some(r) = repoint {
        let flags = if r.first == 0 || !r.contiguous { 0x01 } else { 0x03 };
        set[33] = (set[33] & !0x03) | flags;
        set[40..48].copy_from_slice(&r.len.to_le_bytes());
        set[52..56].copy_from_slice(&r.first.to_le_bytes());
        set[56..64].copy_from_slice(&r.len.to_le_bytes());
    }
    if let Some(m) = mtime {
        let (t, t10) = ts(m.0, m.1);
        set[12..16].copy_from_slice(&t.to_le_bytes());
        set[21] = t10;
        set[23] = 0x80;
    }
    if let Some(name) = name {
        for (k, chunk) in name.chunks(15).enumerate() {
            for (j, &c) in chunk.iter().enumerate() {
                let at = name_at(k * 15 + j);
                set[at..at + 2].copy_from_slice(&c.to_le_bytes());
            }
        }
    }
    fix_checksum(set);
}

/// Offset in an entry set of name character `i`.
fn name_at(i: usize) -> usize {
    64 + 32 * (i / 15) + 2 + 2 * (i % 15)
}

fn fix_checksum(set: &mut [u8]) {
    let c = checksum16(set, true);
    set[2..4].copy_from_slice(&c.to_le_bytes());
}

/// One past the last byte of the set `e` changes (the first is always 2, the SetChecksum).
fn changed_end(e: &Edit, old: &[u8], name: Option<&[u16]>) -> usize {
    let mut hi = 4;
    if e.mtime {
        hi = hi.max(24);
    }
    if e.repoint || e.dirlen.is_some() || e.clear_nfc {
        hi = hi.max(64);
    }
    if let (true, Some(name)) = (e.name, name) {
        for (i, &c) in name.iter().enumerate() {
            let at = name_at(i);
            if le16(old, at) != c {
                hi = hi.max(at + 2);
            }
        }
    }
    hi
}

/// Whether bytes [off + 2, off + hi) of a directory lie in one 512-byte sector.
fn one_sector(off: usize, hi: usize) -> bool {
    (off + 2) / SECTOR_ALIGN == (off + hi - 1) / SECTOR_ALIGN
}

/// Touched byte ranges per directory, for one metadata phase.
#[derive(Default)]
struct Touched(BTreeMap<usize, Vec<(usize, usize)>>);

/// Ranges closer than this are written as one (the bytes between rewritten unchanged).
const MERGE_GAP: usize = 64 << 10;
const SECTOR_ALIGN: usize = 512;

/// Touched ranges rounded out to 512 bytes (within `limit`), merged when at most MERGE_GAP apart.
fn coalesce(mut ranges: Vec<(usize, usize)>, limit: usize) -> Vec<(usize, usize)> {
    ranges.retain(|r| r.1 > r.0);
    ranges.sort();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (a, b) in ranges {
        let a = a / SECTOR_ALIGN * SECTOR_ALIGN;
        let b = b.div_ceil(SECTOR_ALIGN).saturating_mul(SECTOR_ALIGN).min(limit);
        match out.last_mut() {
            Some(l) if a <= l.1 + MERGE_GAP => l.1 = l.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

impl Touched {
    fn mark(&mut self, dir: usize, a: usize, b: usize) {
        self.0.entry(dir).or_default().push((a, b));
    }
    /// The phase's writes, from the directories' working copies as they are now.
    fn patches(self, v: &Vol) -> Patches {
        let mut p = Patches::default();
        for (d, ranges) in self.0 {
            let dir = &v.dirs[d];
            for (a, b) in coalesce(ranges, dir.data.len()) {
                p.object(v, &dir.clusters, a, &dir.data[a..b]);
            }
        }
        p
    }
}

/// Sets (or clears) the bitmap bits of `runs` in `bm` and adds the changed bytes to `p`.
fn bitmap_patches(v: &Vol, bm: &mut [u8], runs: &[(u64, u64)], set: bool, p: &mut Patches) {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &(s, n) in runs.iter().filter(|r| r.1 > 0) {
        for c in s..s + n {
            let i = (c - 2) as usize;
            if set {
                bm[i / 8] |= 1 << (i % 8);
            } else {
                bm[i / 8] &= !(1 << (i % 8));
            }
        }
        ranges.push((((s - 2) / 8) as usize, ((s - 2 + n - 1) / 8) as usize + 1));
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    for (a, b) in merged {
        p.object(v, &v.bitmap_clusters, a, &bm[a..b]);
    }
}

/// Clusters a bitmap marks used (padding bits past ClusterCount ignored).
fn used_clusters(bm: &[u8], cc: u64) -> u64 {
    let full = (cc / 8) as usize;
    let mut used: u64 = bm[..full].iter().map(|b| b.count_ones() as u64).sum();
    for i in full as u64 * 8..cc {
        used += (bm[(i / 8) as usize] >> (i % 8) & 1) as u64;
    }
    used
}

// ───────────────────────── content comparison ─────────────────────────

/// Bytes per fingerprint sample, and samples per file.
const SAMPLE: u64 = 4096;
const SAMPLES: u64 = 8;
/// Files up to this size are fingerprinted whole: the fingerprint is the full content.
const SMALL: u64 = SAMPLES * SAMPLE;
/// Volume fingerprints are read in batches of this many files (bounded memory).
const FP_BATCH: usize = 1024;

/// The (offset, length) samples of a file of `len` bytes: the whole file when it is at most
/// SMALL, else the first and last SAMPLE bytes and six evenly spaced ones between.
fn sample_ranges(len: u64) -> Vec<(u64, u64)> {
    if len <= SMALL {
        return vec![(0, len)];
    }
    let span = len - SAMPLE;
    let mut v: Vec<(u64, u64)> = (0..SAMPLES).map(|k| (span * k / (SAMPLES - 1), SAMPLE)).collect();
    v.dedup();
    v
}

/// The fingerprint of a local file of `len` bytes (None if it cannot be read as such).
fn local_fingerprint(p: &Path, len: u64) -> Option<u128> {
    let f = File::open(p).ok()?;
    if f.metadata().ok()?.len() != len {
        return None;
    }
    let mut buf = Vec::new();
    for (off, n) in sample_ranges(len) {
        let at = buf.len();
        buf.resize(at + n as usize, 0);
        f.read_exact_at(&mut buf[at..], off).ok()?;
    }
    Some(xxhash_rust::xxh3::xxh3_128(&buf))
}

/// Fingerprints of volume files (as [`local_fingerprint`]; bytes past ValidDataLength read as
/// zeros), read in physical order, FP_BATCH files at a time ordered by where they start.
fn vol_fingerprints(v: &mut Vol, files: &[&Child]) -> Result<Vec<u128>> {
    let mut runs = Vec::with_capacity(files.len());
    for ch in files {
        runs.push(v.runs(ch.first, ch.no_fat_chain, ch.len, "move source")?);
    }
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&k| runs[k].first().map_or(0, |r| r.0));
    let mut out = vec![0u128; files.len()];
    for batch in order.chunks(FP_BATCH) {
        let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(batch.len());
        let mut pieces: Vec<(u64, usize, usize, usize)> = Vec::new(); // (device, buf, at, len)
        for (b, &k) in batch.iter().enumerate() {
            let ch = files[k];
            let ranges = sample_ranges(ch.len);
            bufs.push(vec![0u8; ranges.iter().map(|r| r.1 as usize).sum()]);
            let mut at = 0usize;
            for (off, n) in ranges {
                let (end, mut foff) = ((off + n).min(ch.valid_len), 0u64);
                for &(s, cnt) in &runs[k] {
                    let (a, z) = (foff, foff + cnt * v.cs);
                    let (lo, hi) = (a.max(off), z.min(end));
                    if lo < hi {
                        let dev = v.cluster_off(s as u32) + (lo - a);
                        pieces.push((dev, b, at + (lo - off) as usize, (hi - lo) as usize));
                    }
                    foff = z;
                    if foff >= end {
                        break;
                    }
                }
                at += n as usize;
            }
        }
        pieces.sort();
        for (dev, b, at, n) in pieces {
            v.dev.read_exact_at(&mut bufs[b][at..at + n], dev).with_context(|| format!("read {n} bytes at {dev}"))?;
        }
        for (b, &k) in batch.iter().enumerate() {
            out[k] = xxhash_rust::xxh3::xxh3_128(&bufs[b]);
        }
    }
    Ok(out)
}

/// Hash of a local file as [`crate::verify::hash_vol_files`] computes it; None if it is not
/// `len` bytes long (any more) or cannot be read.
fn local_hash(p: &Path, len: u64) -> Option<u128> {
    let mut buf = vec![0u8; 1 << 20];
    let (n, h) = crate::verify::hash_file(p, &mut buf, &crate::verify::Progress::default()).ok()?;
    (n == len).then_some(h)
}

/// A move bucket: size, and LastModified decoded to UTC (secs, nanos).
type MoveKey = (u64, (i64, u32));

/// The move-bucket key of a source file: its size and its time as flashcp would store it.
fn src_key(n: &Node) -> Option<MoveKey> {
    let (t, t10) = ts(n.mtime.0, n.mtime.1);
    decode_ts(t, t10, 0x80).map(|x| (n.size, x))
}

/// The move-bucket key of a volume file: its size and its stored time, decoded.
fn vol_key(ch: &Child) -> Option<MoveKey> {
    decode_ts(ch.mtime.0, ch.mtime.1, ch.mtime.2).map(|x| (ch.len, x))
}

// ───────────────────────── duplicates ─────────────────────────

/// What the source has under a name, for choosing the keeper of a duplicate.
struct Want<'a> {
    is_dir: bool,
    name: &'a [u16],
    size: u64,
    mtime: Option<(i64, u32)>,
}

/// How well a set matches the source (higher is better; see the module doc).
fn keeper_score(c: &Child, w: Option<&Want<'_>>) -> (bool, bool, bool, bool, usize) {
    let ty = w.is_some_and(|w| w.is_dir == c.is_dir);
    let owns = !(c.is_dir && c.len == 0);
    let meta =
        w.is_some_and(|w| !w.is_dir && !c.is_dir && c.len == w.size && w.mtime.is_some_and(|m| same_mtime(m, c.mtime)));
    let name = w.is_some_and(|w| w.name == &c.name[..]);
    (ty, owns, meta, name, c.set_off)
}

// ───────────────────────── sync ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Obj {
    /// Extension clusters of an existing directory.
    Ext(usize),
    NewDir(usize),
    File(usize),
}

fn json_list(items: impl Iterator<Item = String>) -> String {
    format!("[{}]", items.collect::<Vec<_>>().join(","))
}

/// The metadata phases after `data`, in order (each one journaled, then committed and synced).
const META_PHASES: [&str; 7] = ["alloc", "fat", "unlink", "link", "retire", "free", "clean"];

/// [`sync`] onto a [`TargetSpec`] (a path, or an inherited block-device fd).
pub fn sync_to(src: &Path, target: &TargetSpec, o: &SyncOpts, sink: Sink<'_>) -> Result<SyncStats> {
    let t0 = Instant::now();
    let mut tick = Ticker::new(sink);
    ensure!(
        fs::metadata(src).with_context(|| format!("source {}", src.display()))?.is_dir(),
        "source must be a directory"
    );
    let tgt = target.open_rw()?;
    set_direct(&tgt.file, false);
    // An interrupted run first: its journal is replayed before anything is planned.
    if let Some(n) = recovery_note(&journal::recover(&tgt.file)?) {
        eprintln!("{n}");
    }
    {
        let mut s0 = [0u8; 512];
        tgt.file.read_exact_at(&mut s0, 0).context("read boot sector")?;
        ensure!(
            &s0[3..11] != b"EXFAT   " || u16::from_le_bytes([s0[106], s0[107]]) & FLAG_DIRTY == 0,
            "volume is marked dirty and there is no sync journal for it here (still mounted, not cleanly \
             unmounted, or a sync interrupted on another machine): unmount it, or run fsck.exfat -y, then re-run \
             the sync"
        );
    }
    // Duplicates in the root (an interrupted sync) make the volume non-empty: skip the build.
    let root_dups = Vol::open_with(tgt.file.try_clone().context("dup target fd")?, tgt.bytes, false, false, true)
        .map(|p| !p.dirs[0].dups.is_empty())?;
    // An empty volume with coarse clusters (a fresh stick) is written as a build, as by add.
    let tgt = if root_dups {
        tgt
    } else {
        let inner = tick.sink();
        let mut fwd = |e: &Event| match e {
            Event::Plan { files, dirs, bytes, write_bytes, detail, strategy, .. } => {
                let s = format!(
                    r#"{{"unchanged":0,"new":{files},"changed":0,"moved":0,"deleted":0,"deleted_dirs":0,"new_dirs":{dirs},"bytes_skipped":0,"delete":[],"moves":[]}}"#
                );
                inner(&Event::Plan {
                    op: Op::Sync,
                    files: *files,
                    dirs: *dirs,
                    bytes: *bytes,
                    write_bytes: *write_bytes,
                    detail: detail.clone(),
                    strategy: strategy.clone(),
                    sync: Some(s),
                })
            }
            e => inner(e),
        };
        match crate::add_build::empty_as_build(src, tgt, o.to.as_deref(), o.readers, o.dry_run, &mut fwd)? {
            Ok(s) => return Ok(from_build(s)),
            Err(tgt) => tgt,
        }
    };
    let excl = Arc::new(tgt.file);
    set_direct(&excl, false);
    let mut v = Vol::open_with(excl.try_clone().context("dup target fd")?, tgt.bytes, false, true, true)?;
    let cs = v.cs;
    let serial = le32(&v.sector0, 100);
    let mut nodes = scan_tree(src, &v.upc)?;

    // ── verification walk, keeping every directory on the way to and under the destination ──
    let comps: Vec<&str> = o.to.as_deref().unwrap_or("").split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let mut dest: Vec<Vec<u16>> = Vec::new();
    for comp in &comps {
        ensure!(*comp != "..", "--to must not contain '..'");
        let name: Vec<u16> = comp.encode_utf16().collect();
        validate_name(&name, &v.upc, &mut HashSet::new(), Path::new(comp))?;
        dest.push(upcase(&name, &v.upc));
    }
    let keep = |k: &[Vec<u16>]| if k.len() <= dest.len() { dest.starts_with(k) } else { k.starts_with(&dest) };
    let tv = Instant::now();
    let verified = v.verify_allocation_with(&keep)?;
    let verify_secs = tv.elapsed().as_secs_f64();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let now = (now.as_secs() as i64, now.subsec_nanos());
    let mut st = SyncStats { cluster: cs, ..Default::default() };

    // ── duplicates an interrupted sync left: keep the best match of the source, unlink the rest ──
    let mut src_dir_of: HashMap<Vec<Vec<u16>>, usize> = HashMap::new();
    {
        let mut stack = vec![(0usize, dest.clone())];
        while let Some((n, key)) = stack.pop() {
            for &c in &nodes[n].children {
                if nodes[c].is_dir {
                    let mut k = key.clone();
                    k.push(upcase(&nodes[c].name, &v.upc));
                    stack.push((c, k));
                }
            }
            src_dir_of.insert(key, n);
        }
    }
    let comp_names: Vec<Vec<u16>> = comps.iter().map(|c| c.encode_utf16().collect()).collect();
    let mut repairs: Vec<(usize, Child)> = Vec::new();
    let mut freed: Vec<(u64, u64)> = Vec::new();
    for d in 0..v.dirs.len() {
        if v.dirs[d].dups.is_empty() {
            continue;
        }
        let dups = std::mem::take(&mut v.dirs[d].dups);
        let dkey = v.dirs[d].key.clone();
        let src_kids: HashMap<Vec<u16>, usize> = src_dir_of
            .get(&dkey)
            .map(|&n| nodes[n].children.iter().map(|&c| (upcase(&nodes[c].name, &v.upc), c)).collect())
            .unwrap_or_default();
        let mut groups: BTreeMap<Vec<u16>, Vec<Child>> = BTreeMap::new();
        for ch in dups {
            groups.entry(upcase(&ch.name, &v.upc)).or_default().push(ch);
        }
        for (key, mut members) in groups {
            members.push(v.dirs[d].children[&key].clone());
            let want = if let Some(&c) = src_kids.get(&key) {
                let n = &nodes[c];
                Some(Want { is_dir: n.is_dir, name: &n.name, size: n.size, mtime: Some(n.mtime) })
            } else if dkey.len() < dest.len() && dest.starts_with(&dkey) && dest[dkey.len()] == key {
                Some(Want { is_dir: true, name: &comp_names[dkey.len()], size: 0, mtime: None })
            } else {
                None
            };
            members.sort_by_key(|c| keeper_score(c, want.as_ref()));
            let keeper = members.pop().unwrap();
            if keeper.is_dir {
                let mut ck = dkey.clone();
                ck.push(key.clone());
                if let Some(&ki) = v.kept.get(&ck) {
                    v.dirs[ki].entry = Some((d, keeper.set_off, keeper.set_len));
                }
            }
            for l in members {
                if !l.is_dir && l.first != 0 && l.first != keeper.first {
                    freed.extend(v.runs(l.first, l.no_fat_chain, l.len, "duplicate entry")?);
                }
                repairs.push((d, l));
            }
            v.dirs[d].children.insert(key, keeper);
        }
    }
    st.repaired = repairs.len() as u64;

    // ── destination (--to), creating missing parents as new directories ──
    let mut appends: BTreeMap<usize, Vec<Item>> = BTreeMap::new();
    let mut cur = 0usize;
    let mut new_parent: Option<usize> = None;
    for comp in &comps {
        let name: Vec<u16> = comp.encode_utf16().collect();
        let synth = |nodes: &mut Vec<Node>| {
            nodes.push(Node {
                name: name.clone(),
                src: PathBuf::new(),
                is_dir: true,
                size: 0,
                mtime: now,
                children: vec![],
                first_cluster: 0,
                clusters: 0,
            });
            nodes.len() - 1
        };
        if let Some(np) = new_parent {
            let k = synth(&mut nodes);
            nodes[np].children.push(k);
            new_parent = Some(k);
            continue;
        }
        match v.dirs[cur].children.get(&upcase(&name, &v.upc)).cloned() {
            Some(ch) if ch.is_dir => cur = v.load_child(cur, &ch)?,
            Some(_) => bail!("--to: {}/{comp} exists on the volume and is not a directory", v.dirs[cur].path),
            None => {
                let k = synth(&mut nodes);
                appends.entry(cur).or_default().push(Item::Node(k));
                new_parent = Some(k);
            }
        }
    }

    // ── match the source tree against the destination ──
    let mut edits: BTreeMap<(usize, usize), Edit> = BTreeMap::new();
    let mut pairs: Vec<(usize, Child, usize)> = Vec::new(); // (dir, volume file, source file)
    let mut deletes: Vec<(usize, Child)> = Vec::new(); // top-level entries that go away
    let mut conflicts: Vec<String> = Vec::new();
    let mut extra_kept = 0u64;
    let top = nodes[0].children.clone();
    if let Some(np) = new_parent {
        nodes[np].children = top;
    } else {
        let mut work = vec![(cur, top)];
        while let Some((d, kids)) = work.pop() {
            let mut seen: HashSet<Vec<u16>> = HashSet::new();
            for c in kids {
                let key = upcase(&nodes[c].name, &v.upc);
                seen.insert(key.clone());
                let Some(ch) = v.dirs[d].children.get(&key).cloned() else {
                    appends.entry(d).or_default().push(Item::Node(c));
                    continue;
                };
                let renamed = ch.name != nodes[c].name;
                match (ch.is_dir, nodes[c].is_dir) {
                    (true, true) => {
                        let nd = v.load_child(d, &ch)?;
                        let retime = !same_mtime(nodes[c].mtime, ch.mtime);
                        if retime || renamed {
                            edits.insert(
                                (d, ch.set_off),
                                Edit {
                                    len: ch.set_len,
                                    node: Some(c),
                                    mtime: retime,
                                    name: renamed,
                                    ..Default::default()
                                },
                            );
                        }
                        work.push((nd, nodes[c].children.clone()));
                    }
                    (false, false) => pairs.push((d, ch, c)),
                    _ if o.delete => {
                        deletes.push((d, ch));
                        appends.entry(d).or_default().push(Item::Node(c));
                    }
                    _ => conflicts.push(format!("{}/{}", v.dirs[d].path, String::from_utf16_lossy(&ch.name))),
                }
            }
            let extra: Vec<Child> =
                v.dirs[d].children.iter().filter(|(k, _)| !seen.contains(*k)).map(|e| e.1.clone()).collect();
            if o.delete {
                deletes.extend(extra.into_iter().map(|ch| (d, ch)));
            } else {
                extra_kept += extra.len() as u64;
            }
        }
    }
    if !conflicts.is_empty() {
        conflicts.sort();
        let n = conflicts.len();
        let list: Vec<_> = conflicts.into_iter().take(10).collect();
        bail!(
            "{n} name(s) are a file on one side and a directory on the other, e.g. {}; pass --delete to replace them",
            list.join(", ")
        );
    }
    deletes.sort_by(|a, b| (a.0, &a.1.name).cmp(&(b.0, &b.1.name)));

    // ── what is new: directories (BFS) and files, with their volume paths ──
    let mut path_of: HashMap<usize, String> = HashMap::new();
    let mut new_dirs = Vec::new();
    let mut new_files = Vec::new();
    for (&d, kids) in &appends {
        for &it in kids {
            let Item::Node(c) = it else { continue };
            path_of.insert(c, format!("{}/{}", v.dirs[d].path, String::from_utf16_lossy(&nodes[c].name)));
            if nodes[c].is_dir { new_dirs.push(c) } else { new_files.push(c) }
        }
    }
    let mut q = 0;
    while q < new_dirs.len() {
        let d = new_dirs[q];
        let base = path_of[&d].clone();
        for &c in &nodes[d].children {
            path_of.insert(c, format!("{base}/{}", String::from_utf16_lossy(&nodes[c].name)));
            if nodes[c].is_dir { new_dirs.push(c) } else { new_files.push(c) }
        }
        q += 1;
    }

    // ── everything that goes away (deep), and the move pool ──
    let mut gone_files: Vec<Gone> = Vec::new();
    let mut gone_dirs: Vec<usize> = Vec::new(); // indices into v.dirs
    let mut delete_list: Vec<(String, bool, std::ops::Range<usize>)> = Vec::new(); // (path, dir, files)
    for (d, ch) in &deletes {
        let path = format!("{}/{}", v.dirs[*d].path, String::from_utf16_lossy(&ch.name));
        let f0 = gone_files.len();
        if ch.is_dir {
            let mut stack = vec![v.load_child(*d, ch)?];
            while let Some(di) = stack.pop() {
                gone_dirs.push(di);
                let mut kids: Vec<&Child> = v.dirs[di].children.values().collect();
                kids.sort_by(|a, b| a.name.cmp(&b.name));
                for k in kids {
                    if k.is_dir {
                        stack.push(v.load_child(di, k)?);
                    } else {
                        let p = format!("{}/{}", v.dirs[di].path, String::from_utf16_lossy(&k.name));
                        gone_files.push(Gone { path: p, ch: k.clone() });
                    }
                }
            }
        } else {
            gone_files.push(Gone { path: path.clone(), ch: ch.clone() });
        }
        delete_list.push((path, ch.is_dir, f0..gone_files.len()));
    }

    // ── move candidates: same size and same decoded time, by bucket ──
    let mut pool_by: HashMap<MoveKey, Vec<usize>> = HashMap::new();
    for (i, g) in gone_files.iter().enumerate() {
        if g.ch.len > 0
            && let Some(k) = vol_key(&g.ch)
        {
            pool_by.entry(k).or_default().push(i);
        }
    }
    let candidates: Vec<(usize, MoveKey)> = new_files
        .iter()
        .filter(|&&f| nodes[f].size > 0)
        .filter_map(|&f| src_key(&nodes[f]).filter(|k| pool_by.contains_key(k)).map(|k| (f, k)))
        .collect();
    let live_keys: HashSet<MoveKey> = candidates.iter().map(|c| c.1).collect();
    let pool_used: BTreeSet<usize> = live_keys.iter().flat_map(|k| pool_by[k].iter().copied()).collect();

    // --checksum: hash every same-size matched file and every pool file with a candidate.
    let mut vol_hash: HashMap<(usize, usize), u128> = HashMap::new(); // (kind 0 pair / 1 pool, index)
    let mut src_hash: HashMap<usize, Option<u128>> = HashMap::new();
    if o.checksum {
        let mut want: Vec<(usize, usize)> = Vec::new();
        let mut objs: Vec<(u32, bool, u64, u64)> = Vec::new();
        for (i, (_, ch, c)) in pairs.iter().enumerate() {
            if ch.len == nodes[*c].size && ch.len > 0 {
                want.push((0, i));
                objs.push((ch.first, ch.no_fat_chain, ch.len, ch.valid_len));
                src_hash.insert(*c, None);
            }
        }
        for &i in &pool_used {
            let ch = &gone_files[i].ch;
            want.push((1, i));
            objs.push((ch.first, ch.no_fat_chain, ch.len, ch.valid_len));
        }
        for (f, _) in &candidates {
            src_hash.insert(*f, None);
        }
        let hs = crate::verify::hash_vol_files(&mut v, &objs)?;
        vol_hash = want.into_iter().zip(hs).collect();
        for (&n, h) in src_hash.iter_mut() {
            *h = local_hash(&nodes[n].src, nodes[n].size);
        }
    }

    let mut alloc_files: Vec<usize> = Vec::new(); // new and changed files that get new clusters
    for (i, (d, ch, c)) in pairs.iter().enumerate() {
        let (d, c) = (*d, *c);
        let n = &nodes[c];
        let renamed = ch.name != n.name;
        let same_time = same_mtime(n.mtime, ch.mtime);
        let same_content = if o.checksum {
            n.size == ch.len && (n.size == 0 || src_hash[&c].is_some_and(|h| vol_hash[&(0, i)] == h))
        } else {
            n.size == ch.len && same_time
        };
        let base = Edit { len: ch.set_len, node: Some(c), name: renamed, ..Default::default() };
        if same_content && same_time {
            if renamed {
                st.moved += 1;
                edits.insert((d, ch.set_off), base);
            } else {
                st.unchanged += 1;
            }
            st.data_skipped += n.size;
        } else if same_content {
            st.changed += 1; // metadata only
            edits.insert((d, ch.set_off), Edit { mtime: true, ..base });
        } else {
            st.changed += 1;
            st.data_written += n.size;
            edits.insert((d, ch.set_off), Edit { repoint: true, mtime: true, ..base });
            if n.size > 0 {
                alloc_files.push(c);
            }
            freed.extend(v.runs(ch.first, ch.no_fat_chain, ch.len, "changed file")?);
        }
    }

    // ── moves: equal fingerprints (or hashes), then confirmed; each pool file at most once ──
    let mut moves: HashMap<usize, usize> = HashMap::new(); // new file node → gone_files index
    let mut used: HashSet<usize> = HashSet::new();
    if o.checksum {
        // Consume-once queues by (bucket, full hash), as for fingerprints: no per-candidate scan.
        let mut by_hash: HashMap<(MoveKey, u128), VecDeque<usize>> = HashMap::new();
        for &i in &pool_used {
            by_hash.entry((vol_key(&gone_files[i].ch).unwrap(), vol_hash[&(1, i)])).or_default().push_back(i);
        }
        for (f, k) in &candidates {
            let Some(h) = src_hash[f] else { continue };
            if let Some(i) = by_hash.get_mut(&(*k, h)).and_then(|q| q.pop_front()) {
                used.insert(i);
                moves.insert(*f, i);
                st.moves_verified += 1;
            }
        }
    } else if !candidates.is_empty() {
        let pool: Vec<usize> = pool_used.iter().copied().collect();
        let refs: Vec<&Child> = pool.iter().map(|&i| &gone_files[i].ch).collect();
        let fps = vol_fingerprints(&mut v, &refs)?;
        let mut by_fp: HashMap<(MoveKey, u128), VecDeque<usize>> = HashMap::new();
        for (k, &i) in pool.iter().enumerate() {
            by_fp.entry((vol_key(&gone_files[i].ch).unwrap(), fps[k])).or_default().push_back(i);
        }
        let pays = verify_pays(None, None);
        let mut budget = 0u64;
        let mut check: Vec<(usize, usize)> = Vec::new();
        for (f, k) in &candidates {
            let size = nodes[*f].size;
            let Some(fp) = local_fingerprint(&nodes[*f].src, size) else { continue };
            let Some(i) = by_fp.get_mut(&(*k, fp)).and_then(|q| q.pop_front()) else { continue };
            if size <= SMALL {
                st.moves_verified += 1; // the fingerprint is the whole content
            } else if o.fast_moves {
                st.moves_fast += 1;
            } else if pays && budget + size <= VERIFY_BUDGET {
                budget += size;
                check.push((*f, i));
                continue;
            } else {
                st.moves_copied_budget += 1;
                continue;
            }
            used.insert(i);
            moves.insert(*f, i);
        }
        if !check.is_empty() {
            let objs: Vec<(u32, bool, u64, u64)> = check
                .iter()
                .map(|&(_, i)| {
                    let ch = &gone_files[i].ch;
                    (ch.first, ch.no_fat_chain, ch.len, ch.valid_len)
                })
                .collect();
            let hs = crate::verify::hash_vol_files(&mut v, &objs)?;
            for (&(f, i), h) in check.iter().zip(hs) {
                if local_hash(&nodes[f].src, nodes[f].size) == Some(h) {
                    st.moves_verified += 1;
                    used.insert(i);
                    moves.insert(f, i);
                }
            }
        }
    }
    for &f in &new_files {
        if moves.contains_key(&f) {
            st.moved += 1;
            st.data_skipped += nodes[f].size;
        } else {
            st.new += 1;
            st.data_written += nodes[f].size;
            if nodes[f].size > 0 {
                alloc_files.push(f);
            }
        }
    }
    st.new_dirs = new_dirs.len() as u64;
    st.deleted = (gone_files.len() - used.len()) as u64;
    st.deleted_dirs = gone_dirs.len() as u64;
    for (i, g) in gone_files.iter().enumerate() {
        if !used.contains(&i) {
            freed.extend(v.runs(g.ch.first, g.ch.no_fat_chain, g.ch.len, &g.path)?);
        }
    }
    let gone_set: HashSet<usize> = gone_dirs.iter().copied().collect();
    for &di in &gone_dirs {
        let r: Vec<(u64, u64)> = v.dirs[di].clusters.iter().map(|&c| (c as u64, 1)).collect();
        freed.extend(r);
        // Duplicates inside a deleted directory with clusters of their own go too.
        for l in std::mem::take(&mut v.dirs[di].dups) {
            let twin = v.dirs[di].children[&upcase(&l.name, &v.upc)].first;
            if !l.is_dir && l.first != 0 && l.first != twin {
                freed.extend(v.runs(l.first, l.no_fat_chain, l.len, "duplicate entry")?);
            }
        }
    }
    // Every freed cluster is in use before the sync, and freed once.
    let mut seen_free = cluster_set(v.cc);
    for &(s, n) in &freed {
        mark_range(&mut seen_free, s as u32, n)
            .map_err(|c| anyhow::anyhow!("internal error: cluster {c} would be freed twice"))?;
        ensure!(
            (s..s + n).all(|c| v.bitmap[((c - 2) / 8) as usize] >> ((c - 2) % 8) & 1 == 1),
            "internal error: freeing a cluster that is already free"
        );
    }
    // Orphan secondaries of earlier interrupted runs, in the directories that stay.
    let orphan_dirs: Vec<usize> =
        (0..v.dirs.len()).filter(|d| !gone_set.contains(d) && !v.dirs[*d].orphans.is_empty()).collect();
    st.orphans = orphan_dirs.iter().map(|&d| v.dirs[d].orphans.len() as u64).sum();

    // ── in place or relocated, and directory growth: deepest directories first, so that a
    //    directory's own set edit (its new length) is known before its parent is sized ──
    let item_bytes = |nodes: &[Node], edits: &BTreeMap<(usize, usize), Edit>, d: usize, it: Item| match it {
        Item::Node(c) => set_bytes(&nodes[c]),
        Item::Reloc(off) => edits[&(d, off)].len as u64,
    };
    let mut depth: Vec<usize> = (0..v.dirs.len()).filter(|d| !gone_set.contains(d)).collect();
    depth.sort_by_key(|&d| std::cmp::Reverse(v.dirs[d].key.len()));
    let mut relocs: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut ext_need: BTreeMap<usize, u64> = BTreeMap::new();
    for &d in &depth {
        let here: Vec<usize> = edits.range((d, 0)..(d + 1, 0)).map(|(k, _)| k.1).collect();
        for off in here {
            let e = &edits[&(d, off)];
            let name = e.node.map(|c| nodes[c].name.as_slice());
            let hi = changed_end(e, &v.dirs[d].data[off..off + e.len], name);
            if !one_sector(off, hi) {
                relocs.insert((d, off));
                appends.entry(d).or_default().push(Item::Reloc(off));
            }
        }
        let Some(kids) = appends.get(&d) else { continue };
        let need: u64 = kids.iter().map(|&it| item_bytes(&nodes, &edits, d, it)).sum();
        let dir = &v.dirs[d];
        let free = (dir.data.len() - dir.end) as u64;
        if need > free {
            let n = (need - free).div_ceil(cs);
            ensure!(
                dir.data.len() as u64 + n * cs <= MAX_DIR_BYTES,
                "directory {} would grow past the 256 MB exFAT limit",
                if dir.path.is_empty() { "/" } else { &dir.path }
            );
            ext_need.insert(d, n);
            if let Some((p, off, len)) = dir.entry {
                let e = edits.entry((p, off)).or_insert_with(|| Edit { len, ..Default::default() });
                e.dirlen = Some(dir.data.len() as u64 + n * cs);
            }
        }
    }
    st.relocated = relocs.len() as u64;
    for &d in &new_dirs {
        let bytes: u64 = nodes[d].children.iter().map(|&c| set_bytes(&nodes[c])).sum();
        nodes[d].clusters = bytes.div_ceil(cs).max(1);
        ensure!(nodes[d].clusters * cs <= MAX_DIR_BYTES, "a source directory has too many entries for exFAT");
    }
    for &f in &alloc_files {
        nodes[f].clusters = nodes[f].size.div_ceil(cs);
    }

    // ── allocation, from the pre-sync free space only ──
    let mut free = free_extents(&v.bitmap, v.cc);
    let free_total: u64 = free.iter().map(|e| e.1).sum();
    let mut runs: BTreeMap<Obj, Vec<(u64, u64)>> = BTreeMap::new();
    for (&d, &n) in &ext_need {
        let dir = &v.dirs[d];
        if dir.no_fat_chain {
            let next = *dir.clusters.last().unwrap() as u64 + 1;
            if take_exact(&mut free, next, n) {
                runs.insert(Obj::Ext(d), vec![(next, n)]);
            }
        }
    }
    let mut order: Vec<(Obj, u64)> = Vec::new();
    for (&d, &n) in &ext_need {
        if !runs.contains_key(&Obj::Ext(d)) {
            order.push((Obj::Ext(d), n));
        }
    }
    order.extend(new_dirs.iter().map(|&d| (Obj::NewDir(d), nodes[d].clusters)));
    order.extend(alloc_files.iter().map(|&f| (Obj::File(f), nodes[f].clusters)));
    let total: u64 = order.iter().map(|e| e.1).sum();
    let Some(chosen) = choose_extents(&free, total) else {
        let need: u64 = total + runs.values().flatten().map(|r| r.1).sum::<u64>();
        bail!(
            "not enough free space: need {} ({need} clusters), the volume has {} free (space this sync frees \
             is not reused by it)",
            human_bytes((need * cs) as f64),
            human_bytes((free_total * cs) as f64)
        );
    };
    let mut ext_q: VecDeque<_> = chosen.into();
    for &(obj, n) in &order {
        runs.insert(obj, carve(&mut ext_q, n));
    }
    let frag = |obj: Obj| runs.get(&obj).is_some_and(|r| r.len() > 1);
    for (&obj, r) in &runs {
        if let Obj::NewDir(i) | Obj::File(i) = obj {
            nodes[i].first_cluster = r[0].0 as u32;
        }
    }
    // A directory extension that is not the next clusters of a NoFatChain run needs a chain.
    let contiguous_ext: HashSet<usize> = ext_need
        .keys()
        .copied()
        .filter(|&d| {
            let dir = &v.dirs[d];
            let r = &runs[&Obj::Ext(d)];
            dir.no_fat_chain && r.len() == 1 && r[0].0 == *dir.clusters.last().unwrap() as u64 + 1
        })
        .collect();
    for &d in ext_need.keys() {
        if let (false, Some((p, off, _))) = (contiguous_ext.contains(&d), v.dirs[d].entry) {
            edits.get_mut(&(p, off)).unwrap().clear_nfc = true;
        }
    }
    let upc = v.upc.clone();
    let (heap, bps) = (v.heap_off, v.bps);
    let coff = move |c: u64| heap + (c - 2) * cs;
    let set_for = |nodes: &[Node], i: usize| {
        let n = &nodes[i];
        if let Some(&g) = moves.get(&i) {
            // A move: the new name, parent and time over the old clusters.
            let old = &gone_files[g].ch;
            let mut e = entry_set(&Node { first_cluster: old.first, ..clone_node(n) }, &upc);
            e[33] = if old.no_fat_chain { 0x03 } else { 0x01 };
            return finish_set(e, old.valid_len, old.len);
        }
        let mut e = entry_set(n, &upc);
        let obj = if n.is_dir { Obj::NewDir(i) } else { Obj::File(i) };
        if frag(obj) {
            e[33] = 0x01; // AllocationPossible, FAT chain
        }
        let len = if n.is_dir { n.clusters * cs } else { n.size };
        finish_set(e, len, len)
    };
    // An existing set with its edit applied.
    let edited = |nodes: &[Node], old: &[u8], e: &Edit| {
        let mut set = old.to_vec();
        let n = e.node.map(|c| &nodes[c]);
        let repoint = n.filter(|_| e.repoint).map(|n| Repoint {
            first: n.first_cluster,
            contiguous: !frag(Obj::File(e.node.unwrap())),
            len: n.size,
        });
        patch_set(&mut set, repoint, n.filter(|_| e.mtime).map(|n| n.mtime), n.filter(|_| e.name).map(|n| &n.name[..]));
        if let Some(l) = e.dirlen {
            if e.clear_nfc {
                set[33] &= !0x02;
            }
            set[40..48].copy_from_slice(&l.to_le_bytes());
            set[56..64].copy_from_slice(&l.to_le_bytes());
            fix_checksum(&mut set);
        }
        set
    };

    // ── phase-1 content: new directories, extension clusters ──
    let mut mems: Vec<Vec<u8>> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    let push_mem = |segs: &mut Vec<Seg>, mems: &mut Vec<Vec<u8>>, r: &[(u64, u64)], mut bytes: Vec<u8>| {
        let total: u64 = r.iter().map(|x| x.1).sum::<u64>() * cs;
        bytes.resize(total as usize, 0);
        let m = mems.len();
        mems.push(bytes);
        let mut off = 0usize;
        for &(s, n) in r {
            segs.push(Seg { dev: coff(s), len: n * cs, kind: SegKind::Mem(m, off) });
            off += (n * cs) as usize;
        }
    };
    for &d in &new_dirs {
        let mut bytes = Vec::new();
        for &c in &nodes[d].children {
            bytes.extend(set_for(&nodes, c));
        }
        push_mem(&mut segs, &mut mems, &runs[&Obj::NewDir(d)], bytes);
    }

    // ── unlink: primaries only ──
    let dead: Vec<(usize, usize, usize)> =
        deletes.iter().chain(&repairs).map(|(d, ch)| (*d, ch.set_off, ch.set_len)).collect();
    let mut t_unlink = Touched::default();
    for &(d, off, _) in &dead {
        v.dirs[d].data[off] &= 0x7F;
        t_unlink.mark(d, off, off + 1);
    }
    let p_unlink = t_unlink.patches(&v);

    // ── link: appends (new, moved, relocated), extended directories, in-place edits ──
    let torn_test = std::env::var(TORN_ENV).as_deref() == Ok("relocate");
    let pre_link: HashMap<usize, Vec<u8>> =
        if torn_test { relocs.iter().map(|&(d, _)| (d, v.dirs[d].data.clone())).collect() } else { HashMap::new() };
    let mut new_sets: Vec<(usize, usize, usize)> = Vec::new(); // relocated sets at their new place
    let mut t_link = Touched::default();
    let mut ext_fat: Vec<(u32, u32)> = Vec::new();
    for (&d, kids) in &appends {
        let (end, dlen) = (v.dirs[d].end, v.dirs[d].data.len());
        let mut bytes = Vec::new();
        for &it in kids {
            match it {
                Item::Node(c) => bytes.extend(set_for(&nodes, c)),
                Item::Reloc(off) => {
                    let e = &edits[&(d, off)];
                    new_sets.push((d, end + bytes.len(), e.len));
                    bytes.extend(edited(&nodes, &v.dirs[d].data[off..off + e.len], e));
                }
            }
        }
        let tail = bytes.len().min(dlen - end);
        v.dirs[d].data[end..end + tail].copy_from_slice(&bytes[..tail]);
        let after = end + tail;
        if v.dirs[d].data[after..].iter().any(|&b| b != 0) {
            // Everything after the end-of-directory marker must read as "end".
            v.dirs[d].data[after..].fill(0);
            t_link.mark(d, after, dlen);
        }
        t_link.mark(d, end, after);
        let Some(r) = runs.get(&Obj::Ext(d)).cloned() else { continue };
        push_mem(&mut segs, &mut mems, &r, bytes[tail..].to_vec());
        let dir = &v.dirs[d];
        if !contiguous_ext.contains(&d) {
            // Existing chain → extension. A NoFatChain directory needs its chain written first.
            let last = *dir.clusters.last().unwrap();
            let mut all = if dir.no_fat_chain { dir.clusters.clone() } else { vec![last] };
            all.extend(run_clusters(&r));
            chain_entries(&all, &mut ext_fat);
        }
    }
    for (&(d, off), e) in &edits {
        if relocs.contains(&(d, off)) {
            continue;
        }
        let new = edited(&nodes, &v.dirs[d].data[off..off + e.len], e);
        v.dirs[d].data[off..off + e.len].copy_from_slice(&new);
        t_link.mark(d, off, off + e.len);
    }
    let p_link = t_link.patches(&v);
    let mut p_fat = Patches::default();
    p_fat.fat(&v, &ext_fat);

    // ── retire: the relocated sets' old copies ──
    let mut t_retire = Touched::default();
    for &(d, off) in &relocs {
        v.dirs[d].data[off] &= 0x7F;
        t_retire.mark(d, off, off + 1);
    }
    let p_retire = t_retire.patches(&v);

    // ── alloc, and free: secondaries of every set that died, old orphans, bitmap bits ──
    let mut bm = v.bitmap.clone();
    let mut p_alloc = Patches::default();
    let all_runs: Vec<(u64, u64)> = runs.values().flatten().copied().collect();
    bitmap_patches(&v, &mut bm, &all_runs, true, &mut p_alloc);
    let mut new_fat: Vec<(u32, u32)> = Vec::new();
    for (&obj, r) in &runs {
        if !matches!(obj, Obj::Ext(_)) && r.len() > 1 {
            chain_entries(&run_clusters(r), &mut new_fat);
        }
    }
    p_alloc.fat(&v, &new_fat);
    let mut t_free = Touched::default();
    let retired: Vec<(usize, usize, usize)> = relocs.iter().map(|&(d, off)| (d, off, edits[&(d, off)].len)).collect();
    for &(d, off, len) in dead.iter().chain(&retired) {
        for k in 1..len / 32 {
            v.dirs[d].data[off + 32 * k] &= 0x7F;
        }
        t_free.mark(d, off + 32, off + len);
    }
    for &d in &orphan_dirs {
        for o in std::mem::take(&mut v.dirs[d].orphans) {
            v.dirs[d].data[o] &= 0x7F;
            t_free.mark(d, o, o + 1);
        }
    }
    let mut p_free = t_free.patches(&v);
    bitmap_patches(&v, &mut bm, &freed, false, &mut p_free);
    // clean: VolumeDirty cleared, PercentInUse of the final bitmap.
    let pct = (used_clusters(&bm, v.cc) * 100 / v.cc).min(100) as u8;
    let orig_s0 = v.sector0.clone();
    let mut dirty_s0 = orig_s0.clone();
    dirty_s0[106..108].copy_from_slice(&working_flags(v.flags).to_le_bytes());
    let mut clean_s0 = orig_s0.clone();
    clean_s0[106..108].copy_from_slice(&clean_flags(v.flags).to_le_bytes());
    clean_s0[112] = pct;
    let mut p_clean = Patches::default();
    p_clean.add(0, clean_s0);

    // ── plan ──
    segs.sort_by_key(|s| s.dev);
    let file_order: Vec<usize> = alloc_files.clone();
    let pos_of: HashMap<usize, usize> = file_order.iter().enumerate().map(|(k, &f)| (f, k)).collect();
    for &f in &file_order {
        let mut off = 0u64;
        for &(s, n) in &runs[&Obj::File(f)] {
            segs.push(Seg { dev: v.cluster_off(s as u32), len: n * cs, kind: SegKind::File(pos_of[&f], off) });
            off += n * cs;
        }
    }
    segs.sort_by_key(|s| s.dev);
    let sizes: Vec<u64> = file_order.iter().map(|&f| nodes[f].size).collect();
    let data_write = data_write_bytes(&segs, &sizes);
    let nothing = appends.is_empty() && edits.is_empty() && deletes.is_empty() && repairs.is_empty() && st.orphans == 0;
    let meta: Vec<(&str, Patches)> = vec![
        ("alloc", p_alloc),
        ("fat", p_fat),
        ("unlink", p_unlink),
        ("link", p_link),
        ("retire", p_retire),
        ("free", p_free),
        ("clean", p_clean),
    ];
    let write_bytes = if nothing { 0 } else { data_write + meta.iter().map(|(_, p)| p.bytes()).sum::<u64>() + bps };
    let direct = tgt.direct && cs.is_multiple_of(4096) && v.heap_off.is_multiple_of(4096);
    let pol = data_gap_policy(&excl, direct, &v, tgt.part_off * 512, o.dry_run, &segs, &sizes);

    let (files, dirs, bytes) = (st.new + st.changed, st.new_dirs, st.data_written);
    let mut moves_list: Vec<(String, String, usize)> =
        moves.iter().map(|(&f, &g)| (gone_files[g].path.clone(), path_of[&f].clone(), g)).collect();
    moves_list.sort();
    let shown = |p: &str, dir: bool| if dir { format!("{p}/") } else { p.to_string() };
    let sync_json = format!(
        r#"{{"unchanged":{},"new":{},"changed":{},"moved":{},"deleted":{},"deleted_dirs":{},"new_dirs":{},"bytes_skipped":{},"relocated":{},"repaired":{},"orphans":{},"moves_verified":{},"moves_fast":{},"moves_copied_budget":{},"delete":{},"moves":{}}}"#,
        st.unchanged,
        st.new,
        st.changed,
        st.moved,
        st.deleted,
        st.deleted_dirs,
        st.new_dirs,
        st.data_skipped,
        st.relocated,
        st.repaired,
        st.orphans,
        st.moves_verified,
        st.moves_fast,
        st.moves_copied_budget,
        json_list(
            delete_list
                .iter()
                .filter(|(_, d, r)| *d || !used.contains(&r.start))
                .map(|(p, d, _)| json_str(&shown(p, *d)))
        ),
        json_list(moves_list.iter().map(|(a, b, _)| format!(r#"{{"from":{},"to":{}}}"#, json_str(a), json_str(b))))
    );
    let mut detail = format!(
        "plan: sync {} unchanged, {} new, {} changed, {} moved, {} deleted ({} dirs), {} new dirs │ {} of file data \
         to write, {} unchanged │ cluster {} │ {} free │ will write {} │ {} │ verified {verified} objects in {:.0} ms",
        st.unchanged,
        st.new,
        st.changed,
        st.moved,
        st.deleted,
        st.deleted_dirs,
        st.new_dirs,
        human_bytes(st.data_written as f64),
        human_bytes(st.data_skipped as f64),
        human_bytes(cs as f64),
        human_bytes((free_total * cs) as f64),
        human_bytes(write_bytes as f64),
        pol.describe(),
        verify_secs * 1e3
    );
    if !candidates.is_empty() {
        detail += &format!(" │ {} moves verified, {} copied (budget)", st.moves_verified, st.moves_copied_budget);
        if st.moves_fast > 0 {
            detail += &format!(", {} on fingerprints only (--fast-moves)", st.moves_fast);
        }
    }
    if st.repaired > 0 || st.orphans > 0 {
        detail += &format!(
            " │ repaired {} interrupted entries ({} duplicate sets, {} orphan secondaries)",
            st.repaired + st.orphans,
            st.repaired,
            st.orphans
        );
    }
    if extra_kept > 0 {
        detail += &format!(" │ {extra_kept} extra entries kept (pass --delete to remove them)");
    }
    for (p, d, r) in &delete_list {
        if *d {
            let n = r.clone().filter(|i| !used.contains(i)).count();
            detail += &format!("\ndelete: {} ({n} files)", shown(p, true));
        } else if !used.contains(&r.start) {
            detail += &format!("\ndelete: {p}");
        }
    }
    for (a, b, _) in &moves_list {
        detail += &format!("\nmove: {a} → {b}");
    }
    tick.emit(Event::Plan {
        op: Op::Sync,
        files,
        dirs,
        bytes,
        write_bytes,
        detail,
        strategy: None,
        sync: Some(sync_json),
    });
    if o.dry_run || nothing {
        st.secs = t0.elapsed().as_secs_f64();
        tick.emit(Event::Done { files, bytes, secs: st.secs });
        return Ok(st);
    }
    let deleting = st.deleted + st.deleted_dirs;
    ensure!(
        deleting == 0 || o.yes,
        "this sync would delete {deleting} entries (listed above); pass --yes to confirm (nothing was written)"
    );
    ensure!(
        deleting <= MANY_DELETES || o.yes_many,
        "this sync would delete {deleting} entries, more than {MANY_DELETES}; pass --yes-many as well as --yes to \
         confirm (nothing was written)"
    );

    // ── phase 0: the undo journal, then mark dirty ──
    let device = match target {
        TargetSpec::Path(p) => p.display().to_string(),
        TargetSpec::Fd(fd) => format!("fd {fd}"),
    };
    let mut j = Journal {
        serial,
        device: format!("{device} ({} bytes)", tgt.bytes),
        mode: "undo".into(),
        moves: Vec::new(),
        writes: vec![JWrite { phase: "clean".into(), off: 0, pre: dirty_s0, post: orig_s0.clone() }],
    };
    journal::save(&j).context("write the sync journal (nothing was written to the volume)")?;
    let flags = v.flags;
    write_flags(&mut v, working_flags(flags), None)?;
    stop_after("dirty")?;

    // ── phase 1: data, one ascending stream ──
    let done0 = files - file_order.len() as u64;
    tick.progress(done0, 0, true);
    let reads: Vec<(PathBuf, u64)> = file_order.iter().map(|&i| (nodes[i].src.clone(), nodes[i].size)).collect();
    write_data(&excl, direct, &v, &segs, &mems, reads, &pol, o.readers, &mut tick, done0)?;
    excl.sync_all().context("sync")?;
    stop_after("data")?;

    // ── the redo journal: every metadata write from here on, with its before and after bytes ──
    let journaled = journal::plan_writes(&v.dev, meta).and_then(|writes| {
        j.mode = "redo".into();
        j.writes = writes;
        j.moves = moves_list
            .iter()
            .map(|(a, b, g)| {
                let ch = &gone_files[*g].ch;
                MoveRec {
                    from: a.clone(),
                    to: b.clone(),
                    first: ch.first,
                    len: ch.len,
                    valid_len: ch.valid_len,
                    no_fat_chain: ch.no_fat_chain,
                    mtime: ch.mtime,
                }
            })
            .collect();
        journal::save(&j)
    });
    if let Err(e) = journaled {
        // Only data was written, into free clusters: nothing references it.
        write_flags(&mut v, flags, None)?;
        journal::remove(serial)?;
        return Err(e.context("write the sync journal (the volume is unchanged)"));
    }

    // ── phases 2..8: alloc, fat, unlink, link, retire, free, clean ──
    let mut by_phase: HashMap<String, Vec<JWrite>> = HashMap::new();
    for w in &j.writes {
        by_phase.entry(w.phase.clone()).or_default().push(w.clone());
    }
    for phase in META_PHASES {
        journal::apply(&v.dev, by_phase.get(phase).map_or(&[][..], |w| &w[..]))?;
        if phase == "link" && torn_test {
            torn_write(&v, &new_sets, &pre_link)?;
        }
        if phase != "clean" {
            stop_after(phase)?;
        }
    }
    journal::remove(serial)?;
    tick.progress(files, bytes, true);
    if !tgt.direct {
        eprintln!("note: target does not support O_DIRECT, used buffered writes");
    }
    st.written = write_bytes;
    st.secs = t0.elapsed().as_secs_f64();
    tick.emit(Event::Done { files, bytes, secs: st.secs });
    drop(excl);
    Ok(st)
}

/// Test hook (FLASHCP_TEST_TORN=relocate): the first relocated set that crosses a sector
/// boundary gets only its first sector written: the rest is put back as it was before `link`.
fn torn_write(v: &Vol, new_sets: &[(usize, usize, usize)], pre: &HashMap<usize, Vec<u8>>) -> Result<()> {
    let Some(&(d, off, len)) =
        new_sets.iter().find(|&&(_, off, len)| off / SECTOR_ALIGN != (off + len - 1) / SECTOR_ALIGN)
    else {
        return Ok(());
    };
    let a = (off / SECTOR_ALIGN + 1) * SECTOR_ALIGN;
    let b = (off + len).min(pre[&d].len());
    if a < b {
        let mut p = Patches::default();
        p.object(v, &v.dirs[d].clusters, a, &pre[&d][a..b]);
        for (at, bytes) in p.into_inner() {
            v.dev.write_all_at(&bytes, at).context("torn write")?;
        }
        v.dev.sync_all().context("sync")?;
    }
    bail!("simulated torn write of a relocated entry set ({TORN_ENV})")
}

fn clone_node(n: &Node) -> Node {
    Node {
        name: n.name.clone(),
        src: n.src.clone(),
        is_dir: n.is_dir,
        size: n.size,
        mtime: n.mtime,
        children: Vec::new(),
        first_cluster: n.first_cluster,
        clusters: n.clusters,
    }
}

/// An empty volume written as a build: everything is new.
fn from_build(s: AddStats) -> SyncStats {
    SyncStats {
        new: s.files,
        new_dirs: s.dirs,
        data_written: s.data_bytes,
        written: s.written,
        cluster: s.cluster,
        secs: s.secs,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_rounds_and_merges_close_ranges() {
        // Rounded out to 512, clamped to the directory, merged across gaps up to 64 KB.
        assert_eq!(coalesce(vec![(40, 72)], 4096), vec![(0, 512)]);
        assert_eq!(coalesce(vec![(4000, 4090)], 4096), vec![(3584, 4096)]);
        assert_eq!(coalesce(vec![(0, 32), (600, 700)], 1 << 20), vec![(0, 1024)]);
        let far = 1024 + MERGE_GAP; // a gap of 64 KB + 512 after the first sector
        assert_eq!(
            coalesce(vec![(far, far + 1), (0, 32)], 1 << 20),
            vec![(0, 512), (far / 512 * 512, (far / 512 + 1) * 512)]
        );
        assert_eq!(
            coalesce(vec![(0, 32), (512 + MERGE_GAP, 512 + MERGE_GAP + 1)], 1 << 20),
            vec![(0, 1024 + MERGE_GAP)]
        );
        assert_eq!(coalesce(vec![(5, 5)], 4096), vec![]);
        assert_eq!(coalesce(vec![(0, 100), (50, 3000)], 4096), vec![(0, 3072)]);
    }

    fn file_set(name: &str, first: u32, len: u64, vendor: bool) -> Vec<u8> {
        let upc = crate::exfat::upcase_map();
        let n = Node {
            name: name.encode_utf16().collect(),
            src: PathBuf::new(),
            is_dir: false,
            size: len,
            mtime: (1_700_000_000, 0),
            children: vec![],
            first_cluster: first,
            clusters: 0,
        };
        let mut e = entry_set(&n, &upc);
        if vendor {
            // A benign vendor-extension secondary entry after the name.
            let mut x = [0u8; 32];
            x[0] = 0xE0;
            x[2..18].copy_from_slice(&[0xAB; 16]);
            e.extend_from_slice(&x);
            e[1] += 1;
        }
        finish_set(e, len, len)
    }

    #[test]
    fn repoint_keeps_vendor_secondaries_and_a_valid_checksum() {
        let mut set = file_set("Hello.TXT", 10, 5000, true);
        let before = set.clone();
        let new_name: Vec<u16> = "hello.txt".encode_utf16().collect();
        patch_set(
            &mut set,
            Some(Repoint { first: 99, contiguous: false, len: 70_000 }),
            Some((1_800_000_000, 0)),
            Some(&new_name),
        );
        assert_eq!(le16(&set, 2), checksum16(&set, true));
        assert_eq!(set[33] & 0x03, 0x01);
        assert_eq!(le32(&set, 52), 99);
        assert_eq!((le64(&set, 40), le64(&set, 56)), (70_000, 70_000));
        assert_eq!(set[96..], before[96..], "the vendor secondary survives");
        assert_eq!(set[32 + 4..32 + 6], before[32 + 4..32 + 6], "NameHash is unchanged by a case rename");
        let (t, t10) = ts(1_800_000_000, 0);
        assert_eq!((le32(&set, 12), set[21], set[23]), (t, t10, 0x80));
        assert_eq!(set[8..12], before[8..12], "the creation time is kept");
        // The parser accepts it and sees the new name and stream.
        let mut d = set.clone();
        d.resize(4096, 0);
        let (_, kids, _) = parse_dir(&d, None, "/", 4096, false).unwrap();
        assert_eq!(String::from_utf16_lossy(&kids[0].name), "hello.txt");
        assert!(!kids[0].no_fat_chain && kids[0].first == 99 && kids[0].len == 70_000);
        // Contiguous, and empty.
        patch_set(&mut set, Some(Repoint { first: 7, contiguous: true, len: 1 }), None, None);
        assert_eq!(set[33] & 0x03, 0x03);
        patch_set(&mut set, Some(Repoint { first: 0, contiguous: true, len: 0 }), None, None);
        assert_eq!((set[33] & 0x03, le32(&set, 52)), (0x01, 0));
        assert_eq!(le16(&set, 2), checksum16(&set, true));
    }

    #[test]
    fn in_place_only_when_every_changed_byte_is_in_one_sector() {
        let set = file_set("Hello.TXT", 10, 5000, false);
        let retime = Edit { len: set.len(), mtime: true, ..Default::default() };
        let repoint = Edit { len: set.len(), repoint: true, mtime: true, ..Default::default() };
        for off in (0..4096).step_by(32) {
            // A retime (2..24) never crosses; a re-point (2..64) crosses only from offset 480.
            assert!(one_sector(off, changed_end(&retime, &set, None)), "retime at {off}");
            assert_eq!(one_sector(off, changed_end(&repoint, &set, None)), off % 512 != 480, "re-point at {off}");
        }
        let dirlen = Edit { len: set.len(), dirlen: Some(8192), ..Default::default() };
        assert_eq!(changed_end(&dirlen, &set, None), 64);
        // A case rename changes the checksum and the characters that differ: "hello.txt" vs
        // "Hello.TXT" differ at characters 0, 6, 7, 8.
        let name: Vec<u16> = "hello.txt".encode_utf16().collect();
        let rename = Edit { len: set.len(), name: true, ..Default::default() };
        assert_eq!(changed_end(&rename, &set, Some(&name)), name_at(8) + 2);
        assert!(one_sector(0, name_at(8) + 2));
        assert!(!one_sector(448, name_at(8) + 2), "the last changed character is in the next sector");
        let same: Vec<u16> = "Hello.TXT".encode_utf16().collect();
        assert_eq!(changed_end(&rename, &set, Some(&same)), 4);
    }

    #[test]
    fn verifying_pays_unless_reads_are_slower_than_writes() {
        assert!(verify_pays(None, None) && verify_pays(Some(1.0), None) && verify_pays(None, Some(1.0)));
        assert!(verify_pays(Some(40e6), Some(20e6)) && verify_pays(Some(20e6), Some(20e6)));
        assert!(!verify_pays(Some(10e6), Some(20e6)));
    }

    #[test]
    fn the_keeper_owns_its_clusters_then_matches_the_source_then_is_newer() {
        let ch = |name: &str, is_dir: bool, len: u64, mtime: i64, off: usize| {
            let (t, t10) = ts(mtime, 0);
            Child {
                name: name.encode_utf16().collect(),
                is_dir,
                first: if len > 0 { 9 } else { 0 },
                no_fat_chain: true,
                len,
                valid_len: len,
                mtime: (t, t10, 0x80),
                set_off: off,
                set_len: 96,
            }
        };
        let best = |members: &[Child], w: Option<&Want<'_>>| {
            members.iter().max_by_key(|c| keeper_score(c, w)).unwrap().set_off
        };
        let name: Vec<u16> = "A.txt".encode_utf16().collect();
        let w = Want { is_dir: false, name: &name, size: 100, mtime: Some((1_700_000_000, 0)) };
        // The set with the source's size and time wins over the newer one with the source's name.
        let m = [ch("a.txt", false, 100, 1_700_000_000, 0), ch("A.txt", false, 0, 1_700_000_000, 96)];
        assert_eq!(best(&m, Some(&w)), 0);
        // Both match: the exact name, then the newer.
        let m = [ch("a.txt", false, 100, 1_700_000_000, 0), ch("A.txt", false, 100, 1_700_000_000, 96)];
        assert_eq!(best(&m, Some(&w)), 96);
        let m = [ch("A.txt", false, 100, 1_700_000_000, 0), ch("A.txt", false, 100, 1_700_000_000, 96)];
        assert_eq!(best(&m, Some(&w)), 96);
        // Neither matches: the newer. No source at all: the newer.
        let m = [ch("A.txt", false, 5, 1, 0), ch("A.txt", false, 6, 2, 96)];
        assert_eq!(best(&m, Some(&w)), 96);
        assert_eq!(best(&m, None), 96);
        // A directory owning clusters wins over the zero-length one fsck leaves.
        let dname: Vec<u16> = "docs".encode_utf16().collect();
        let wd = Want { is_dir: true, name: &dname, size: 0, mtime: None };
        let m = [ch("Docs", true, 4096, 1, 0), ch("docs", true, 0, 1, 96)];
        assert_eq!(best(&m, Some(&wd)), 0);
        assert_eq!(best(&m, None), 0);
    }

    #[test]
    fn samples_cover_small_files_whole_and_spread_over_big_ones() {
        assert_eq!(sample_ranges(10), vec![(0, 10)]);
        assert_eq!(sample_ranges(SAMPLES * SAMPLE), vec![(0, SAMPLES * SAMPLE)]);
        let len = 1 << 30;
        let s = sample_ranges(len);
        assert_eq!(s.len() as u64, SAMPLES);
        assert_eq!(s[0], (0, SAMPLE));
        assert_eq!(*s.last().unwrap(), (len - SAMPLE, SAMPLE));
        assert!(s.windows(2).all(|w| w[0].0 + SAMPLE <= w[1].0));
    }
}
