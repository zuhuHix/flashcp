//! `flashcp sync`: make a directory of an EXISTING exFAT volume match a local tree, writing only
//! what differs, without ever overwriting a live cluster.
//!
//! Everything is parsed, verified and planned before the first byte is written (as `add`):
//! the whole volume's allocation is checked against the bitmap, and every directory under the
//! destination is kept in memory. Per directory, source and volume entries are matched by
//! up-cased name:
//!
//! - unchanged: same size and LastModified (the quick check, [`same_mtime`]), or with
//!   `--checksum` the same content (read in physical order); nothing is written;
//! - changed: the new version goes into FRESH clusters (the pre-sync free space only), then the
//!   existing entry set is re-pointed in place (FirstCluster, flags, lengths, LastModified,
//!   SetChecksum: the first 64 bytes of the set, one write); the old clusters are freed last;
//! - a name that differs only by case is rewritten in place (same length and NameHash);
//! - new: appended (or written into a new directory) as `add` does;
//! - extraneous (on the volume, not in the source): kept, unless `--delete`;
//! - with `--delete`, a new file with the same size and time as a file that is going away, and
//!   the same content (sampled, or hashed with `--checksum`), is a MOVE: its new entry points at
//!   the old clusters, nothing is copied.
//!
//! A re-point is atomic when the 64 bytes lie in one 512-byte sector (the set does not start at
//! offset 480 of a sector); otherwise it is one 64-byte write across two adjacent sectors of one
//! 4 KB page (counted in [`SyncStats::split_repoints`]).
//!
//! Crash ordering (a sync between every phase; `FLASHCP_TEST_STOP_AFTER=<phase>` stops after it):
//!   0. `dirty`  VolumeDirty set. Nothing to do → nothing is written at all.
//!   1. `data`   new files, new versions of changed files, new directories (holding the entries
//!      of their new files, moved files and subdirectories), extension clusters of existing
//!      directories: all in clusters the pre-sync bitmap calls free, one ascending stream.
//!   2. `alloc`  bitmap bits of every new cluster, FAT chains of fragmented new objects.
//!   3. `fat`    FAT links extending existing directory chains.
//!   4. `unlink` entry sets of deleted entries, moved-away files and type-conflict losers lose
//!      their InUse bit; their clusters stay allocated (leaked until `free`).
//!   5. `link`   appended entry sets, extended directories' lengths, re-points, retimes and
//!      case renames.
//!   6. `free`   bitmap bits of the old versions of changed files and of deleted files and
//!      directories (whole subtrees), except clusters now owned by a move target. FAT entries
//!      of freed clusters are left alone (the bitmap is authoritative).
//!   7. clean    VolumeDirty cleared, PercentInUse updated.
//!
//! Crash after dirty/data: the old tree; after alloc/fat: the old tree plus leaked clusters;
//! after unlink: deleted and moved-away entries are gone (moved files are unreachable until
//! `link`; their data is intact on the device and in the source, a re-run copies them again),
//! changed files are still the OLD version; after link: the new tree, old versions leaked;
//! after free: done but dirty. No phase boundary has two entries referencing one cluster, or
//! two names in one directory equal case-insensitively (unlink is committed before link).
//!
//! Directory metadata is batched: each phase applies its changes to the in-memory copies of the
//! directories and writes, per directory, the touched byte ranges rounded to 512 bytes and
//! merged when less than 64 KB apart (the bytes between are rewritten unchanged).
//! Appends go at the end of a directory; deleted slots are not reused.

use crate::add::{
    AddStats, Patches, Seg, SegKind, carve, chain_entries, choose_extents, clean_flags, data_gap_policy,
    data_write_bytes, free_extents, run_clusters, stop_after, take_exact, working_flags, write_data, write_flags,
};
use crate::events::{Event, Op, Sink, Ticker, json_str};
use crate::exfat::{
    Node, TargetSpec, checksum16, entry_set, finish_set, scan_tree, set_bytes, set_direct, ts, validate_name,
};
use crate::util::human_bytes;
use crate::volume::*;
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

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
}

#[derive(Clone, Debug, Default)]
pub struct SyncStats {
    pub unchanged: u64,
    pub new: u64,
    /// Rewritten, or (with --checksum, same content) only retimed.
    pub changed: u64,
    /// Re-linked under a new path without copying, or renamed in place (case only).
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
    /// Re-pointed entry sets whose first 64 bytes span two 512-byte sectors (test statistic).
    pub split_repoints: u64,
}

pub fn sync(src: &Path, target: &Path, o: &SyncOpts, sink: Sink<'_>) -> Result<SyncStats> {
    sync_to(src, &TargetSpec::from(target), o, sink)
}

// ───────────────────────── plan model ─────────────────────────

/// A change to an existing entry set, applied in the link phase.
#[derive(Clone, Debug)]
struct EntryPatch {
    /// Directory holding the set, and the set's place in it.
    dir: usize,
    off: usize,
    len: usize,
    /// The source node (new name, mtime, size, allocation).
    node: usize,
    /// Point the stream at the node's new version.
    repoint: bool,
    mtime: bool,
    name: bool,
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

/// Applies a re-point, a retime and/or a case rename to an entry set in place (vendor
/// secondaries and every other byte untouched) and recomputes its SetChecksum.
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
                let at = 64 + 32 * k + 2 + 2 * j;
                set[at..at + 2].copy_from_slice(&c.to_le_bytes());
            }
        }
    }
    fix_checksum(set);
}

fn fix_checksum(set: &mut [u8]) {
    let c = checksum16(set, true);
    set[2..4].copy_from_slice(&c.to_le_bytes());
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

// ───────────────────────── content comparison ─────────────────────────

/// Bytes per sample of the sampled move check, and samples per file.
const SAMPLE: u64 = 4096;
const SAMPLES: u64 = 8;

/// The (offset, length) samples compared for a file of `len` bytes: the whole file when it is
/// at most SAMPLES × SAMPLE, else the first and last SAMPLE bytes and evenly spaced ones between.
fn sample_ranges(len: u64) -> Vec<(u64, u64)> {
    if len <= SAMPLES * SAMPLE {
        return vec![(0, len)];
    }
    let span = len - SAMPLE;
    let mut v: Vec<(u64, u64)> = (0..SAMPLES).map(|k| (span * k / (SAMPLES - 1), SAMPLE)).collect();
    v.dedup();
    v
}

/// Reads bytes [off, off + n) of a volume file stored in `runs` (bytes at or past ValidDataLength
/// read as zeros).
fn read_vol_file(v: &Vol, runs: &[(u64, u64)], valid: u64, off: u64, n: u64) -> Result<Vec<u8>> {
    let mut out = vec![0u8; n as usize];
    let (mut foff, end) = (0u64, (off + n).min(valid));
    for &(s, k) in runs {
        let (a, b) = (foff, foff + k * v.cs);
        let (lo, hi) = (a.max(off), b.min(end));
        if lo < hi {
            let phys = v.cluster_off(s as u32) + (lo - a);
            let at = (lo - off) as usize;
            v.dev
                .read_exact_at(&mut out[at..at + (hi - lo) as usize], phys)
                .with_context(|| format!("read {} bytes at {phys}", hi - lo))?;
        }
        foff = b;
        if foff >= end {
            break;
        }
    }
    Ok(out)
}

/// The sampled content check of a move candidate: the samples of the local file equal the
/// volume file's. A local file that cannot be read never matches.
fn samples_match(v: &mut Vol, ch: &Child, local: &Path, len: u64) -> Result<bool> {
    let runs = v.runs(ch.first, ch.no_fat_chain, ch.len, "move source")?;
    let Ok(f) = File::open(local) else { return Ok(false) };
    for (off, n) in sample_ranges(len) {
        let mut mine = vec![0u8; n as usize];
        if f.read_exact_at(&mut mine, off).is_err() {
            return Ok(false);
        }
        if read_vol_file(v, &runs, ch.valid_len, off, n)? != mine {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Hash of a local file as [`crate::verify::hash_vol_files`] computes it; None if it is not
/// `len` bytes long (any more) or cannot be read.
fn local_hash(p: &Path, len: u64) -> Option<u128> {
    let mut buf = vec![0u8; 1 << 20];
    let (n, h) = crate::verify::hash_file(p, &mut buf, &crate::verify::Progress::default()).ok()?;
    (n == len).then_some(h)
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

/// [`sync`] onto a [`TargetSpec`] (a path, or an inherited block-device fd).
pub fn sync_to(src: &Path, target: &TargetSpec, o: &SyncOpts, sink: Sink<'_>) -> Result<SyncStats> {
    let t0 = Instant::now();
    let mut tick = Ticker::new(sink);
    ensure!(
        fs::metadata(src).with_context(|| format!("source {}", src.display()))?.is_dir(),
        "source must be a directory"
    );
    let tgt = target.open_rw()?;
    // An empty volume with coarse clusters (a fresh stick) is written as a build, as by add.
    let tgt = {
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
    let mut v = Vol::open(excl.try_clone().context("dup target fd")?, tgt.bytes, false, true)?;
    let cs = v.cs;
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

    // ── destination (--to), creating missing parents as new directories ──
    let mut appends: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
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
                appends.entry(cur).or_default().push(k);
                new_parent = Some(k);
            }
        }
    }

    // ── match the source tree against the destination ──
    let mut patches: Vec<EntryPatch> = Vec::new();
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
                    appends.entry(d).or_default().push(c);
                    continue;
                };
                let renamed = ch.name != nodes[c].name;
                match (ch.is_dir, nodes[c].is_dir) {
                    (true, true) => {
                        let nd = v.load_child(d, &ch)?;
                        let retime = !same_mtime(nodes[c].mtime, ch.mtime);
                        if retime || renamed {
                            let (off, len) = (ch.set_off, ch.set_len);
                            patches.push(EntryPatch {
                                dir: d,
                                off,
                                len,
                                node: c,
                                repoint: false,
                                mtime: retime,
                                name: renamed,
                            });
                        }
                        work.push((nd, nodes[c].children.clone()));
                    }
                    (false, false) => pairs.push((d, ch, c)),
                    _ if o.delete => {
                        deletes.push((d, ch));
                        appends.entry(d).or_default().push(c);
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
        for &c in kids {
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

    // ── classification of matched files, move candidates, and the content checks ──
    // Candidate (new file, pool file) pairs by size and time; checked below.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, g) in gone_files.iter().enumerate() {
        if g.ch.len > 0 {
            by_size.entry(g.ch.len).or_default().push(i);
        }
    }
    let candidates: Vec<(usize, Vec<usize>)> = new_files
        .iter()
        .filter(|&&f| nodes[f].size > 0)
        .filter_map(|&f| {
            let pool: Vec<usize> = by_size
                .get(&nodes[f].size)?
                .iter()
                .copied()
                .filter(|&i| same_mtime(nodes[f].mtime, gone_files[i].ch.mtime))
                .collect();
            (!pool.is_empty()).then_some((f, pool))
        })
        .collect();
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
        let pool_used: BTreeSet<usize> = candidates.iter().flat_map(|c| c.1.iter().copied()).collect();
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

    let mut st = SyncStats { cluster: cs, ..Default::default() };
    let mut alloc_files: Vec<usize> = Vec::new(); // new and changed files that get new clusters
    let mut freed: Vec<(u64, u64)> = Vec::new();
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
        let base = EntryPatch {
            dir: d,
            off: ch.set_off,
            len: ch.set_len,
            node: c,
            repoint: false,
            mtime: false,
            name: renamed,
        };
        if same_content && same_time {
            if renamed {
                st.moved += 1;
                patches.push(base);
            } else {
                st.unchanged += 1;
            }
            st.data_skipped += n.size;
        } else if same_content {
            st.changed += 1; // metadata only
            patches.push(EntryPatch { mtime: true, ..base });
        } else {
            st.changed += 1;
            st.data_written += n.size;
            patches.push(EntryPatch { repoint: true, mtime: true, ..base });
            if n.size > 0 {
                alloc_files.push(c);
            }
            freed.extend(v.runs(ch.first, ch.no_fat_chain, ch.len, "changed file")?);
        }
    }

    // ── moves: each pool file at most once, first confirmed candidate wins ──
    let mut moves: HashMap<usize, usize> = HashMap::new(); // new file node → gone_files index
    let mut used: HashSet<usize> = HashSet::new();
    for (f, pool) in &candidates {
        for &i in pool {
            if used.contains(&i) {
                continue;
            }
            let ok = if o.checksum {
                src_hash[f].is_some_and(|h| vol_hash[&(1, i)] == h)
            } else {
                let ch = gone_files[i].ch.clone();
                samples_match(&mut v, &ch, &nodes[*f].src, nodes[*f].size)?
            };
            if ok {
                used.insert(i);
                moves.insert(*f, i);
                break;
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
    for &di in &gone_dirs {
        let r: Vec<(u64, u64)> = v.dirs[di].clusters.iter().map(|&c| (c as u64, 1)).collect();
        freed.extend(r);
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

    // ── sizes ──
    let mut ext_need: BTreeMap<usize, u64> = BTreeMap::new();
    for (&d, kids) in &appends {
        let need: u64 = kids.iter().map(|&c| set_bytes(&nodes[c])).sum();
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
        }
    }
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
    let mut ext_q: std::collections::VecDeque<_> = chosen.into();
    for &(obj, n) in &order {
        runs.insert(obj, carve(&mut ext_q, n));
    }
    let frag = |obj: Obj| runs.get(&obj).is_some_and(|r| r.len() > 1);
    for (&obj, r) in &runs {
        if let Obj::NewDir(i) | Obj::File(i) = obj {
            nodes[i].first_cluster = r[0].0 as u32;
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

    // ── phase 4 (unlink), applied to the working copies first ──
    let mut t_unlink = Touched::default();
    for (d, ch) in &deletes {
        let data = &mut v.dirs[*d].data;
        for k in 0..ch.set_len / 32 {
            data[ch.set_off + 32 * k] &= 0x7F;
        }
        t_unlink.mark(*d, ch.set_off, ch.set_off + ch.set_len);
    }
    let p_unlink = t_unlink.patches(&v);

    // ── phase 5 (link): appends, extended directories, re-points, retimes, renames ──
    let mut t_link = Touched::default();
    let mut sets: BTreeSet<(usize, usize, usize)> = BTreeSet::new(); // sets whose checksum to redo
    let mut ext_fat: Vec<(u32, u32)> = Vec::new();
    for (&d, kids) in &appends {
        let mut bytes = Vec::new();
        for &c in kids {
            bytes.extend(set_for(&nodes, c));
        }
        let (end, dlen) = (v.dirs[d].end, v.dirs[d].data.len());
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
        let ext = run_clusters(&r);
        let last = *dir.clusters.last().unwrap();
        let contiguous_ext = dir.no_fat_chain && r.len() == 1 && r[0].0 == last as u64 + 1;
        let new_len = (dir.clusters.len() + ext.len()) as u64 * cs;
        if !contiguous_ext {
            // Existing chain → extension. A NoFatChain directory needs its chain written first.
            let mut all = if dir.no_fat_chain { dir.clusters.clone() } else { vec![last] };
            all.extend(&ext);
            chain_entries(&all, &mut ext_fat);
        }
        if let Some((p, off, len)) = dir.entry {
            let set = &mut v.dirs[p].data[off..off + len];
            if !contiguous_ext {
                set[33] &= !0x02;
            }
            set[40..48].copy_from_slice(&new_len.to_le_bytes());
            set[56..64].copy_from_slice(&new_len.to_le_bytes());
            sets.insert((p, off, len));
        }
    }
    for p in &patches {
        let n = &nodes[p.node];
        let repoint =
            p.repoint.then(|| Repoint { first: n.first_cluster, contiguous: !frag(Obj::File(p.node)), len: n.size });
        if p.repoint && p.off % 512 == 480 {
            st.split_repoints += 1;
        }
        let set = &mut v.dirs[p.dir].data[p.off..p.off + p.len];
        patch_set(set, repoint, p.mtime.then_some(n.mtime), p.name.then_some(&n.name[..]));
        sets.insert((p.dir, p.off, p.len));
    }
    for &(d, off, len) in &sets {
        fix_checksum(&mut v.dirs[d].data[off..off + len]);
        t_link.mark(d, off, off + len);
    }
    let p_link = t_link.patches(&v);
    let mut p_fat = Patches::default();
    p_fat.fat(&v, &ext_fat);

    // ── phase 2 (alloc) and phase 6 (free) ──
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
    let mut p_free = Patches::default();
    bitmap_patches(&v, &mut bm, &freed, false, &mut p_free);

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
    let nothing = appends.is_empty() && patches.is_empty() && deletes.is_empty();
    let write_bytes = if nothing {
        0
    } else {
        data_write + p_alloc.bytes() + p_fat.bytes() + p_unlink.bytes() + p_link.bytes() + p_free.bytes() + 2 * bps
    };
    let direct = tgt.direct && cs.is_multiple_of(4096) && v.heap_off.is_multiple_of(4096);
    let pol = data_gap_policy(&excl, direct, &v, tgt.part_off * 512, o.dry_run, &segs, &sizes);

    let (files, dirs, bytes) = (st.new + st.changed, st.new_dirs, st.data_written);
    let mut moves_list: Vec<(String, String)> =
        moves.iter().map(|(&f, &g)| (gone_files[g].path.clone(), path_of[&f].clone())).collect();
    moves_list.sort();
    let shown = |p: &str, dir: bool| if dir { format!("{p}/") } else { p.to_string() };
    let sync_json = format!(
        r#"{{"unchanged":{},"new":{},"changed":{},"moved":{},"deleted":{},"deleted_dirs":{},"new_dirs":{},"bytes_skipped":{},"delete":{},"moves":{}}}"#,
        st.unchanged,
        st.new,
        st.changed,
        st.moved,
        st.deleted,
        st.deleted_dirs,
        st.new_dirs,
        st.data_skipped,
        json_list(
            delete_list
                .iter()
                .filter(|(_, d, r)| *d || !used.contains(&r.start))
                .map(|(p, d, _)| json_str(&shown(p, *d)))
        ),
        json_list(moves_list.iter().map(|(a, b)| format!(r#"{{"from":{},"to":{}}}"#, json_str(a), json_str(b))))
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
    for (a, b) in &moves_list {
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

    // ── phase 0: mark dirty ──
    let flags = v.flags;
    write_flags(&mut v, working_flags(flags), None)?;
    stop_after("dirty")?;

    // ── phase 1: data, one ascending stream ──
    let done0 = files - file_order.len() as u64;
    tick.progress(done0, 0, true);
    let reads: Vec<(PathBuf, u64)> = file_order.iter().map(|&i| (nodes[i].src.clone(), nodes[i].size)).collect();
    write_data(&excl, direct, &v, &segs, &mems, reads, &pol, o.readers, &mut tick, done0)?;
    stop_after("data")?;

    // ── phase 2: bitmap + new chains ──
    p_alloc.commit(&v.dev)?;
    stop_after("alloc")?;
    // ── phase 3: links into existing directory chains ──
    p_fat.commit(&v.dev)?;
    stop_after("fat")?;
    // ── phase 4: unlink ──
    p_unlink.commit(&v.dev)?;
    stop_after("unlink")?;
    // ── phase 5: link ──
    p_link.commit(&v.dev)?;
    stop_after("link")?;
    // ── phase 6: free ──
    p_free.commit(&v.dev)?;
    v.bitmap = bm;
    stop_after("free")?;

    // ── phase 7: clean ──
    let used_clusters = v.cc - v.free_clusters();
    let pct = (used_clusters * 100 / v.cc).min(100) as u8;
    write_flags(&mut v, clean_flags(flags), Some(pct))?;
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
        let (_, kids) = parse_dir(&d, None, "/", 4096).unwrap();
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
