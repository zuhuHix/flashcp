//! `flashcp recluster`: give an existing exFAT volume smaller clusters IN PLACE, without moving
//! file data.
//!
//! A stick formatted the usual way has big clusters (32 KB at 8 GB, 128 KB at 128 GB), so every
//! small file owns a whole one and writing small files means writing mostly padding. A big
//! cluster is exactly `ratio` small ones, so the same bytes can be described with small
//! clusters by rewriting only the metadata: boot sector, FAT, allocation bitmap, directories.
//!
//! # What is written, and why a crash never leaves a mix
//!
//! Every byte this command writes goes to clusters that are FREE in the volume as it is at that
//! moment, except one 512-byte sector: main boot sector 0. Writing that sector is the switch.
//! Four bytes of its boot code area (ignored by every driver; `optimize` keeps its job id
//! there; they lie below offset 440, clear of where a partition table would be, which matters
//! on a stick without one) are chosen so that the new sector has the SAME boot checksum as the old one
//! ([`neutral`]). The checksum sector (11) is therefore never written, and the switch is one
//! sector, which a device writes whole or not at all (the model the crash tests of `sync` and
//! drive mode use, [`crate::drive::dev::CrashDev`]). Before the switch the old volume is
//! complete and untouched; after it the new one is complete. The backup boot sector (12)
//! follows as a second one-sector write; see "The backup boot region" below.
//!
//! The new FAT is bigger than the old one and has to sit before the cluster heap, so the heap
//! must start later: the first clusters of the old heap (the *zone*: 0.1 % of the volume at
//! 4 KB) make room for the new FAT, bitmap, up-case table and all directories. Whatever lives
//! there has to move first, and it cannot move under the new geometry (the new FAT would
//! overwrite it while the old volume still needs it). So there are up to two switches:
//!
//! 1. **relocate** (old cluster size; skipped when the zone is already free): the parts of
//!    files that lie in the zone are copied to free clusters behind it; every directory with a
//!    moved child (and so on up to the root) is written again, compacted, into free clusters;
//!    a new bitmap and, if needed, the up-case table too. FAT entries are written only for
//!    clusters that are free or belong to a NoFatChain file in the current volume (both are
//!    ignored there). Sync. The result is parsed through the boot sector about to be written
//!    with the validating parser every command uses, and compared with the plan; moved file data
//!    and every directory written are read back and compared byte for byte. Then the
//!    switch: sector 0 with the new root directory cluster.
//! 2. **recluster**: the new FAT, bitmap, up-case table and every directory (re-numbered:
//!    new cluster = (old − zone) × ratio; NoFatChain where an object is one run, a FAT chain
//!    otherwise) go into the zone, which is free now. Sync, the same check, then the switch:
//!    sector 0 with the new geometry.
//!
//! An interruption before a switch leaves writes in free space only: nothing to undo, and the
//! next run starts over. After the first switch the volume is an ordinary one with a free
//! zone; the next run goes straight to step 2. Nothing is kept on the host.
//!
//! # The backup boot region
//!
//! Sectors 12..=23 mirror 0..=11. The Linux driver never reads them; `fsck.exfat`, Windows and
//! macOS use them when the main region is damaged. Main and backup cannot change in one atomic
//! step, so the order is: main (the commit point), sync, then backup sector 12 (its checksum
//! sector 23 needs no write either), sync. A crash in between leaves a valid new main region
//! and a valid backup that still describes the previous volume, which stays intact on the
//! medium until something writes to the new one. The rule that closes this window: every run
//! of `recluster` (and `--check`, which reports it as `"finish":true`) looks for exactly this
//! state (sectors 13..=23 equal 1..=11, sector 12 differs from sector 0, both regions
//! checksum-valid) and completes the backup write once the main volume has passed validation,
//! before anything else (also when it then refuses). What the window means elsewhere
//! (measured in CI, `recl-crash-window`): Linux mounts the new volume and `fsck.exfat` calls it
//! clean; macOS mounts it, `fsck_exfat` asks to update the alternate boot region; Windows does
//! NOT mount it ("not a recognized file system", while `chkdsk` finds no problems), and it does
//! not mount a volume whose backup region is invalid either (`recl-backup-zero`,
//! `recl-backup-badsum`): it wants the backup to equal the main region. No order of two
//! one-sector writes avoids a moment where they differ, so this window (one sync, per
//! switch) stays; the next `flashcp recluster`, or drive mode's `--check` → `finish`, closes it. Both boot sector writes go through an
//! O_DIRECT handle on a block device, so each reaches the device as one sector, not as the
//! page cache's 4 KiB block.
//!
//! # Refusals
//!
//! Decided before the first write, each with a reason code ([`Refused`]): a dirty or invalid
//! volume, an interrupted `flashcp sync` or drive-mode commit, a sector size other than 512,
//! entries that own clusters this command does not know how to re-address, clusters marked bad
//! (their marks would be lost), a backup boot region
//! that does not match the main one, a geometry that cannot be expressed, and too little free
//! space. Kept: every file's bytes and position, names, attributes, all timestamps, benign
//! entries (volume GUID, vendor extensions), the label and the serial number. Not carried
//! over: unused directory entries, and drive mode's idle journal file (the driver creates a
//! new one).

use crate::add::{carve, free_extents, worth_filling};
use crate::drive::dev::{Dev, FileDev};
use crate::events::json_str;
use crate::exfat::{FatMode, TargetSpec, checksum16, checksum32, set_direct};
use crate::util::human_bytes;
use crate::volume::{Child, EOC, FLAG_DIRTY, MAX_CLUSTER_COUNT, Vol, cluster_set, le16, le32, le64, mark_range};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::Instant;

#[cfg(test)]
mod tests;

/// The cluster size `recluster` goes to unless told otherwise: what the cluster-size optimizer
/// of `build` picks for small files and never exceeds on a removable stick.
pub const DEFAULT_CLUSTER: u64 = 4096;
/// `--check` says a volume would benefit from this cluster size on.
pub const OFFER_MIN_CLUSTER: u64 = 16 << 10;
const SECTOR: u64 = 512;
/// The largest single device write, and the page the sparse FAT is written in.
const MAX_REQ: u64 = 4 << 20;
const FAT_PAGE: u64 = 4096;
/// A sync at least this often while data is copied, so progress is what is on the medium.
const SYNC_EVERY: u64 = 32 << 20;
/// Zero bytes the root directory keeps after its last entry, at least. flashcp's own journal
/// files (drive mode's, `sync`'s) are linked with an entry set that must fit one sector of the
/// root as it is: a root packed to the last byte would make drive mode read-only.
const ROOT_SLACK: u64 = 2048;
/// Test hook: stop (with an error, as if killed) right after the main boot sector of the
/// named switch (`relocated` | `reclustered`) is on the medium, before its backup copy: the
/// window a crash can leave, for the cross-OS checks. Only with the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
const STOP_ENV: &str = "FLASHCP_TEST_RECLUSTER_STOP";
/// The boot code bytes a switch may tune lie below this offset of sector 0 (see [`neutral`]).
const NEUTRAL_TOP: usize = 439;
/// More files and directories than this are refused (the plan keeps them all in memory).
const MAX_OBJECTS: usize = 8 << 20;
/// ... and more directory bytes, or more file fragments, than these.
const MAX_META: usize = 1 << 30;
const MAX_PIECES: usize = 16 << 20;

#[derive(Clone, Debug)]
pub struct ReclusterOpts {
    /// New cluster size in bytes (default: [`DEFAULT_CLUSTER`], or the next size the volume allows).
    pub cluster: Option<u32>,
    /// `Prefix`: write only the FAT entries in use. `Full`: zero the rest of the new FAT too.
    pub fat: FatMode,
    pub dry_run: bool,
    /// Refuse (reason `wrong-volume`, nothing written) unless the boot sector read through the
    /// claimed fd carries this volume serial: the caller checked and asked about that volume.
    pub expect_serial: Option<u32>,
    /// Refuse the same way unless the claimed device's hardware serial (from sysfs, through
    /// the fd: [`hw_serial_of`]) is this one; an unreadable one is refused too.
    pub expect_hw: Option<String>,
}

#[cfg(test)]
thread_local! {
    /// Tests: treat the target as a block device whose O_DIRECT read fails.
    pub(crate) static DIRECT_READ_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Tests: the hardware serial [`hw_serial_of`] reports (Some(None): none can be read).
    pub(crate) static HW_SERIAL: std::cell::RefCell<Option<Option<String>>> = const { std::cell::RefCell::new(None) };
}

/// Sector 0 as the medium holds it now. On a block device it is read with O_DIRECT through the
/// same open file (a reopen of `/proc/self/fd/N`), so a changed medium is not hidden by the page
/// cache; if that cannot be done the answer is an error, never a cached copy. An image file is
/// read normally (its page cache is the file).
pub(crate) fn fresh_sector0(f: &File) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    #[cfg(test)]
    if DIRECT_READ_FAILS.with(|c| c.get()) {
        bail!("O_DIRECT read of the boot sector failed (injected)");
    }
    if f.metadata()?.file_type().is_block_device() {
        let d = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(format!("/proc/self/fd/{}", f.as_raw_fd()))
            .context("open the device for a read past the page cache")?;
        // 4096 bytes, aligned: valid for 512- and 4096-byte logical blocks.
        let mut b = crate::util::AlignedBuf::new(4096);
        d.read_exact_at(b.as_mut(), 0).context("read the boot sector past the page cache")?;
        return Ok(b.as_ref()[..SECTOR as usize].to_vec());
    }
    let mut b = vec![0u8; SECTOR as usize];
    f.read_exact_at(&mut b, 0)?;
    Ok(b)
}

/// The hardware serial of the disk behind an open block device, from sysfs: `rdev` →
/// `/sys/dev/block/MAJ:MIN` → the first parent that is a USB device (`idVendor`) → its
/// `serial` (what udev's `ID_SERIAL_SHORT` is made from). None for an image file or a device
/// without one. Whitespace and control characters become `_`.
pub fn hw_serial_of(f: &File) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = HW_SERIAL.with(|h| h.borrow().clone()) {
        return v;
    }
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = f.metadata().ok()?;
    md.file_type().is_block_device().then(|| hw_serial_in(std::path::Path::new("/sys"), md.rdev())).flatten()
}

/// [`hw_serial_of`] for a device number under the sysfs tree `sys`.
pub fn hw_serial_in(sys: &std::path::Path, rdev: u64) -> Option<String> {
    let (maj, min) = (libc::major(rdev), libc::minor(rdev));
    let mut dir = std::fs::canonicalize(sys.join(format!("dev/block/{maj}:{min}"))).ok()?;
    let top = std::fs::canonicalize(sys).ok()?;
    while dir.starts_with(&top) && dir != top {
        if dir.join("idVendor").is_file()
            && let Ok(s) = std::fs::read_to_string(dir.join("serial"))
        {
            let s: String =
                s.trim().chars().map(|c| if c.is_whitespace() || c.is_control() { '_' } else { c }).collect();
            return (!s.is_empty()).then_some(s);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// The volume serial of a boot sector (what udev shows as `ID_FS_UUID`, `XXXX-XXXX`).
pub fn boot_serial(boot: &[u8]) -> u32 {
    u32::from_le_bytes([boot[100], boot[101], boot[102], boot[103]])
}

/// `1A2B-3C4D` (udev's `ID_FS_UUID` of an exFAT volume) as the serial number.
pub fn parse_serial(s: &str) -> Option<u32> {
    let h: String = s.chars().filter(|c| *c != '-').collect();
    (h.len() == 8).then(|| u32::from_str_radix(&h, 16).ok()).flatten()
}

/// Why `recluster` will not touch a volume. Nothing was written.
#[derive(Debug)]
pub struct Refused {
    /// A stable code (see `docs/drive-mode-interface.md`).
    pub reason: &'static str,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

/// A cluster size the way the README writes them: "128 KB", "512 B".
pub fn kb(cluster: u64) -> String {
    if cluster >= 1 << 20 {
        format!("{} MB", cluster >> 20)
    } else if cluster >= 1024 {
        format!("{} KB", cluster >> 10)
    } else {
        format!("{cluster} B")
    }
}

fn refused(reason: &'static str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refused { reason, message: message.into() })
}

/// The refusal in an error of [`recluster`] or [`check`], if it is one.
pub fn refusal_of(e: &anyhow::Error) -> Option<&Refused> {
    e.downcast_ref::<Refused>()
}

/// What `--dry-run` prints and every run announces before its first write.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub old_cluster: u64,
    pub cluster: u64,
    pub files: u64,
    pub dirs: u64,
    /// File data on the volume (it stays where it is, except `move_bytes`).
    pub data_bytes: u64,
    /// Everything that will be written to the device.
    pub write_bytes: u64,
    /// Of that, file data copied out of the way of the new FAT.
    pub move_bytes: u64,
    /// From the device's cached profile (default constants without one).
    pub est_secs: f64,
    pub free_before: u64,
    pub free_after: u64,
    /// Files that are in more than one piece (they keep their FAT chains).
    pub fragmented: u64,
    /// Two switches (data has to move out of the zone first) instead of one.
    pub relocate: bool,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    /// False: the clusters already had the wanted size (at most the backup boot sector of an
    /// interrupted switch was completed).
    pub changed: bool,
    pub old_cluster: u64,
    pub cluster: u64,
    pub written: u64,
    pub secs: f64,
    pub free_before: u64,
    pub free_after: u64,
    /// The backup boot sector of an interrupted switch was completed first.
    pub finished_switch: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Plan(Plan),
    /// Bytes written so far of `Plan::write_bytes` (at most ~10 per second).
    Progress {
        bytes_done: u64,
        bytes_total: u64,
    },
    /// A switch is on the medium: `relocated` (still the old cluster size; the zone is free)
    /// or `reclustered` (the volume has the new cluster size from here on).
    Switched {
        step: &'static str,
        cluster: u64,
    },
    Done(Stats),
}

impl Event {
    /// One JSON line of the contract in `docs/drive-mode-interface.md`.
    pub fn to_json(&self) -> String {
        match self {
            Event::Plan(p) => format!(
                r#"{{"event":"plan","op":"recluster","cluster":{},"new_cluster":{},"files":{},"dirs":{},"bytes":{},"write_bytes":{},"move_bytes":{},"est_secs":{:.1},"free":{},"free_after":{},"fragmented":{},"switches":{}}}"#,
                p.old_cluster,
                p.cluster,
                p.files,
                p.dirs,
                p.data_bytes,
                p.write_bytes,
                p.move_bytes,
                p.est_secs,
                p.free_before,
                p.free_after,
                p.fragmented,
                if p.relocate { 2 } else { 1 }
            ),
            Event::Progress { bytes_done, bytes_total } => format!(
                r#"{{"event":"progress","op":"recluster","bytes_done":{bytes_done},"bytes_total":{bytes_total}}}"#
            ),
            Event::Switched { step, cluster } => {
                format!(r#"{{"event":"switched","op":"recluster","step":"{step}","cluster":{cluster}}}"#)
            }
            Event::Done(s) => format!(
                r#"{{"event":"done","op":"recluster","changed":{},"cluster":{},"old_cluster":{},"written":{},"free":{},"secs":{:.3},"finished_switch":{}}}"#,
                s.changed, s.cluster, s.old_cluster, s.written, s.free_after, s.secs, s.finished_switch
            ),
        }
    }
}

/// The `refused` line of the JSON contract.
pub fn refused_json(r: &Refused) -> String {
    format!(r#"{{"event":"refused","op":"recluster","reason":"{}","message":{}}}"#, r.reason, json_str(&r.message))
}

// ───────────────────────── runs ─────────────────────────

/// Clusters of an object in file order: (first cluster, count), adjacent ones merged.
type Runs = Vec<(u64, u64)>;

fn count(r: &[(u64, u64)]) -> u64 {
    r.iter().map(|x| x.1).sum()
}

fn push_run(r: &mut Runs, s: u64, n: u64) {
    if n == 0 {
        return;
    }
    match r.last_mut() {
        Some(l) if l.0 + l.1 == s => l.1 += n,
        _ => r.push((s, n)),
    }
}

fn runs_of(clusters: &[u32]) -> Runs {
    let mut r = Runs::new();
    for &c in clusters {
        push_run(&mut r, c as u64, 1);
    }
    r
}

/// The first `m` clusters of `runs`, and the rest.
fn split(runs: &[(u64, u64)], mut m: u64) -> (Runs, Runs) {
    let (mut a, mut b) = (Runs::new(), Runs::new());
    for &(s, n) in runs {
        let t = n.min(m);
        push_run(&mut a, s, t);
        push_run(&mut b, s + t, n - t);
        m -= t;
    }
    (a, b)
}

/// How many clusters from the start of the chain reach up to (and include) its last cluster
/// below `zend`: the prefix that must move for the object to leave the zone while every FAT
/// entry that stays in use keeps its value.
fn zone_prefix(runs: &[(u64, u64)], zend: u64) -> u64 {
    let (mut seen, mut m) = (0, 0);
    for &(s, n) in runs {
        if s < zend {
            m = seen + n.min(zend - s);
        }
        seen += n;
    }
    m
}

/// Byte extents (device offset, length) of the first `bytes` bytes of `runs`, merged.
fn extents(runs: &[(u64, u64)], heap: u64, cs: u64, mut bytes: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(s, n) in runs {
        let len = (n * cs).min(bytes);
        if len == 0 {
            break;
        }
        let off = heap + (s - 2) * cs;
        match out.last_mut() {
            Some(l) if l.0 + l.1 == off => l.1 += len,
            _ => out.push((off, len)),
        }
        bytes -= len;
    }
    out
}

fn bit(bitmap: &[u8], c: u64) -> bool {
    let i = (c - 2) as usize;
    bitmap[i / 8] >> (i % 8) & 1 == 1
}

/// The first of clusters [s, s + n) whose bit equals `want`, whole bytes eight at a time.
fn find_bit(bitmap: &[u8], s: u64, n: u64, want: bool) -> Option<u64> {
    let (mut i, end) = (s - 2, s - 2 + n);
    let skip = if want { 0x00 } else { 0xFF };
    while i < end {
        if i.is_multiple_of(8) && end - i >= 8 && bitmap[(i / 8) as usize] == skip {
            i += 8;
            continue;
        }
        if bit(bitmap, i + 2) == want {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

// ───────────────────────── the volume as it is ─────────────────────────

/// A file or directory (not the root).
struct Obj {
    /// Index of the parent directory in [`Tree::dirs`].
    parent: u32,
    /// Offset and length of its entry set in the parent's [`DirNode::content`].
    at: u32,
    set_len: u32,
    is_dir: bool,
    /// NoFatChain, as stored.
    nfc: bool,
    /// DataLength.
    len: u64,
    runs: Runs,
    /// Index into [`Tree::dirs`] (directories only).
    dir: u32,
}

struct DirNode {
    /// The directory's live entries in their order, unused entries left out: what it will hold.
    content: Vec<u8>,
    runs: Runs,
    kids: Vec<u32>,
    /// Its own object (None: the root).
    obj: Option<u32>,
}

/// Everything a volume references, as the validating parser read it.
struct Tree {
    cs: u64,
    heap: u64,
    cc: u64,
    fat_off: u64,
    vol_bytes: u64,
    sector0: Vec<u8>,
    /// Parents come before their children (index 0 is the root).
    dirs: Vec<DirNode>,
    objs: Vec<Obj>,
    /// Offsets of the bitmap and up-case entries in the root's content.
    bitmap_at: usize,
    upcase_at: usize,
    bitmap: Vec<u8>,
    bitmap_runs: Runs,
    upcase_runs: Runs,
    upcase_raw: Vec<u8>,
    upc: Vec<u16>,
    label: String,
    files: u64,
    data_bytes: u64,
}

fn claim(refd: &mut [u8], bitmap: &[u8], runs: &[(u64, u64)], what: &dyn Fn() -> String) -> Result<()> {
    for &(s, n) in runs {
        mark_range(refd, s as u32, n)
            .map_err(|c| anyhow!("cluster {c} is used twice (cross-linked), again by {}; run fsck.exfat", what()))?;
        if let Some(c) = find_bit(bitmap, s, n, false) {
            bail!("cluster {c} used by {} is marked free in the allocation bitmap; run fsck.exfat", what());
        }
    }
    Ok(())
}

/// Walks an opened volume (boot region, root, bitmap and up-case table already validated by
/// [`Vol::open`]): every directory and every allocation, checked against the bitmap and
/// against each other like `add` does, plus what only this command cares about (entries that
/// own clusters it cannot re-address, journals of interrupted flashcp runs).
fn load(v: &mut Vol) -> Result<Tree> {
    journal_sane(v)?;
    crate::drive::djournal::hide_from(v).map_err(|e| refused("pending-commit", format!("{e:#}")))?;
    let root = v.dirs.pop().ok_or_else(|| anyhow!("no root directory"))?;
    if root.children.values().any(crate::volume::is_sync_journal) {
        return Err(refused(
            "pending-sync",
            format!("the volume holds the journal of {}; finish that first", crate::volume::SYNC_HINT),
        ));
    }
    let mut t = Tree {
        cs: v.cs,
        heap: v.heap_off,
        cc: v.cc,
        fat_off: v.fat_off,
        vol_bytes: v.vol_bytes,
        sector0: v.sector0.clone(),
        dirs: vec![DirNode { content: Vec::new(), runs: runs_of(&root.clusters), kids: Vec::new(), obj: None }],
        objs: Vec::new(),
        bitmap_at: usize::MAX,
        upcase_at: usize::MAX,
        bitmap: std::mem::take(&mut v.bitmap),
        bitmap_runs: runs_of(&v.bitmap_clusters),
        upcase_runs: runs_of(&v.upcase_clusters),
        upcase_raw: Vec::new(),
        upc: v.upc.clone(),
        label: v.label.clone(),
        files: 0,
        data_bytes: 0,
    };
    let mut refd = cluster_set(t.cc);
    claim(&mut refd, &t.bitmap, &t.dirs[0].runs, &|| "the root directory".into())?;
    claim(&mut refd, &t.bitmap, &t.bitmap_runs, &|| "the allocation bitmap".into())?;
    claim(&mut refd, &t.bitmap, &t.upcase_runs, &|| "the up-case table".into())?;
    // One directory's raw data at a time: a child is read when its turn comes.
    let mut queue: VecDeque<(usize, Option<(Child, String)>)> = VecDeque::from([(0, None)]);
    let (mut root, mut kept, mut pieces) = (Some(root), 0usize, 0usize);
    while let Some((di, child)) = queue.pop_front() {
        let d = match child {
            Some((ch, parent)) => v.read_child(0, &parent, &ch)?,
            None => root.take().ok_or_else(|| anyhow!("internal error: the root is walked once"))?,
        };
        let by_off: HashMap<usize, &Child> = d.children.values().map(|c| (c.set_off, c)).collect();
        let shown = if d.path.is_empty() { "/" } else { &d.path };
        let mut content = Vec::with_capacity(d.end);
        let mut i = 0;
        while i + 32 <= d.end {
            let ty = d.data[i];
            if ty & 0x80 == 0 {
                // Unused, or deleted with what an interrupted sync left of its set: not kept.
                let sc = if ty == 0x05 { d.data[i + 1] as usize } else { 0 };
                i += 32;
                for _ in 0..sc {
                    if i + 32 > d.end || d.data[i] & 0x40 == 0 {
                        break;
                    }
                    i += 32;
                }
                continue;
            }
            let n = match ty {
                0x81..=0x83 => 32,
                _ => 32 * (1 + d.data[i + 1] as usize),
            };
            ensure!(i + n <= d.end, "directory {shown}: an entry set runs past the end of the directory");
            let set = &d.data[i..i + n];
            match ty {
                0x81 => t.bitmap_at = content.len(),
                0x82 => {
                    t.upcase_at = content.len();
                    let len = le64(set, 24).min(t.upcase_runs.iter().map(|r| r.1).sum::<u64>() * t.cs);
                    t.upcase_raw = v.read_clusters(&v.upcase_clusters, len)?;
                }
                0x83 => {}
                0x85 => {
                    // Not among the parsed children: flashcp's own idle journal, left behind.
                    let Some(ch) = by_off.get(&i) else {
                        i += n;
                        continue;
                    };
                    let what = || format!("{}/{}", d.path, String::from_utf16_lossy(&ch.name));
                    for e in set.as_chunks::<32>().0.iter().skip(2 + (set[35] as usize).div_ceil(15)) {
                        if e[0] & 0x80 != 0 && e[1] & 1 != 0 && le32(e, 20) != 0 {
                            return Err(refused(
                                "unsupported-entry",
                                format!(
                                    "{} has a vendor entry ({:#04x}) that owns clusters; flashcp cannot re-address it",
                                    what(),
                                    e[0]
                                ),
                            ));
                        }
                    }
                    let oi = t.objs.len();
                    if oi >= MAX_OBJECTS {
                        return Err(refused(
                            "too-many-files",
                            format!("the volume holds more than {MAX_OBJECTS} files and directories"),
                        ));
                    }
                    let (runs, dir) = if ch.is_dir {
                        let runs = runs_of(&v.object_clusters(ch.first, ch.no_fat_chain, ch.len, &what())?);
                        t.dirs.push(DirNode {
                            content: Vec::new(),
                            runs: runs.clone(),
                            kids: Vec::new(),
                            obj: Some(oi as u32),
                        });
                        queue.push_back((t.dirs.len() - 1, Some(((*ch).clone(), d.path.clone()))));
                        (runs, t.dirs.len() as u32 - 1)
                    } else {
                        let runs = v.runs(ch.first, ch.no_fat_chain, ch.len, &what())?;
                        t.files += 1;
                        t.data_bytes += ch.len;
                        (runs, u32::MAX)
                    };
                    claim(&mut refd, &t.bitmap, &runs, &what)?;
                    pieces += runs.len();
                    t.objs.push(Obj {
                        parent: di as u32,
                        at: content.len() as u32,
                        set_len: n as u32,
                        is_dir: ch.is_dir,
                        nfc: ch.no_fat_chain,
                        len: ch.len,
                        runs,
                        dir,
                    });
                    t.dirs[di].kids.push(oi as u32);
                }
                // A benign primary the parser skipped (volume GUID, …), kept as it is.
                _ => {
                    // (A primary's AllocationPossible is in its flags at 4, a secondary's at 1.)
                    let owns = |e: &[u8], flags: usize| e[flags] & 1 != 0 && le32(e, 20) != 0;
                    let secondaries = set.as_chunks::<32>().0.iter().skip(1);
                    if matches!(ty, 0xA1 | 0xA2) || owns(set, 4) || secondaries.clone().any(|e| owns(e, 1)) {
                        return Err(refused(
                            "unsupported-entry",
                            format!(
                                "directory {shown} holds an entry of type {ty:#04x} (TexFAT, or one that owns \
                                 clusters); flashcp cannot re-address it"
                            ),
                        ));
                    }
                }
            }
            content.extend_from_slice(set);
            i += n;
        }
        kept += content.len();
        if kept > MAX_META || pieces > MAX_PIECES {
            return Err(refused(
                "too-many-files",
                "the volume's directories or file fragments are more than recluster plans in memory",
            ));
        }
        t.dirs[di].content = content;
    }
    ensure!(t.bitmap_at != usize::MAX && t.upcase_at != usize::MAX, "the root directory lost its system entries");
    // Allocated, referenced by nothing: leaked (dropped by the rewrite, as fsck.exfat would),
    // or marked bad (FAT entry 0xFFFFFFF7), which must never become free space.
    for (i, (&b, &r)) in t.bitmap.iter().zip(&refd).enumerate() {
        let mut m = b & !r;
        while m != 0 {
            let c = 2 + i as u64 * 8 + m.trailing_zeros() as u64;
            m &= m - 1;
            if c < t.cc + 2 && v.fat(c as u32)? == BAD {
                return Err(refused(
                    "bad-clusters",
                    format!(
                        "the volume has clusters marked bad (cluster {c} and maybe more); recluster would lose \
                         those marks"
                    ),
                ));
            }
        }
    }
    Ok(t)
}

/// The FAT entry of a cluster marked bad.
const BAD: u32 = 0xFFFF_FFF7;

/// Drive mode's journal file, if the root has one, must point into the heap before its header
/// is read.
fn journal_sane(v: &Vol) -> Result<()> {
    let bad = v
        .dirs
        .first()
        .into_iter()
        .flat_map(|r| r.children.values())
        .find(|c| crate::drive::djournal::is_journal(c) && !(v.valid(c.first) && contiguous_ok(v, c.first, c.len)));
    match bad {
        Some(_) => {
            Err(refused("invalid-volume", "drive mode's journal file lies outside the cluster heap; run fsck.exfat"))
        }
        None => Ok(()),
    }
}

fn contiguous_ok(v: &Vol, first: u32, len: u64) -> bool {
    crate::volume::contiguous_run(first, len.div_ceil(v.cs).max(1), v.cc).is_ok()
}

// ───────────────────────── the new geometry ─────────────────────────

/// Where the new FAT and heap go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geo {
    cs: u64,
    /// New clusters per old cluster.
    ratio: u64,
    /// Bytes; the FAT starts where the old heap started.
    fat_off: u64,
    fat_bytes: u64,
    heap: u64,
    cc: u64,
    /// Old clusters between the old and the new heap start (the FAT's share of the zone).
    k: u64,
}

/// The geometry for clusters of `nc` bytes: the new heap starts a whole number of old clusters
/// after the old one (so every old cluster is `ratio` new ones), as early as the new FAT,
/// placed at the old heap's start, allows.
fn new_geometry(vol_bytes: u64, old_heap: u64, old_cs: u64, old_cc: u64, nc: u64) -> Result<Geo> {
    if !(nc.is_power_of_two() && nc >= SECTOR) {
        return Err(refused("bad-cluster-size", format!("a cluster size is a power of two from 512 bytes, not {nc}")));
    }
    if nc >= old_cs {
        return Err(refused(
            "bad-cluster-size",
            format!(
                "the volume has {} clusters; recluster only makes them smaller (bigger ones would mean moving every \
                 file), not {}",
                kb(old_cs),
                kb(nc)
            ),
        ));
    }
    let fat_for = |k: u64| -> Option<(u64, u64, u64)> {
        let heap = old_heap + k * old_cs;
        let cc = vol_bytes.checked_sub(heap)? / nc;
        let fat = fat_bytes_for(cc);
        (cc >= 1 && old_heap + fat <= heap).then_some((heap, cc, fat))
    };
    // The FAT only shrinks as the heap moves up, so the size for the old heap start is enough.
    let widest = fat_bytes_for(vol_bytes.saturating_sub(old_heap) / nc);
    let mut k = widest.div_ceil(old_cs).max(1);
    while k > 1 && fat_for(k - 1).is_some() {
        k -= 1;
    }
    let too_small = || refused("geometry", "the volume is too small to hold a FAT for clusters that small");
    let (heap, cc, fat_bytes) = fat_for(k).ok_or_else(too_small)?;
    if k >= old_cc {
        return Err(too_small());
    }
    if cc > MAX_CLUSTER_COUNT {
        return Err(refused(
            "too-many-clusters",
            format!("{} clusters would give the volume {cc} of them; exFAT allows {MAX_CLUSTER_COUNT}", kb(nc)),
        ));
    }
    if heap / SECTOR > u32::MAX as u64 || fat_bytes / SECTOR > u32::MAX as u64 {
        return Err(refused("geometry", "the new FAT or cluster heap offset does not fit the boot sector's fields"));
    }
    Ok(Geo { cs: nc, ratio: old_cs / nc, fat_off: old_heap, fat_bytes, heap, cc, k })
}

/// The cluster size to go to and its geometry; None when the clusters are already that small.
fn target_geometry(vol_bytes: u64, heap: u64, cs: u64, cc: u64, want: Option<u32>) -> Result<Option<Geo>> {
    if let Some(nc) = want {
        if nc as u64 == cs {
            return Ok(None);
        }
        return new_geometry(vol_bytes, heap, cs, cc, nc as u64).map(Some);
    }
    if cs <= DEFAULT_CLUSTER {
        return Ok(None);
    }
    // 4 KB, or the next size whose cluster count exFAT can express (volumes over 16 TB).
    let mut nc = DEFAULT_CLUSTER;
    loop {
        match new_geometry(vol_bytes, heap, cs, cc, nc) {
            Err(e) if refusal_of(&e).is_some_and(|r| r.reason == "too-many-clusters") && nc * 2 < cs => nc *= 2,
            r => return r.map(Some),
        }
    }
}

/// The new volume's metadata, packed at the start of its heap: bitmap, up-case table, then
/// every directory (parents first), and the old clusters all of it displaces.
struct Layout {
    bitmap_n: u64,
    upcase_n: u64,
    dir_first: Vec<u64>,
    dir_n: Vec<u64>,
    /// Old clusters [2, 2 + kz) are the zone: FAT plus metadata.
    kz: u64,
}

/// Clusters of `cs` bytes directory `di` takes, rewritten.
fn dir_clusters(t: &Tree, di: usize, cs: u64) -> u64 {
    (t.dirs[di].content.len() as u64 + if di == 0 { ROOT_SLACK } else { 0 }).div_ceil(cs).max(1)
}

fn layout(t: &Tree, g: &Geo) -> Result<Layout> {
    let bitmap_n = g.cc.div_ceil(8).div_ceil(g.cs);
    let upcase_n = (t.upcase_raw.len() as u64).div_ceil(g.cs);
    let mut next = 2 + bitmap_n + upcase_n;
    let (mut dir_first, mut dir_n) = (Vec::with_capacity(t.dirs.len()), Vec::with_capacity(t.dirs.len()));
    for di in 0..t.dirs.len() {
        let n = dir_clusters(t, di, g.cs);
        dir_first.push(next);
        dir_n.push(n);
        next += n;
    }
    let kz = g.k + ((next - 2) * g.cs).div_ceil(t.cs);
    if kz >= t.cc || next - 2 > g.cc {
        return Err(refused("geometry", "the volume is too small to hold its metadata with clusters that small"));
    }
    Ok(Layout { bitmap_n, upcase_n, dir_first, dir_n, kz })
}

// ───────────────────────── the volume a switch makes current ─────────────────────────

#[derive(Clone, Default, PartialEq, Debug)]
struct Place {
    runs: Runs,
    /// NoFatChain (meaningless without clusters, and for the root).
    nfc: bool,
}

/// FAT entries for clusters [start, start + n): each points at the next, the last at `next`.
#[derive(Clone, Copy)]
struct FatRun {
    start: u64,
    n: u64,
    next: u32,
    /// The object was NoFatChain so far: entries of clusters it already owns are ignored by
    /// the current volume and may change.
    loose: bool,
}

fn chain(runs: &[(u64, u64)], loose: bool, out: &mut Vec<FatRun>) {
    for (i, &(start, n)) in runs.iter().enumerate() {
        out.push(FatRun { start, n, next: runs.get(i + 1).map_or(EOC, |r| r.0 as u32), loose });
    }
}

struct Copy {
    src: u64,
    dst: u64,
    len: u64,
}

struct View {
    cs: u64,
    heap: u64,
    cc: u64,
    fat_off: u64,
    /// Some(bytes): the FAT region is new and all ours. None: entries are patched into the
    /// current FAT.
    fresh_fat: Option<u64>,
    /// Per object of the tree (directories: unused, see `dirs`).
    files: Vec<Place>,
    dirs: Vec<Place>,
    /// Directories that are written (the others stay as they are).
    emit: Vec<bool>,
    bitmap: Runs,
    upcase: Runs,
    write_upcase: bool,
    fat: Vec<FatRun>,
    copies: Vec<Copy>,
    /// The main boot sector that makes this view the volume.
    sector0: Vec<u8>,
    /// Sorted byte ranges [start, end) the writes may touch: free clusters, and the FAT.
    allowed: Vec<(u64, u64)>,
}

impl View {
    fn place<'a>(&'a self, t: &Tree, oi: usize) -> &'a Place {
        let o = &t.objs[oi];
        if o.is_dir { &self.dirs[o.dir as usize] } else { &self.files[oi] }
    }

    /// The allocation bitmap of this view: exactly what it references.
    fn bitmap_bytes(&self, t: &Tree) -> Result<Vec<u8>> {
        let mut b = cluster_set(self.cc);
        let all = [&self.bitmap, &self.upcase].into_iter();
        let all = all
            .chain(self.dirs.iter().map(|p| &p.runs))
            .chain(t.objs.iter().enumerate().filter(|(_, o)| !o.is_dir).map(|(oi, _)| &self.files[oi].runs));
        for runs in all {
            for &(s, n) in runs {
                ensure!(s >= 2 && s + n <= self.cc + 2, "internal error: planned cluster {s} outside the heap");
                mark_range(&mut b, s as u32, n).map_err(|c| anyhow!("internal error: cluster {c} planned twice"))?;
            }
        }
        Ok(b)
    }

    /// Directory `di` as this view holds it: its live entries with every child's first
    /// cluster, NoFatChain flag and (directories) size as placed, checksums redone, padded
    /// with zeros to its clusters.
    fn dir_bytes(&self, t: &Tree, di: usize) -> Result<Vec<u8>> {
        let d = &t.dirs[di];
        let mut out = d.content.clone();
        for &oi in &d.kids {
            let o = &t.objs[oi as usize];
            let p = self.place(t, oi as usize);
            let set = &mut out[o.at as usize..(o.at + o.set_len) as usize];
            if o.is_dir {
                let bytes = count(&p.runs) * self.cs;
                set[40..48].copy_from_slice(&bytes.to_le_bytes());
                set[56..64].copy_from_slice(&bytes.to_le_bytes());
            }
            if !p.runs.is_empty() {
                set[33] = (set[33] & !2) | if p.nfc { 2 } else { 0 };
            }
            set[52..56].copy_from_slice(&(p.runs.first().map_or(0, |r| r.0) as u32).to_le_bytes());
            let c = checksum16(set, true);
            set[2..4].copy_from_slice(&c.to_le_bytes());
        }
        if di == 0 {
            let b = &mut out[t.bitmap_at..t.bitmap_at + 32];
            b[20..24].copy_from_slice(&(self.bitmap[0].0 as u32).to_le_bytes());
            b[24..32].copy_from_slice(&self.cc.div_ceil(8).to_le_bytes());
            out[t.upcase_at + 20..t.upcase_at + 24].copy_from_slice(&(self.upcase[0].0 as u32).to_le_bytes());
        }
        let size = (count(&self.dirs[di].runs) * self.cs) as usize;
        ensure!(out.len() <= size, "internal error: a directory does not fit its clusters");
        out.resize(size, 0);
        Ok(out)
    }

    fn off(&self, c: u64) -> u64 {
        self.heap + (c - 2) * self.cs
    }
}

fn pct_in_use(bitmap: &[u8], cc: u64) -> u8 {
    let used: u64 = bitmap.iter().map(|b| b.count_ones() as u64).sum();
    (used * 100 / cc).min(100) as u8
}

fn merge_ranges(mut r: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    r.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(r.len());
    for (s, e) in r {
        match out.last_mut() {
            Some(l) if l.1 >= s => l.1 = l.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// Step 1, planned: the same cluster size, nothing left in the zone [2, 2 + kz).
fn plan_relocate(t: &Tree, kz: u64, cost: &Cost) -> Result<View> {
    let (cs, zend) = (t.cs, 2 + kz);
    let mut pool: VecDeque<(u64, u64)> = free_extents(&t.bitmap, t.cc)
        .into_iter()
        .filter_map(|(s, n)| {
            let from = s.max(zend);
            (from < s + n).then(|| (from, s + n - from))
        })
        .collect();
    let have: u64 = pool.iter().map(|e| e.1).sum();

    // What moves: files by the part of their chain that reaches into the zone, directories
    // whole, and every directory whose entries change with them, up to the root.
    let mut cut = vec![0u64; t.objs.len()];
    let mut dirty = vec![false; t.dirs.len()];
    dirty[0] = true;
    for (oi, o) in t.objs.iter().enumerate() {
        let m = zone_prefix(&o.runs, zend);
        if m > 0 && o.is_dir {
            dirty[o.dir as usize] = true;
        } else if m > 0 {
            cut[oi] = m;
            dirty[o.parent as usize] = true;
        }
    }
    for di in (1..t.dirs.len()).rev() {
        if dirty[di] {
            dirty[t.objs[t.dirs[di].obj.unwrap() as usize].parent as usize] = true;
        }
    }
    let dir_n = |di: usize| dir_clusters(t, di, cs);
    let bitmap_n = t.cc.div_ceil(8).div_ceil(cs);
    let upcase_moves = zone_prefix(&t.upcase_runs, zend) > 0;
    let need = bitmap_n
        + if upcase_moves { count(&t.upcase_runs) } else { 0 }
        + (0..t.dirs.len()).filter(|&d| dirty[d]).map(dir_n).sum::<u64>()
        + cut.iter().sum::<u64>();
    if need > have {
        return Err(refused(
            "no-space",
            format!(
                "not enough free space: {} must move out of the first {} of the volume to make room for the new FAT, \
                 and {} is free behind it",
                human_bytes((need * cs) as f64),
                human_bytes((kz * cs) as f64),
                human_bytes((have * cs) as f64)
            ),
        ));
    }
    // The bitmap and the up-case table must be one run each (drivers read them as such).
    let contiguous = |pool: &mut VecDeque<(u64, u64)>, n: u64, what: &str| -> Result<Runs> {
        let k = pool.iter().position(|e| e.1 >= n).ok_or_else(|| {
            refused(
                "no-contiguous-space",
                format!(
                    "the free space is too fragmented: no free run of {} for the {what}",
                    human_bytes((n * cs) as f64)
                ),
            )
        })?;
        let (s, l) = pool[k];
        if l == n {
            pool.remove(k);
        } else {
            pool[k] = (s + n, l - n);
        }
        Ok(vec![(s, n)])
    };
    let bitmap = contiguous(&mut pool, bitmap_n, "allocation bitmap")?;
    let upcase = if upcase_moves {
        contiguous(&mut pool, count(&t.upcase_runs), "up-case table")?
    } else {
        t.upcase_runs.clone()
    };
    let mut allowed: Vec<(u64, u64)> = Vec::new();
    let mut fat = Vec::new();
    chain(&bitmap, false, &mut fat);
    if upcase_moves {
        chain(&upcase, false, &mut fat);
    }
    let mut dirs = Vec::with_capacity(t.dirs.len());
    for (di, d) in t.dirs.iter().enumerate() {
        if !dirty[di] {
            dirs.push(Place { runs: d.runs.clone(), nfc: t.objs[d.obj.unwrap() as usize].nfc });
            continue;
        }
        let runs = match contiguous(&mut pool, dir_n(di), "") {
            Ok(r) => r,
            Err(_) => {
                let mut r = Runs::new();
                carve(&mut pool, dir_n(di)).into_iter().for_each(|(s, n)| push_run(&mut r, s, n));
                r
            }
        };
        if di == 0 || runs.len() > 1 {
            chain(&runs, false, &mut fat);
        }
        dirs.push(Place { nfc: runs.len() == 1, runs });
    }
    let mut files: Vec<Place> = t.objs.iter().map(|o| Place { runs: o.runs.clone(), nfc: o.nfc }).collect();
    let mut movers: Vec<usize> = (0..t.objs.len()).filter(|&oi| cut[oi] > 0).collect();
    movers.sort_by_key(|&oi| t.objs[oi].runs[0].0);
    let mut copies: Vec<Copy> = Vec::new();
    for oi in movers {
        let o = &t.objs[oi];
        let (head, tail) = split(&o.runs, cut[oi]);
        let mut dst = Runs::new();
        carve(&mut pool, cut[oi]).into_iter().for_each(|(s, n)| push_run(&mut dst, s, n));
        // Only the file's own bytes (to a 4 KB boundary): the rest of its last cluster is slack.
        let used = (count(&head) * cs).min(o.len.div_ceil(4096) * 4096);
        let (from, to) = (extents(&head, t.heap, cs, used), extents(&dst, t.heap, cs, used));
        let (mut a, mut b) = (from.into_iter(), to.into_iter());
        let (mut x, mut y) = (a.next(), b.next());
        while let (Some((so, sl)), Some((dof, dl))) = (x, y) {
            let n = sl.min(dl);
            // Read through the slack between two files when that is cheaper than a new request
            // (the gap is the tail of the previous file's last cluster on both sides).
            match copies.last_mut() {
                Some(c)
                    if so >= c.src + c.len
                        && so - (c.src + c.len) == dof.wrapping_sub(c.dst + c.len)
                        && so - (c.src + c.len) < cs
                        && (so == c.src + c.len || worth_filling(so - (c.src + c.len), cost.bps, cost.skip_s)) =>
                {
                    c.len = so + n - c.src
                }
                _ => copies.push(Copy { src: so, dst: dof, len: n }),
            }
            x = if sl > n { Some((so + n, sl - n)) } else { a.next() };
            y = if dl > n { Some((dof + n, dl - n)) } else { b.next() };
        }
        let mut runs = dst;
        tail.into_iter().for_each(|(s, n)| push_run(&mut runs, s, n));
        if runs.len() > 1 {
            chain(&runs, o.nfc, &mut fat);
        }
        files[oi] = Place { nfc: runs.len() == 1, runs };
    }
    for runs in [&bitmap, &upcase].into_iter().chain(dirs.iter().map(|p| &p.runs)).chain(files.iter().map(|p| &p.runs))
    {
        allowed.extend(runs.iter().map(|&(s, n)| (t.heap + (s - 2) * cs, t.heap + (s - 2 + n) * cs)));
    }
    // Only what was free may be written: drop everything that stays where it is.
    let allowed: Vec<(u64, u64)> = merge_ranges(allowed)
        .into_iter()
        .flat_map(|(s, e)| {
            let (c0, c1) = (2 + (s - t.heap) / cs, 2 + (e - t.heap) / cs);
            let mut free = Vec::new();
            let mut c = c0;
            while c < c1 {
                let Some(a) = find_bit(&t.bitmap, c, c1 - c, false) else { break };
                let b = find_bit(&t.bitmap, a, c1 - a, true).unwrap_or(c1);
                free.push((t.heap + (a - 2) * cs, t.heap + (b - 2) * cs));
                c = b;
            }
            free
        })
        .chain([(t.fat_off, t.fat_off + fat_bytes_for(t.cc))])
        .collect();
    let mut v = View {
        cs,
        heap: t.heap,
        cc: t.cc,
        fat_off: t.fat_off,
        fresh_fat: None,
        files,
        dirs,
        emit: dirty,
        bitmap,
        upcase,
        write_upcase: upcase_moves,
        fat,
        copies,
        sector0: t.sector0.clone(),
        allowed: merge_ranges(allowed),
    };
    v.sector0[96..100].copy_from_slice(&(v.dirs[0].runs[0].0 as u32).to_le_bytes());
    v.sector0[112] = pct_in_use(&v.bitmap_bytes(t)?, v.cc);
    Ok(v)
}

/// Step 2, planned: the new geometry. `cur` gives each file's clusters under the current one
/// (the tree's, or step 1's placement while only planning). The zone must be free.
fn plan_switch<'a>(t: &Tree, g: &Geo, l: &Layout, cur: impl Fn(usize) -> &'a [(u64, u64)]) -> Result<View> {
    let zend = 2 + l.kz;
    let mut fat = Vec::new();
    let bitmap = vec![(2, l.bitmap_n)];
    let upcase = vec![(2 + l.bitmap_n, l.upcase_n)];
    chain(&bitmap, false, &mut fat);
    chain(&upcase, false, &mut fat);
    chain(&[(l.dir_first[0], l.dir_n[0])], false, &mut fat);
    let dirs: Vec<Place> =
        l.dir_first.iter().zip(&l.dir_n).map(|(&s, &n)| Place { runs: vec![(s, n)], nfc: true }).collect();
    let mut files = Vec::with_capacity(t.objs.len());
    for (oi, o) in t.objs.iter().enumerate() {
        if o.is_dir {
            files.push(Place::default());
            continue;
        }
        let old = cur(oi);
        let mut left = o.len.div_ceil(g.cs);
        let mut runs = Runs::new();
        for &(s, n) in old {
            ensure!(s >= zend, "internal error: a file still has cluster {s} in the zone");
            let take = (n * g.ratio).min(left);
            push_run(&mut runs, 2 + (s - 2 - g.k) * g.ratio, take);
            left -= take;
        }
        ensure!(left == 0, "internal error: a file's clusters do not cover its length");
        // The whole point: the same bytes, at the same place on the device.
        let bytes = o.len.div_ceil(g.cs) * g.cs;
        ensure!(
            extents(&runs, g.heap, g.cs, bytes) == extents(old, t.heap, t.cs, bytes),
            "internal error: re-numbering would move a file"
        );
        if runs.len() > 1 {
            chain(&runs, false, &mut fat);
        }
        files.push(Place { nfc: if runs.is_empty() { o.nfc } else { runs.len() == 1 }, runs });
    }
    let mut v = View {
        cs: g.cs,
        heap: g.heap,
        cc: g.cc,
        fat_off: g.fat_off,
        fresh_fat: Some(g.fat_bytes),
        files,
        emit: vec![true; dirs.len()],
        dirs,
        bitmap,
        upcase,
        write_upcase: true,
        fat,
        copies: Vec::new(),
        sector0: t.sector0.clone(),
        allowed: vec![(t.heap, t.heap + l.kz * t.cs)],
    };
    let pct = pct_in_use(&v.bitmap_bytes(t)?, g.cc);
    let s = &mut v.sector0;
    s[80..84].copy_from_slice(&((g.fat_off / SECTOR) as u32).to_le_bytes());
    s[84..88].copy_from_slice(&((g.fat_bytes / SECTOR) as u32).to_le_bytes());
    s[88..92].copy_from_slice(&((g.heap / SECTOR) as u32).to_le_bytes());
    s[92..96].copy_from_slice(&(g.cc as u32).to_le_bytes());
    s[96..100].copy_from_slice(&(l.dir_first[0] as u32).to_le_bytes());
    s[109] = (g.cs / SECTOR).trailing_zeros() as u8;
    s[112] = pct;
    Ok(v)
}

// ───────────────────────── writing ─────────────────────────

/// What the device costs, for the plan's estimate and the copy's gap decisions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cost {
    bps: f64,
    skip_s: f64,
    read_bps: f64,
}

impl Cost {
    /// From the device's cached profile; the defaults of `build`'s optimizer without one.
    fn of(f: &File) -> Cost {
        let p = crate::devprofile::block_dev_of(f).and_then(crate::devprofile::load).unwrap_or_default();
        let (bps, skip_s) = p.write_cost();
        Cost { bps, skip_s, read_bps: p.seq_read_bps.unwrap_or(2.0 * bps) }
    }
}

/// All writes of a step go through here: checked against the ranges the plan may touch,
/// adjacent ones merged into requests of up to [`MAX_REQ`], counted. `dry`: only counted.
struct Out<'a> {
    dev: &'a dyn Dev,
    dry: bool,
    allowed: Vec<(u64, u64)>,
    at: u64,
    pending: Vec<u8>,
    /// Bytes pending (the buffer stays empty when dry).
    len: u64,
    unsynced: u64,
    written: u64,
    requests: u64,
    read: u64,
    syncs: u64,
    progress: &'a mut dyn FnMut(u64),
}

impl<'a> Out<'a> {
    fn new(dev: &'a dyn Dev, dry: bool, progress: &'a mut dyn FnMut(u64)) -> Self {
        Out {
            dev,
            dry,
            allowed: Vec::new(),
            at: 0,
            pending: Vec::new(),
            len: 0,
            unsynced: 0,
            written: 0,
            requests: 0,
            read: 0,
            syncs: 0,
            progress,
        }
    }

    /// `len` bytes at `off`: `data` (which may be missing only when dry).
    fn put(&mut self, off: u64, len: u64, data: Option<&[u8]>) -> Result<()> {
        if !(self.len > 0 && off == self.at + self.len && self.len + len <= MAX_REQ) {
            self.flush()?;
            self.at = off;
        }
        self.len += len;
        if !self.dry {
            let d = data.ok_or_else(|| anyhow!("internal error: a write without data"))?;
            debug_assert_eq!(d.len() as u64, len);
            self.pending.extend_from_slice(d);
        }
        if self.len >= MAX_REQ {
            self.flush()?;
        }
        Ok(())
    }

    fn write(&mut self, off: u64, data: &[u8]) -> Result<()> {
        self.put(off, data.len() as u64, Some(data))
    }

    /// `data` over the clusters `runs` of `view`.
    fn write_runs(&mut self, view: &View, runs: &[(u64, u64)], data: &[u8]) -> Result<()> {
        let mut done = 0usize;
        for &(s, n) in runs {
            let len = ((n * view.cs) as usize).min(data.len() - done);
            let mut at = view.off(s);
            for part in data[done..done + len].chunks(MAX_REQ as usize) {
                self.write(at, part)?;
                at += part.len() as u64;
            }
            done += len;
        }
        ensure!(done == data.len(), "internal error: data does not fit its clusters");
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let (off, end) = (self.at, self.at + self.len);
        let k = self.allowed.partition_point(|r| r.1 <= off);
        ensure!(
            self.allowed.get(k).is_some_and(|r| r.0 <= off && end <= r.1),
            "internal error: a write at byte {off} (+{}) is outside the free space planned for it; stopped",
            self.len
        );
        if self.dry {
            self.requests += self.len.div_ceil(MAX_REQ);
        } else {
            let mut o = off;
            for part in self.pending.chunks(MAX_REQ as usize) {
                self.dev.write_at(part, o).with_context(|| format!("write {} bytes at {o}", part.len()))?;
                o += part.len() as u64;
                self.requests += 1;
            }
            self.pending.clear();
        }
        self.written += self.len;
        self.unsynced += self.len;
        (self.progress)(self.len);
        self.len = 0;
        if self.unsynced >= SYNC_EVERY {
            self.sync()?;
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.flush()?;
        if !self.dry {
            self.dev.sync().context("sync")?;
        }
        self.syncs += 1;
        self.unsynced = 0;
        Ok(())
    }
}

/// The 4 KB pages of a FAT that hold an entry of `runs` (sorted by cluster), as merged
/// (first page, pages).
fn fat_pages(runs: &[FatRun], with_first: bool) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = if with_first { vec![(0, 1)] } else { Vec::new() };
    for r in runs {
        let (a, b) = (r.start * 4 / FAT_PAGE, ((r.start + r.n) * 4 - 1) / FAT_PAGE + 1);
        match out.last_mut() {
            Some(l) if l.0 + l.1 >= a => l.1 = l.1.max(b - l.0),
            _ => out.push((a, b - a)),
        }
    }
    out
}

/// Puts the entries of `runs` (sorted) that fall into `buf`, whose first entry is cluster
/// `first`. `changing(cluster, loose)` is asked before an entry gets another value.
fn fat_fill(
    buf: &mut [u8],
    first: u64,
    runs: &[FatRun],
    changing: &mut dyn FnMut(u64, bool) -> Result<()>,
) -> Result<()> {
    let end = first + buf.len() as u64 / 4;
    for r in &runs[runs.partition_point(|r| r.start + r.n <= first)..] {
        if r.start >= end {
            break;
        }
        for c in r.start.max(first)..(r.start + r.n).min(end) {
            let v = if c + 1 == r.start + r.n { r.next } else { c as u32 + 1 };
            let at = ((c - first) * 4) as usize;
            if le32(buf, at) != v {
                changing(c, r.loose)?;
                buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
    }
    Ok(())
}

/// Writes everything `view` needs except its boot sector. With a dry `out`, nothing is read
/// or written, only counted.
fn apply(t: &Tree, view: &View, fat_mode: FatMode, out: &mut Out<'_>) -> Result<()> {
    out.allowed = view.allowed.clone();
    let dev = out.dev;
    let mut fat = view.fat.clone();
    fat.sort_unstable_by_key(|r| r.start);
    let in_use = fat_bytes_for(view.cc);
    if let Some(fat_bytes) = view.fresh_fat {
        // A new FAT: media type and end-of-chain in entries 0 and 1, the chains, zeros.
        let ranges = match fat_mode {
            FatMode::Full => vec![(0, fat_bytes.div_ceil(FAT_PAGE))],
            FatMode::Prefix => fat_pages(&fat, true),
        };
        for (p0, n) in ranges {
            let (mut off, end) = (p0 * FAT_PAGE, ((p0 + n) * FAT_PAGE).min(fat_bytes));
            while off < end {
                let len = (end - off).min(MAX_REQ);
                if out.dry {
                    out.put(view.fat_off + off, len, None)?;
                } else {
                    let mut buf = vec![0u8; len as usize];
                    fat_fill(&mut buf, off / 4, &fat, &mut |_, _| Ok(()))?;
                    if off == 0 {
                        buf[..8].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
                    }
                    out.write(view.fat_off + off, &buf)?;
                }
                off += len;
            }
        }
    }
    for c in &view.copies {
        let mut done = 0;
        while done < c.len {
            let len = (c.len - done).min(MAX_REQ);
            if out.dry {
                out.put(c.dst + done, len, None)?;
            } else {
                let mut buf = vec![0u8; len as usize];
                dev.read_at(&mut buf, c.src + done).with_context(|| format!("read {len} bytes at {}", c.src + done))?;
                out.write(c.dst + done, &buf)?;
            }
            out.read += len;
            done += len;
        }
    }
    let pad = |mut b: Vec<u8>, runs: &Runs| {
        b.resize((count(runs) * view.cs) as usize, 0);
        b
    };
    out.write_runs(view, &view.bitmap, &pad(view.bitmap_bytes(t)?, &view.bitmap))?;
    if view.write_upcase {
        out.write_runs(view, &view.upcase, &pad(t.upcase_raw.clone(), &view.upcase))?;
    }
    for di in (0..t.dirs.len()).filter(|&d| view.emit[d]) {
        out.write_runs(view, &view.dirs[di].runs, &view.dir_bytes(t, di)?)?;
    }
    if view.fresh_fat.is_none() {
        // Entries patched into the FAT in use. An entry may only change if the current volume
        // ignores it: its cluster is free there, or belongs to a NoFatChain object.
        for (p0, n) in fat_pages(&fat, false) {
            let (mut off, end) = (p0 * FAT_PAGE, ((p0 + n) * FAT_PAGE).min(in_use));
            while off < end {
                let len = (end - off).min(MAX_REQ);
                if out.dry {
                    out.put(view.fat_off + off, len, None)?;
                } else {
                    let mut buf = vec![0u8; len as usize];
                    dev.read_at(&mut buf, view.fat_off + off).context("read the FAT")?;
                    let mut check = |c: u64, loose: bool| -> Result<()> {
                        ensure!(
                            loose || !bit(&t.bitmap, c),
                            "internal error: FAT entry {c} is in use and would change; stopped"
                        );
                        Ok(())
                    };
                    fat_fill(&mut buf, off / 4, &fat, &mut check)?;
                    out.write(view.fat_off + off, &buf)?;
                }
                off += len;
            }
        }
    }
    out.sync()
}

/// Bytes of a FAT with entries for `cc` clusters, rounded up to sectors.
fn fat_bytes_for(cc: u64) -> u64 {
    ((cc + 2) * 4).div_ceil(SECTOR) * SECTOR
}

// ───────────────────────── the switch ─────────────────────────

/// Sets four bytes of the boot code area in sector 0 of `region` (the 11 checksummed sectors)
/// so that the region's checksum is `target`: the sector then matches the checksum sector that
/// is already on the medium. The bytes are 8 apart, so each steers one byte of the 32-bit sum;
/// a meet-in-the-middle search over 2 × 65536 states finds them. Several positions are tried
/// (a given one has a solution for most targets, not all). False: no position worked.
fn neutral(region: &mut [u8], target: u32) -> bool {
    // Behind the job id `optimize` keeps at 120.., and below 440: where an MBR has its disk
    // signature and partition table (440..510), a volume without a partition table must keep
    // what it has, or Windows takes four stray bytes for partition entries and shows no volume.
    for base in (160..=NEUTRAL_TOP - 24).rev() {
        let q = [base, base + 8, base + 16, base + 24];
        if let Some(v) = solve(region, q, target) {
            for (p, b) in q.into_iter().zip(v) {
                region[p] = b;
            }
            debug_assert_eq!(checksum32(region, &[106, 107, 112], 0), target);
            return true;
        }
    }
    false
}

fn solve(region: &[u8], q: [usize; 4], target: u32) -> Option<[u8; 4]> {
    let step = |c: u32, b: u8| c.rotate_right(1).wrapping_add(b as u32);
    let back = |c: u32, b: u8| c.wrapping_sub(b as u32).rotate_left(1);
    let run = |c: u32, r: std::ops::Range<usize>| region[r].iter().fold(c, |c, &b| step(c, b));
    let undo = |c: u32, r: std::ops::Range<usize>| region[r].iter().rev().fold(c, |c, &b| back(c, b));
    // The bytes the checksum skips (106, 107, 112) all lie before q[0].
    let start = checksum32(&region[..q[0]], &[106, 107, 112], 0);
    // Forward: the state just before byte q[2], for every value of bytes q[0] and q[1].
    let mut fwd: HashMap<u32, (u8, u8)> = HashMap::with_capacity(1 << 16);
    for a in 0..=255u8 {
        let s = run(step(start, a), q[0] + 1..q[1]);
        for b in 0..=255u8 {
            fwd.entry(run(step(s, b), q[1] + 1..q[2])).or_insert((a, b));
        }
    }
    // Backward from the target: the same state, for every value of bytes q[3] and q[2].
    let after = undo(target, q[3] + 1..region.len());
    for d in 0..=255u8 {
        let s = undo(back(after, d), q[2] + 1..q[3]);
        for c in 0..=255u8 {
            if let Some(&(a, b)) = fwd.get(&back(s, c)) {
                return Some([a, b, c, d]);
            }
        }
    }
    None
}

/// The boot regions as they are on the medium: main (0..12) and backup (12..24).
fn read_boot(f: &File) -> Result<Vec<u8>> {
    let mut b = vec![0u8; 24 * SECTOR as usize];
    f.read_exact_at(&mut b, 0).context("read the boot regions")?;
    Ok(b)
}

/// The state of the backup boot region against the main one.
#[derive(Debug, PartialEq, Eq)]
enum Backup {
    Same,
    /// What a crash between the two writes of a switch leaves: only sector 12 differs, and the
    /// backup region is valid by itself (it describes the volume before the switch).
    SwitchPending,
}

fn same_boot_sector(a: &[u8], b: &[u8]) -> bool {
    // VolumeFlags and PercentInUse change in the main sector alone, and no checksum covers them.
    (0..SECTOR as usize).all(|i| a[i] == b[i] || matches!(i, 106 | 107 | 112))
}

fn backup_state(boot: &[u8]) -> Result<Backup> {
    let s = SECTOR as usize;
    let (main, backup) = boot.split_at(12 * s);
    let mismatch = |what: &str| {
        refused(
            "backup-boot-mismatch",
            format!(
                "the backup boot region does not match the main one ({what}); run fsck.exfat (nothing was written)"
            ),
        )
    };
    if main[s..] != backup[s..] {
        return Err(mismatch("sectors 13 to 23"));
    }
    if same_boot_sector(main, backup) {
        return Ok(Backup::Same);
    }
    let sum = checksum32(&backup[..11 * s], &[106, 107, 112], 0);
    if backup[11 * s..].as_chunks::<4>().0.iter().any(|w| u32::from_le_bytes(*w) != sum) {
        return Err(mismatch("sector 12, which its checksum does not cover"));
    }
    Ok(Backup::SwitchPending)
}

/// The raw checks that give a precise reason before the parser gives a general one.
fn precheck(boot: &[u8]) -> Result<()> {
    if &boot[3..11] != b"EXFAT   " {
        return Err(refused("not-exfat", "the target does not hold an exFAT volume"));
    }
    if boot[108] != 9 {
        return Err(refused(
            "unsupported-sector-size",
            format!(
                "the volume has {}-byte sectors; recluster handles 512-byte sectors only",
                1u64 << boot[108].min(40)
            ),
        ));
    }
    if le16(boot, 106) & FLAG_DIRTY != 0 {
        return Err(refused(
            "dirty",
            "the volume is marked dirty (still mounted, or not cleanly unmounted); unmount it or run fsck.exfat first",
        ));
    }
    Ok(())
}

fn drop_cache(f: &File, ranges: &[(u64, u64)]) {
    use std::os::fd::AsRawFd;
    for (s, e) in ranges.iter().copied().chain([(0, 24 * SECTOR)]) {
        // Whole pages only; best effort.
        let (a, b) = (s / 4096 * 4096, e.div_ceil(4096) * 4096);
        unsafe { libc::posix_fadvise(f.as_raw_fd(), a as i64, (b - a) as i64, libc::POSIX_FADV_DONTNEED) };
    }
}

fn invalid(e: anyhow::Error) -> anyhow::Error {
    if refusal_of(&e).is_some() { e } else { refused("invalid-volume", format!("{e:#}")) }
}

/// Opens and walks the volume on `f` as it is.
fn open_tree(f: &File, dev_len: u64) -> Result<Tree> {
    let mut v = Vol::open(f.try_clone().context("dup the device handle")?, dev_len, false, true)?;
    load(&mut v)
}

/// Reads back what `view` will be, through the boot sector about to be written and the
/// validating parser, from the medium (the written ranges are dropped from the page cache
/// first), and compares it with the plan: geometry, bitmap bit for bit, every directory entry
/// for entry, every allocation. Returns the tree as the view holds it.
fn verify(f: &File, dev_len: u64, t: &Tree, view: &View) -> Result<Tree> {
    let sources: Vec<(u64, u64)> = view.copies.iter().map(|c| (c.src, c.src + c.len)).collect();
    drop_cache(f, &view.allowed);
    drop_cache(f, &sources);
    // The file data copied out of the way must be what it was copied from (the original is
    // still there: it is overwritten only after this switch, by the next step).
    let (mut a, mut b) = (vec![0u8; MAX_REQ as usize], vec![0u8; MAX_REQ as usize]);
    for c in &view.copies {
        let mut done = 0;
        while done < c.len {
            let n = (c.len - done).min(MAX_REQ) as usize;
            f.read_exact_at(&mut a[..n], c.src + done).context("read back the moved data")?;
            f.read_exact_at(&mut b[..n], c.dst + done).context("read back the moved data")?;
            ensure!(a[..n] == b[..n], "file data moved to byte {} did not arrive intact; nothing was switched", c.dst);
            done += n as u64;
        }
    }
    // Every directory written, byte for byte (also the zeros after its last entry).
    for di in (0..t.dirs.len()).filter(|&d| view.emit[d]) {
        let want = view.dir_bytes(t, di)?;
        let mut got = vec![0u8; want.len()];
        let mut at = 0;
        for &(s, n) in &view.dirs[di].runs {
            let len = (n * view.cs) as usize;
            f.read_exact_at(&mut got[at..at + len], view.off(s)).context("read back a directory")?;
            at += len;
        }
        ensure!(got == want, "a directory written did not arrive intact; nothing was switched");
    }
    let mut region = read_boot(f)?;
    region.truncate(12 * SECTOR as usize);
    region[..SECTOR as usize].copy_from_slice(&view.sector0);
    let v = Vol::from_boot_region(f.try_clone().context("dup the device handle")?, dev_len, &region, false, false)
        .context("the boot sector to be written")?;
    let mut v = Vol::open_rest(v, true).context("the prepared volume")?;
    let n = load(&mut v).context("the prepared volume")?;
    let same = n.cs == view.cs
        && n.heap == view.heap
        && n.cc == view.cc
        && n.fat_off == view.fat_off
        && n.label == t.label
        && n.upc == t.upc
        && n.upcase_raw == t.upcase_raw
        && n.bitmap == view.bitmap_bytes(t)?
        && n.bitmap_runs == view.bitmap
        && n.upcase_runs == view.upcase
        && n.dirs.len() == t.dirs.len()
        && n.objs.len() == t.objs.len()
        && n.files == t.files
        && n.data_bytes == t.data_bytes;
    ensure!(same, "internal error: the prepared volume's geometry, bitmap or totals differ from the plan");
    for (di, (a, b)) in t.dirs.iter().zip(&n.dirs).enumerate() {
        ensure!(
            b.runs == view.dirs[di].runs && a.kids == b.kids && comparable(t, a, di) == comparable(&n, b, di),
            "internal error: a prepared directory differs from the plan"
        );
    }
    for (oi, (a, b)) in t.objs.iter().zip(&n.objs).enumerate() {
        let p = view.place(t, oi);
        ensure!(
            a.is_dir == b.is_dir
                && a.parent == b.parent
                && (a.at, a.set_len) == (b.at, b.set_len)
                && (a.is_dir || a.len == b.len)
                && b.runs == p.runs
                && (p.runs.is_empty() || b.nfc == p.nfc),
            "internal error: a prepared file or directory differs from the plan"
        );
    }
    Ok(n)
}

/// A directory's content with the fields a re-addressing changes blanked: FirstCluster, the
/// NoFatChain bit, the set checksum, a directory's size, the bitmap's and up-case table's
/// place. Everything else (names, attributes, timestamps, lengths, benign entries) must be
/// kept bit for bit.
fn comparable(t: &Tree, d: &DirNode, di: usize) -> Vec<u8> {
    let mut c = d.content.clone();
    for &oi in &d.kids {
        let o = &t.objs[oi as usize];
        let set = &mut c[o.at as usize..(o.at + o.set_len) as usize];
        set[2..4].fill(0);
        set[33] &= !2;
        set[52..56].fill(0);
        if o.is_dir {
            set[40..48].fill(0);
            set[56..64].fill(0);
        }
    }
    if di == 0 {
        c[t.bitmap_at + 20..t.bitmap_at + 32].fill(0);
        c[t.upcase_at + 20..t.upcase_at + 24].fill(0);
    }
    c
}

/// Tunes the boot code bytes of `view`'s sector 0 so that it has the boot checksum that is on
/// the medium (see [`neutral`]). Done while planning: a volume without such bytes is refused
/// before anything is written.
fn tune(f: &File, view: &mut View) -> Result<()> {
    let boot = read_boot(f)?;
    let s = SECTOR as usize;
    let mut region = boot[..11 * s].to_vec();
    region[..s].copy_from_slice(&view.sector0);
    if !neutral(&mut region, le32(&boot, 11 * s)) {
        return Err(refused(
            "checksum",
            "no boot code bytes keep this volume's boot checksum unchanged, so the switch could not be one sector",
        ));
    }
    view.sector0.copy_from_slice(&region[..s]);
    Ok(())
}

/// Makes `view` the volume: its (tuned) sector 0, a sync, then the same sector as backup.
fn switch(f: &File, dev: &dyn Dev, view: &View, name: &str, on_main: &mut dyn FnMut()) -> Result<()> {
    let boot = read_boot(f)?;
    let s = SECTOR as usize;
    let mut region = boot[..11 * s].to_vec();
    region[..s].copy_from_slice(&view.sector0);
    ensure!(
        checksum32(&region, &[106, 107, 112], 0) == le32(&boot, 11 * s),
        "internal error: the new boot sector does not have the boot checksum; nothing was switched"
    );
    dev.write_at(&view.sector0, 0).context("write the main boot sector")?;
    dev.sync().context("sync")?;
    on_main();
    // The medium must still be the one just switched: else the backup write would put this
    // volume's boot sector onto another one. Stopping here leaves this volume in the normal
    // "backup owed" state that the next run completes.
    if !fresh_sector0(f).is_ok_and(|now| now == view.sector0) {
        return Err(refused(
            "wrong-volume",
            "the medium changed right after the switch (its boot sector is not the one just written); the backup \
             boot sector was not written, run flashcp recluster on the original drive to complete it",
        ));
    }
    #[cfg(feature = "test-hooks")]
    if std::env::var(STOP_ENV).as_deref() == Ok(name) {
        bail!("stopped by {STOP_ENV} after the main boot sector of the {name} switch");
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = name;
    dev.write_at(&view.sector0, 12 * SECTOR).context("write the backup boot sector")?;
    dev.sync().context("sync")
}

// ───────────────────────── the command ─────────────────────────

#[derive(Default)]
struct Totals {
    written: u64,
    requests: u64,
    read: u64,
    syncs: u64,
}

/// Counts what [`apply`] and the switch of `view` will do, without reading or writing.
fn count_writes(t: &Tree, view: &View, fat: FatMode, sum: &mut Totals) -> Result<()> {
    struct Nowhere;
    impl Dev for Nowhere {
        fn read_at(&self, _: &mut [u8], _: u64) -> std::io::Result<()> {
            Err(std::io::Error::other("planning reads nothing"))
        }
        fn write_at(&self, _: &[u8], _: u64) -> std::io::Result<()> {
            Err(std::io::Error::other("planning writes nothing"))
        }
        fn sync(&self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut nop = |_: u64| {};
    let mut out = Out::new(&Nowhere, true, &mut nop);
    apply(t, view, fat, &mut out)?;
    sum.written += out.written + 2 * SECTOR;
    sum.requests += out.requests + 2;
    sum.read += out.read;
    sum.syncs += out.syncs + 2;
    Ok(())
}

struct Run<'a> {
    f: &'a File,
    dev_len: u64,
    dev: &'a dyn Dev,
    fat: FatMode,
    guard: &'a dyn Fn() -> Result<()>,
    /// The main boot sector the medium must hold now (what the last switch wrote).
    expected: &'a std::cell::RefCell<Vec<u8>>,
    sink: &'a mut dyn FnMut(&Event),
    done: u64,
    total: u64,
    last: Instant,
}

impl Run<'_> {
    /// One step: write `view` into free space, sync, check it, switch. Returns the volume's
    /// tree as it is now.
    fn step(&mut self, t: &Tree, view: &View, name: &'static str) -> Result<Tree> {
        (self.guard)()?;
        {
            let (done, total, last, sink) = (&mut self.done, self.total, &mut self.last, &mut *self.sink);
            let mut progress = |n: u64| {
                *done += n;
                if last.elapsed().as_millis() >= 100 {
                    *last = Instant::now();
                    sink(&Event::Progress { bytes_done: (*done).min(total), bytes_total: total });
                }
            };
            let mut out = Out::new(self.dev, false, &mut progress);
            apply(t, view, self.fat, &mut out)?;
        }
        let n = verify(self.f, self.dev_len, t, view)
            .context("the prepared volume failed its check; nothing was switched, the volume is as it was")?;
        (self.guard)()?;
        let sink = &mut *self.sink;
        switch(self.f, self.dev, view, name, &mut || sink(&Event::Switched { step: name, cluster: view.cs }))?;
        *self.expected.borrow_mut() = view.sector0.to_vec();
        self.done += 2 * SECTOR;
        Ok(n)
    }
}

/// `recluster` on an opened device: `f` reads, `dev` writes (the same file). `guard` runs
/// before every step that writes.
pub(crate) fn run_on(
    f: &File,
    dev_len: u64,
    dev: &dyn Dev,
    o: &ReclusterOpts,
    cost: &Cost,
    guard: &dyn Fn() -> Result<()>,
    sink: &mut dyn FnMut(&Event),
) -> Result<Stats> {
    let t0 = Instant::now();
    let boot = read_boot(f).map_err(invalid)?;
    precheck(&boot)?;
    // Before anything is written, on the same claimed fd: is this still the volume, and the
    // physical device the user was asked about?
    if let Some(want) = &o.expect_hw {
        let got = hw_serial_of(f);
        if got.as_deref() != Some(want.as_str()) {
            return Err(refused(
                "wrong-volume",
                format!(
                    "this is not the drive that was checked (hardware serial {}, expected {want}); nothing was \
                     written",
                    got.as_deref().unwrap_or("unreadable")
                ),
            ));
        }
    }
    if let Some(want) = o.expect_serial {
        let got = boot_serial(&boot);
        if got != want {
            return Err(refused(
                "wrong-volume",
                format!(
                    "this is not the volume that was checked (serial {:04X}-{:04X}, expected {:04X}-{:04X}); \
                     nothing was written",
                    got >> 16,
                    got & 0xffff,
                    want >> 16,
                    want & 0xffff
                ),
            ));
        }
    }
    // Planning takes a while, and the fd stays the same while the medium behind it may not (a
    // card in a reader): before every phase that writes, the main boot sector read again
    // through the fd (bypassing the page cache on a block device) must be the one this run
    // planned from, or the one its last switch wrote.
    let expected = std::cell::RefCell::new(boot[..SECTOR as usize].to_vec());
    let medium = || -> Result<()> {
        let Ok(now) = fresh_sector0(f) else {
            return Err(refused(
                "wrong-volume",
                "the boot sector cannot be read again past the cache, so a changed medium could not be noticed; \
                 nothing further was written",
            ));
        };
        let same = {
            let want = expected.borrow();
            (0..SECTOR as usize).all(|i| now[i] == want[i] || matches!(i, 106 | 107 | 112))
        };
        if !same || o.expect_serial.is_some_and(|s| boot_serial(&now) != s) {
            return Err(refused(
                "wrong-volume",
                "the medium changed while recluster ran (its boot sector is not the one this run planned \
                 from); nothing further was written",
            ));
        }
        Ok(())
    };
    let guard = &|| -> Result<()> {
        guard()?;
        medium()
    };
    let pending = backup_state(&boot)? == Backup::SwitchPending;
    let mut t = open_tree(f, dev_len).map_err(invalid)?;
    // The backup boot sector an interrupted switch still owes. The main region has just passed
    // validation, so the backup follows it, whatever this run decides next (also a refusal: a
    // backup that describes a previous volume must not stay behind).
    let finish = pending && !o.dry_run;
    if finish {
        guard()?;
        dev.write_at(&boot[..SECTOR as usize], 12 * SECTOR).context("write the backup boot sector")?;
        dev.sync().context("sync")?;
    }
    // What follows, in a closure: a refusal after that backup write must say it happened.
    let mut rest = || -> Result<Stats> {
        let (old_cs, free_before) = (t.cs, free_bytes(&t.bitmap, t.cc, t.cs));
        let stats = |changed: bool, cluster: u64, written: u64, free_after: u64, finished_switch: bool| Stats {
            changed,
            old_cluster: old_cs,
            cluster,
            written,
            secs: t0.elapsed().as_secs_f64(),
            free_before,
            free_after,
            finished_switch,
        };
        let Some(g) = target_geometry(t.vol_bytes, t.heap, t.cs, t.cc, o.cluster)? else {
            let s = stats(false, old_cs, if finish { SECTOR } else { 0 }, free_before, finish);
            sink(&Event::Done(s.clone()));
            return Ok(s);
        };
        let l = layout(&t, &g)?;
        let relocate = find_bit(&t.bitmap, 2, l.kz, true).is_some();

        // ── both steps are planned (and every refusal decided) before the first write ──
        let mut sum = Totals::default();
        let mut step1 = None;
        let mut step2;
        if relocate {
            let mut v1 = plan_relocate(&t, l.kz, cost)?;
            tune(f, &mut v1)?;
            count_writes(&t, &v1, o.fat, &mut sum)?;
            step2 = plan_switch(&t, &g, &l, |oi| &v1.files[oi].runs)?;
            step1 = Some(v1);
        } else {
            step2 = plan_switch(&t, &g, &l, |oi| &t.objs[oi].runs)?;
        }
        tune(f, &mut step2)?;
        count_writes(&t, &step2, o.fat, &mut sum)?;
        let free_after = free_bytes(&step2.bitmap_bytes(&t)?, g.cc, g.cs);
        let est_secs = 0.3
            + sum.written as f64 / cost.bps
            + sum.requests as f64 * cost.skip_s
            + sum.read as f64 / cost.read_bps
            + sum.syncs as f64 * 0.02;
        let move_bytes = step1.as_ref().map_or(0, |v| v.copies.iter().map(|c| c.len).sum());
        let detail = format!(
            "plan: clusters {} → {} │ {} files, {} dirs, {} of data stay where they are{} │ will write {} (new FAT, \
             bitmap, directories), about {:.0} s │ free space {} → {} │ small files will need smaller writes",
            kb(old_cs),
            kb(g.cs),
            t.files,
            t.dirs.len() - 1,
            human_bytes(t.data_bytes as f64),
            if relocate {
                format!(", except {} that make room for the new FAT", human_bytes(move_bytes as f64))
            } else {
                String::new()
            },
            human_bytes(sum.written as f64),
            est_secs.ceil(),
            human_bytes(free_before as f64),
            human_bytes(free_after as f64),
        );
        sink(&Event::Plan(Plan {
            old_cluster: old_cs,
            cluster: g.cs,
            files: t.files,
            dirs: t.dirs.len() as u64 - 1,
            data_bytes: t.data_bytes,
            write_bytes: sum.written,
            move_bytes,
            est_secs,
            free_before,
            free_after,
            fragmented: t.objs.iter().filter(|o| !o.is_dir && o.runs.len() > 1).count() as u64,
            relocate,
            detail,
        }));
        if o.dry_run {
            return Ok(stats(false, g.cs, 0, free_after, false));
        }

        // ── write ──
        let mut run = Run {
            f,
            dev_len,
            dev,
            fat: o.fat,
            guard,
            expected: &expected,
            sink,
            done: 0,
            total: sum.written,
            last: Instant::now(),
        };
        if let Some(v1) = step1 {
            // Step 2 stays as planned: the check before the switch has just shown that the medium
            // holds exactly step 1's plan, which step 2 was planned from (and tuned for: sectors 1
            // to 11 do not change).
            t = run.step(&t, &v1, "relocated")?;
        }
        ensure!(
            find_bit(&t.bitmap, 2, l.kz, true).is_none(),
            "internal error: the zone is not free; nothing was switched"
        );
        run.step(&t, &step2, "reclustered")?;
        let written = run.done + if finish { SECTOR } else { 0 };
        (run.sink)(&Event::Progress { bytes_done: run.total.max(run.done), bytes_total: run.total.max(run.done) });
        // What every other command sees from now on.
        drop_cache(f, &[]);
        let after = Vol::open(f.try_clone().context("dup the device handle")?, dev_len, false, true)
            .context("the volume after the switch")?;
        let s = stats(true, after.cs, written, after.free_clusters() * after.cs, finish);
        (run.sink)(&Event::Done(s.clone()));
        Ok(s)
    };
    rest().map_err(|e| match e.downcast::<Refused>() {
        Ok(r) if finish => refused(
            r.reason,
            format!("{} (only the backup boot sector of an interrupted recluster was completed)", r.message),
        ),
        Ok(r) => anyhow::Error::new(r),
        Err(e) => e,
    })
}

fn free_bytes(bitmap: &[u8], cc: u64, cs: u64) -> u64 {
    let used: u64 = (0..cc / 8).map(|i| bitmap[i as usize].count_ones() as u64).sum::<u64>()
        + (cc / 8 * 8..cc).filter(|&i| bitmap[(i / 8) as usize] >> (i % 8) & 1 == 1).count() as u64;
    (cc - used) * cs
}

/// `flashcp recluster`: see the module docs. The target is opened once, exclusively (O_EXCL on
/// a block device, flock on an image), like every command that writes.
pub fn recluster(target: &TargetSpec, o: &ReclusterOpts, sink: &mut dyn FnMut(&Event)) -> Result<Stats> {
    let tgt = target.open_rw()?;
    // Metadata-sized, unaligned I/O through the page cache; every step ends in a sync.
    set_direct(&tgt.file, false);
    let cost = Cost::of(&tgt.file);
    let image = tgt.file.metadata().context("fstat target")?.is_file();
    let file = Arc::new(tgt.file.try_clone().context("dup target fd")?);
    // A loop device ignores the flock on an image: look again before every step that writes.
    let guard = || -> Result<()> {
        if !image {
            return Ok(());
        }
        match crate::add::loop_device_of(&file.metadata().context("fstat target")?) {
            Some(d) => {
                bail!("the image was attached to loop device {d} while recluster ran (it may be mounted); stopped")
            }
            None => Ok(()),
        }
    };
    let dev = BootDirect::new(FileDev::new(file.clone()), &file, !image);
    run_on(&file, tgt.bytes, &dev, o, &cost, &guard, sink)
}

/// The device for a run: buffered I/O, except that the one-sector boot sector writes go
/// through an O_DIRECT handle on a block device. Through the page cache they would reach the
/// device as a whole 4 KiB cache block (LBAs 0..8, or 8..16 for the backup, which holds main
/// sectors 8..11 too); this way each is the 512-byte write the crash argument counts on.
struct BootDirect {
    inner: FileDev,
    direct: Option<File>,
}

impl BootDirect {
    fn new(inner: FileDev, f: &File, block: bool) -> BootDirect {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let direct = block
            .then(|| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(format!("/proc/self/fd/{}", f.as_raw_fd()))
                    .ok()
            })
            .flatten();
        BootDirect { inner, direct }
    }
}

impl Dev for BootDirect {
    fn read_at(&self, buf: &mut [u8], off: u64) -> std::io::Result<()> {
        self.inner.read_at(buf, off)
    }
    fn write_at(&self, buf: &[u8], off: u64) -> std::io::Result<()> {
        if let Some(d) = &self.direct
            && buf.len() == SECTOR as usize
            && (off == 0 || off == 12 * SECTOR)
        {
            let mut b = crate::util::AlignedBuf::new(SECTOR as usize);
            b.as_mut().copy_from_slice(buf);
            let r = crate::sim::trace::serial(crate::sim::trace::Op::Write, off, SECTOR, || {
                d.write_all_at(b.as_ref(), off)
            });
            // EINVAL: a device whose logical blocks are bigger than 512 bytes. Nothing was written.
            match r {
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
                r => return r,
            }
        }
        self.inner.write_at(buf, off)
    }
    fn sync(&self) -> std::io::Result<()> {
        self.inner.sync()
    }
}

// ───────────────────────── --check ─────────────────────────

/// Whether a stick is worth offering a recluster for. Cheap: the boot regions, the root
/// directory and the bitmap are read, no other directory.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    /// Clusters of at least [`OFFER_MIN_CLUSTER`] and enough free space.
    pub benefit: bool,
    /// "ok", or why not (a [`Refused`] reason, or "small-clusters").
    pub reason: &'static str,
    pub message: String,
    pub cluster: u64,
    pub new_cluster: u64,
    pub free: u64,
    /// Free space the relocation needs, at most.
    pub need_free: u64,
    /// Upper estimates (the directories are not read).
    pub write_bytes: u64,
    pub est_secs: f64,
    /// An interrupted switch left the backup boot sector to write: run `flashcp recluster`.
    pub finish: bool,
}

impl Check {
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"event":"check","op":"recluster","benefit":{},"reason":"{}","message":{},"cluster":{},"new_cluster":{},"free":{},"need_free":{},"write_bytes":{},"est_secs":{:.1},"finish":{}}}"#,
            self.benefit,
            self.reason,
            json_str(&self.message),
            self.cluster,
            self.new_cluster,
            self.free,
            self.need_free,
            self.write_bytes,
            self.est_secs,
            self.finish
        )
    }
}

/// `flashcp recluster --check`: read-only, no exclusive claim (like `info`).
pub fn check(target: &TargetSpec) -> Result<Check> {
    let (f, len) = match target {
        TargetSpec::Fd(fd) => {
            let t = crate::exfat::open_target_fd(*fd, false)?;
            (t.file, t.bytes)
        }
        TargetSpec::Path(p) => {
            use std::os::unix::fs::FileTypeExt;
            let f = File::open(p).with_context(|| format!("open {}", p.display()))?;
            let md = f.metadata()?;
            let len = if md.file_type().is_block_device() {
                crate::exfat::blk_size(&f)?
            } else if md.is_file() {
                md.len()
            } else {
                bail!("{} is neither a block device nor a regular image file", p.display());
            };
            (f, len)
        }
    };
    let cost = Cost::of(&f);
    let mut c = Check {
        benefit: false,
        reason: "ok",
        message: String::new(),
        cluster: 0,
        new_cluster: 0,
        free: 0,
        need_free: 0,
        write_bytes: 0,
        est_secs: 0.0,
        finish: false,
    };
    if let Err(e) = check_on(&f, len, &cost, &mut c) {
        let e = invalid(e);
        let r = refusal_of(&e).expect("invalid() always gives a refusal");
        (c.reason, c.message) = (r.reason, r.message.clone());
    }
    c.benefit = c.reason == "ok";
    Ok(c)
}

fn check_on(f: &File, len: u64, cost: &Cost, c: &mut Check) -> Result<()> {
    let boot = read_boot(f)?;
    precheck(&boot)?;
    c.finish = backup_state(&boot)? == Backup::SwitchPending;
    let mut v = Vol::open(f.try_clone().context("dup the device handle")?, len, false, true)?;
    (c.cluster, c.new_cluster) = (v.cs, v.cs);
    c.free = v.free_clusters() * v.cs;
    journal_sane(&v)?;
    crate::drive::djournal::hide_from(&mut v).map_err(|e| refused("pending-commit", format!("{e:#}")))?;
    if v.dirs[0].children.values().any(crate::volume::is_sync_journal) {
        return Err(refused("pending-sync", format!("the volume holds the journal of {}", crate::volume::SYNC_HINT)));
    }
    // Clusters marked bad: the FAT read in one go (it is small at the cluster sizes worth an
    // offer: 4 MB on 128 GB at 128 KB). An allocated cluster whose entry says bad.
    let fat_len = (v.cc + 2) * 4;
    if fat_len <= 64 << 20 && v.cs >= OFFER_MIN_CLUSTER {
        let fat = v.read_at(v.fat_off, fat_len)?;
        let bad = (2..v.cc + 2).find(|&cl| bit(&v.bitmap, cl) && le32(&fat, (cl * 4) as usize) == BAD);
        if let Some(cl) = bad {
            return Err(refused(
                "bad-clusters",
                format!("the volume has clusters marked bad (cluster {cl}); recluster would lose those marks"),
            ));
        }
    }
    if v.cs < OFFER_MIN_CLUSTER {
        return Err(refused(
            "small-clusters",
            format!("the clusters are {} already; nothing worth changing", kb(v.cs)),
        ));
    }
    let Some(g) = target_geometry(v.vol_bytes, v.heap_off, v.cs, v.cc, None)? else { unreachable!("cs ≥ 16 KB") };
    c.new_cluster = g.cs;
    // The directories are not read: allow an entry set of average size per used cluster.
    let used = v.cc - v.free_clusters();
    let dirs = (used * 160).min(32 << 20);
    let meta = g.cc.div_ceil(8).div_ceil(g.cs) * g.cs + 2 * 65536 + dirs;
    let kz = (g.k + meta.div_ceil(v.cs)).min(v.cc);
    let in_zone = (2..2 + kz).filter(|&cl| bit(&v.bitmap, cl)).count() as u64;
    let free_behind = (2 + kz..2 + v.cc).filter(|&cl| !bit(&v.bitmap, cl)).count() as u64 * v.cs;
    c.need_free = in_zone * v.cs + dirs.div_ceil(v.cs) * v.cs + 8 * v.cs;
    c.write_bytes = in_zone * v.cs + meta + 64 * FAT_PAGE;
    c.est_secs = 0.5 + c.write_bytes as f64 / cost.bps + (in_zone * v.cs) as f64 / cost.read_bps;
    if kz >= v.cc || c.need_free > free_behind {
        return Err(refused(
            "no-space",
            format!(
                "not enough free space: about {} must move to make room for the new FAT, and {} is free behind it",
                human_bytes(c.need_free as f64),
                human_bytes(free_behind as f64)
            ),
        ));
    }
    c.message = format!(
        "clusters {} → {}: about {:.0} s; small files will need smaller writes",
        kb(v.cs),
        kb(g.cs),
        c.est_secs.ceil()
    );
    Ok(())
}
