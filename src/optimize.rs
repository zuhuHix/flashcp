//! `flashcp optimize`: rewrite an existing exFAT volume into the flashcp layout without losing
//! data (pull to a local temp copy, verify, build, verify, delete the copy).
//!
//! The result is what `flashcp build` would have made from the same files: every file one
//! contiguous extent, all directories together right after the system structures, files
//! packed in directory order, and the cluster size the optimizer picks for this data (the
//! "reformat for small files" path that keeps the data). Kept: every file's contents and path,
//! empty directories, modification times (10 ms, as exFAT stores them) and the volume label.
//! Not kept (`build` cannot write them): file attributes (read-only, hidden, system, archive),
//! creation and access times, the volume serial number and GUID.
//!
//! Safety argument, phase by phase. The target is opened ONCE, exclusively (O_EXCL on a block
//! device, flock on an image), and a dup of that fd is held until the end, so nothing can mount
//! or write it between phases. At every instant a complete copy of the data exists, and every
//! error says which one:
//!
//! 1. **pull** into `<tmp>/tree` (synced), then `state=pulled`. The device is only read, and
//!    only once: pull hashes every extent it reads (the very bytes it writes to the copy) into
//!    the device's manifest (path, size, order-independent block hash, mtime; see
//!    [`crate::verify`]). The tree is then made read-only (files 0400, directories 0500; the
//!    temp dir itself, which holds state, manifest and lock, is 0700), and stays so until the
//!    cleanup (or a removal of the dir) gives the owner write permission back.
//! 2. **verify**: that manifest must list exactly what the metadata walk of the device lists,
//!    and equal the manifest of `<tmp>/tree` (hashed from the local disk); saved to
//!    `<tmp>/manifest`, `state=verified`, all fsynced. Only if pull had to retry a device read
//!    is the device's data read again for this (in physical order, with pull's planner and
//!    strategy). Until here an error or crash leaves the device untouched, and the temp dir is
//!    deleted (on a crash: by the next run).
//! 3. **build**: re-scan the tree (it must list exactly the manifest) and run the cluster
//!    optimizer (fails before any write if build would refuse), `syncfs` the temp filesystem
//!    (copy, directories, modes, manifest), fsync `state=building`, then build onto the held
//!    target with a [`Guard`]: every file is hashed as it is read for the device and must
//!    match the manifest, or the build stops at once (the copy changed after it was verified:
//!    the user is told to restore it and re-run). The main boot region is written last (zeros
//!    until then; the backup boot region with our serial but an invalid checksum first), so
//!    an interrupted build is never a mountable volume. From here until the check passes, the
//!    verified temp copy is the copy; it is never deleted on error.
//! 4. **check**: the device, re-parsed, must be a clean volume with the same label whose
//!    manifest (read in physical order with pull's planner and strategy) equals
//!    `<tmp>/manifest`.
//! 5. **cleanup**: `state=done`, then the tree, the manifest, the state file, the dir.
//!
//! The new volume's serial number, a random 128-bit job id and the planned boot sector geometry
//! are decided before the pull and saved in the state file with a fingerprint of the original
//! boot sector and the drive's identity and size; build writes the job id into the boot code
//! area of both boot sectors. So a later run can tell the volume this optimize was writing from
//! the original one and from anything else (a foreign volume with the same 32-bit serial). Every run holds an exclusive
//! flock on `<tmp>/lock`; a temp dir locked by another run is never touched.
//!
//! A later run finds the temp dir by the drive's identity (USB serial / WWID plus the
//! partition's start sector; for an image its inode) and resumes: no state, `pulled` or
//! `verified` (device never written) or `done` (checked) → the dir is discarded and a fresh run
//! starts. `building` → judged by the raw boot sector:
//! - the saved drive identity or size differs → refused, before anything else.
//! - our saved serial, job id and geometry (main or backup boot sector; our serial with another
//!   job id or geometry → refused): a dirty / unparsable volume (a zeroed main
//!   boot sector) is our half-written build → re-verify the temp tree against the manifest and
//!   rebuild; a clean one
//!   whose manifest equals the saved one is the finished build → clean up; a clean one with
//!   other content was written since → refused.
//! - the original boot sector (flags aside), clean, content equal to the manifest: the build
//!   never started → re-verify and rebuild. Original but dirty, or with other content: the drive
//!   was used since and may hold newer data → refused.
//! - anything else (another filesystem, another exFAT volume, another card) → refused.
//!
//! A refusal keeps the temp dir and explains what is where; user data is never erased
//! automatically. An explicit `--temp` on a filesystem that may not survive a power loss
//! (tmpfs, ramfs, overlayfs, unknown) is refused unless `--temp-volatile-i-accept-data-loss`. Image targets are re-checked for a loop device right before the build and
//! the check (loop devices ignore our flock).

use crate::events::{Event, Op, Sink, Ticker};
use crate::exfat::{
    BuildOpts, FatMode, FlagGuard, Guard, Node, SECTOR, SYS_BLOCK, SourceChanged, Target, TargetSpec,
    build_target_guarded, choose_cluster, related_devs, scan_tree, set_direct, upcase_map, usable_bytes, validate_name,
};
use crate::pull::{PullOpts, Strategy, pull_file_hashed, read_strategy};
use crate::util::human_bytes;
use crate::verify::{Manifest, Progress, VolTree, hash_tree, hash_volume, walk};
use crate::volume::Vol;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Test hook: stop at a point (pull | verify | building | build | cleanup), with an error, or
/// with abort() (as if killed) when FLASHCP_TEST_OPTIMIZE_ABORT=1.
const STOP_ENV: &str = "FLASHCP_TEST_OPTIMIZE_STOP";
const ABORT_ENV: &str = "FLASHCP_TEST_OPTIMIZE_ABORT";
/// Test hook: flip the first byte of this file (path relative to the tree) right after pull.
const CORRUPT_ENV: &str = "FLASHCP_TEST_OPTIMIZE_CORRUPT";
/// Test hook: verify as if pull had retried a read: hash the device again (the fallback), and
/// also require that hash to equal the one pull computed while reading.
const VERIFY_DEVICE_ENV: &str = "FLASHCP_TEST_OPTIMIZE_VERIFY_DEVICE";
/// Test hook (shared with pull): trace device reads to stderr; optimize adds a
/// "flashcp-phase NAME" line at the start of each phase.
const TRACE_READS_ENV: &str = "FLASHCP_TEST_TRACE_READS";
/// Test hook: pretend every temp candidate has this many bytes free.
const FAKE_FREE_ENV: &str = "FLASHCP_TEST_FAKE_FREE";
/// Free space kept in reserve on the temp filesystem beyond the copy itself.
const MARGIN: u64 = 64 << 20;
/// Metadata slack per file and directory of the copy.
const PER_ENTRY: u64 = 4096;
const TMPFS_MAGIC: i64 = 0x0102_1994;
const RAMFS_MAGIC: i64 = 0x8584_58f6;
/// Test hook: pretend the image got attached to a loop device right before this phase
/// (build | check).
const LOOP_ENV: &str = "FLASHCP_TEST_OPTIMIZE_LOOP";
/// statfs f_type of filesystems known to be disk-backed (the temp copy must survive a power
/// loss): ext2/3/4, xfs, btrfs, f2fs, bcachefs, zfs, ntfs3, ntfs, exfat, vfat, nilfs, jfs,
/// reiserfs.
const DISK_FS: [u32; 13] = [
    0xEF53,
    0x5846_5342,
    0x9123_683E,
    0xF2F5_2010,
    0xCA45_1A4E,
    0x2FC1_2FC1,
    0x7366_746E,
    0x5346_544E,
    0x2011_BAB0,
    0x4D44,
    0x3434,
    0x3153_464A,
    0x5265_4973,
];
const OVERLAYFS_MAGIC: u32 = 0x794C_7630;
const STATE_MAGIC: &str = "flashcp-optimize-state 3";

#[derive(Clone, Debug, Default)]
pub struct OptimizeOpts {
    /// Directory to hold the temporary copy (default: picked automatically, see [`optimize`]).
    pub temp: Option<PathBuf>,
    /// Only print the plan.
    pub dry_run: bool,
    /// Rebuild even if the volume is already in the flashcp layout.
    pub force: bool,
    /// Allow an explicit `temp` on a filesystem that may not survive a power loss (tmpfs,
    /// ramfs, overlayfs, unknown): a power loss or crash during the rebuild then loses the data.
    pub temp_volatile: bool,
}

#[derive(Clone, Debug)]
pub struct OptimizeStats {
    pub files: u64,
    pub dirs: u64,
    pub data_bytes: u64,
    /// Bytes `build` wrote (0 when nothing was rebuilt).
    pub written: u64,
    pub old_cluster: u64,
    pub cluster: u64,
    /// The volume already had the flashcp layout; nothing was done.
    pub already: bool,
    /// This run finished an interrupted one.
    pub resumed: bool,
    /// The temp dir used (deleted on success).
    pub temp: Option<PathBuf>,
    /// Seconds per phase, in order.
    pub phases: Vec<(&'static str, f64)>,
    pub secs: f64,
}

fn hook(point: &str) -> Result<()> {
    if std::env::var(STOP_ENV).as_deref() == Ok(point) {
        if std::env::var(ABORT_ENV).as_deref() == Ok("1") {
            std::process::abort();
        }
        bail!("stopped at {point} ({STOP_ENV})");
    }
    Ok(())
}

// ───────────────────────── state file ─────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// No state file: the pull never finished.
    None,
    Pulled,
    Verified,
    Building,
    Done,
}

#[derive(Clone, Debug)]
struct State {
    phase: Phase,
    label: String,
    /// Fingerprint of the original volume's boot sector (see [`fingerprint`]).
    orig: u64,
    /// Serial number of the volume this run writes (decided before the pull).
    serial: u32,
    /// Random id of this optimize, also written into the boot code area of the boot sectors
    /// it builds (see [`crate::exfat::JOB_TAG`]).
    job: [u8; 16],
    /// The boot sector geometry this optimize planned.
    geom: Geom,
    /// The target's identity (see [`identity`]) and size in bytes.
    device: String,
    size: u64,
}

/// The geometry fields of an exFAT boot sector that a build decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geom {
    vol_len: u64,
    part_off: u64,
    fat_off: u32,
    fat_len: u32,
    heap_off: u32,
    clusters: u32,
    shift: u8,
}

impl Geom {
    /// What a build of `p` onto `tgt` writes.
    fn planned(p: &crate::exfat::Plan, tgt: &Target) -> Geom {
        Geom {
            vol_len: usable_bytes(tgt.bytes) / SECTOR,
            part_off: tgt.part_off,
            fat_off: p.g.fat_off as u32,
            fat_len: p.g.fat_len as u32,
            heap_off: p.g.heap_off as u32,
            clusters: p.g.cluster_count as u32,
            shift: (p.g.cluster / SECTOR).trailing_zeros() as u8,
        }
    }

    fn of(b: &[u8]) -> Geom {
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Geom {
            vol_len: u64_at(72),
            part_off: u64_at(64),
            fat_off: u32_at(80),
            fat_len: u32_at(84),
            heap_off: u32_at(88),
            clusters: u32_at(92),
            shift: b[109],
        }
    }

    fn encode(&self) -> String {
        format!(
            "{},{},{},{},{},{},{}",
            self.vol_len, self.part_off, self.fat_off, self.fat_len, self.heap_off, self.clusters, self.shift
        )
    }

    fn decode(s: &str) -> Option<Geom> {
        let v: Vec<u64> = s.split(',').map(|x| x.parse().ok()).collect::<Option<_>>()?;
        let [vol_len, part_off, fat_off, fat_len, heap_off, clusters, shift] = v[..] else { return None };
        Some(Geom {
            vol_len,
            part_off,
            fat_off: fat_off.try_into().ok()?,
            fat_len: fat_len.try_into().ok()?,
            heap_off: heap_off.try_into().ok()?,
            clusters: clusters.try_into().ok()?,
            shift: shift.try_into().ok()?,
        })
    }

    /// The fields that differ from `other`, for an error message.
    fn diff(&self, other: &Geom) -> String {
        let mut d = Vec::new();
        let mut f = |n: &str, a: u64, b: u64| {
            if a != b {
                d.push(format!("{n} {b} instead of {a}"))
            }
        };
        f("VolumeLength", self.vol_len, other.vol_len);
        f("PartitionOffset", self.part_off, other.part_off);
        f("FatOffset", self.fat_off.into(), other.fat_off.into());
        f("FatLength", self.fat_len.into(), other.fat_len.into());
        f("ClusterHeapOffset", self.heap_off.into(), other.heap_off.into());
        f("ClusterCount", self.clusters.into(), other.clusters.into());
        f("SectorsPerClusterShift", self.shift.into(), other.shift.into());
        d.join(", ")
    }
}

/// 16 random bytes from the kernel (getrandom, else /dev/urandom).
pub(crate) fn random_id() -> Result<[u8; 16]> {
    let mut b = [0u8; 16];
    let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    if n != b.len() as isize {
        use std::io::Read;
        File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).context("read random bytes for a job id")?;
    }
    Ok(b)
}

fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex_bytes(s: &str) -> Option<Vec<u8>> {
    s.len().is_multiple_of(2).then_some(())?;
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}

fn hex(s: &str) -> String {
    hex_bytes(s.as_bytes())
}

fn unhex(s: &str) -> Option<String> {
    String::from_utf8(unhex_bytes(s)?).ok()
}

impl State {
    fn encode(&self) -> String {
        let p = match self.phase {
            Phase::None => "none",
            Phase::Pulled => "pulled",
            Phase::Verified => "verified",
            Phase::Building => "building",
            Phase::Done => "done",
        };
        format!(
            "{STATE_MAGIC}\nstate={p}\nlabel={}\noriginal={:016x}\nserial={:08x}\njob={}\ngeometry={}\ndevice={}\nsize={}\n",
            hex(&self.label),
            self.orig,
            self.serial,
            hex_bytes(&self.job),
            self.geom.encode(),
            hex(&self.device),
            self.size
        )
    }

    /// The state in `dir`; Ok(None) when there is no state file, Err when it is unreadable.
    fn load(dir: &Path) -> Result<Option<State>> {
        let s = match fs::read_to_string(dir.join("state")) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("read the state file"),
        };
        let mut lines = s.lines();
        ensure!(lines.next() == Some(STATE_MAGIC), "the state file is not a flashcp optimize state");
        let (mut phase, mut label, mut orig, mut serial) = (None, None, None, None);
        let (mut job, mut geom, mut device, mut size) = (None, None, None, None);
        for l in lines {
            match l.split_once('=') {
                Some(("state", v)) => {
                    phase = Some(match v {
                        "none" => Phase::None,
                        "pulled" => Phase::Pulled,
                        "verified" => Phase::Verified,
                        "building" => Phase::Building,
                        "done" => Phase::Done,
                        _ => bail!("unknown state {v:?}"),
                    })
                }
                Some(("label", v)) => label = unhex(v),
                Some(("original", v)) => orig = u64::from_str_radix(v, 16).ok(),
                Some(("serial", v)) => serial = u32::from_str_radix(v, 16).ok(),
                Some(("job", v)) => job = unhex_bytes(v).and_then(|b| <[u8; 16]>::try_from(b).ok()),
                Some(("geometry", v)) => geom = Geom::decode(v),
                Some(("device", v)) => device = unhex(v),
                Some(("size", v)) => size = v.parse().ok(),
                _ => {}
            }
        }
        match (phase, label, orig, serial, job, geom, device, size) {
            (Some(phase), Some(label), Some(orig), Some(serial), Some(job), Some(geom), Some(device), Some(size)) => {
                Ok(Some(State { phase, label, orig, serial, job, geom, device, size }))
            }
            _ => bail!("the state file is incomplete"),
        }
    }
}

/// Writes `name` in `dir` atomically and durably: temp file, fsync, rename, fsync the dir.
fn write_durable(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(data).and_then(|_| f.sync_all()).with_context(|| format!("write {}", tmp.display()))?;
    drop(f);
    fs::rename(&tmp, dir.join(name)).with_context(|| format!("rename {} into place", tmp.display()))?;
    File::open(dir).and_then(|d| d.sync_all()).with_context(|| format!("fsync {}", dir.display()))
}

fn save_state(dir: &Path, st: &State) -> Result<()> {
    write_durable(dir, "state", st.encode().as_bytes())
}

/// The original volume's identity: its boot sector without VolumeFlags and PercentInUse (which
/// a mount changes). The serial number is in it, so a reformat changes it, and so does build.
fn fingerprint(sector0: &[u8]) -> u64 {
    let mut s = sector0.to_vec();
    for i in [106, 107, 112] {
        s[i] = 0;
    }
    xxhash_rust::xxh3::xxh3_64(&s)
}

fn serial_of(sector: &[u8]) -> u32 {
    u32::from_le_bytes(sector[100..104].try_into().unwrap())
}

fn is_exfat(sector: &[u8]) -> bool {
    sector.len() >= 512 && &sector[3..11] == b"EXFAT   " && sector[510..512] == [0x55, 0xAA]
}

/// The optimize job id in a boot sector's boot code area (see [`crate::exfat::JOB_TAG`]).
fn job_of(sector: &[u8]) -> Option<[u8; 16]> {
    use crate::exfat::{JOB_TAG, JOB_TAG_OFF};
    let at = JOB_TAG_OFF + JOB_TAG.len();
    (sector.len() >= 512 && &sector[JOB_TAG_OFF..at] == JOB_TAG).then(|| sector[at..at + 16].try_into().unwrap())
}

/// A serial for the new volume that differs from the original's (and is not 0).
fn new_serial(orig_sector0: &[u8]) -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let mut s = (t ^ (t >> 32) ^ (std::process::id() as u64).rotate_left(16)) as u32;
    while s == 0 || s == serial_of(orig_sector0) {
        s = s.wrapping_add(0x9E37_79B9);
    }
    s
}

/// Takes an exclusive flock on `<dir>/lock`, held as long as the returned file lives. Fails
/// when another run holds it.
fn lock_dir(dir: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let p = dir.join("lock");
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&p)
        .with_context(|| format!("open {}", p.display()))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        ensure!(
            e.raw_os_error() != Some(libc::EWOULDBLOCK),
            "{} is in use by another `flashcp optimize` (of this drive, or of another one with the same identity, \
             e.g. a second card in the same reader); wait for it to finish, then run again",
            dir.display()
        );
        return Err(anyhow::Error::new(e).context(format!("flock {}", p.display())));
    }
    Ok(f)
}

// ───────────────────────── where the copy goes ─────────────────────────

/// The drive's stable identity (names the temp dir) and the "maj:min" of every block device
/// the copy must not live on (the whole disk, its partitions and holders; empty for an image).
/// For a block device the key is the disk's identity plus the partition's start sector
/// (`part_off`), so partitions of one disk, and cards sharing one reader's USB serial at
/// different starts, get different temp dirs (the state's fingerprint and serial catch the rest).
fn identity(md: &fs::Metadata, part_off: u64) -> Result<(String, Vec<String>)> {
    if !md.file_type().is_block_device() {
        return Ok((format!("img-{}-{}", md.dev(), md.ino()), Vec::new()));
    }
    let rdev = md.rdev();
    let (maj, min) = (libc::major(rdev), libc::minor(rdev));
    let key = match crate::devprofile::device_key(rdev) {
        Some(k) => format!("{k}-p{part_off}"),
        None => format!("dev-{maj}-{min}"),
    };
    // A partition's siblings are on the same stick: avoid the whole disk.
    let node = Path::new(SYS_BLOCK).join(format!("{maj}:{min}"));
    let disk = if node.join("partition").exists() {
        fs::read_to_string(node.join("../dev")).map(|s| s.trim().to_string()).unwrap_or(format!("{maj}:{min}"))
    } else {
        format!("{maj}:{min}")
    };
    let devs = related_devs(Path::new(SYS_BLOCK), &disk).context("list the drive's partitions and holders")?;
    Ok((key, devs))
}

/// Candidate base directories, in order: --temp alone if given; else $FLASHCP_TMPDIR,
/// /var/tmp (disk-backed and kept across reboots on most systems), $TMPDIR or /tmp, and
/// $XDG_CACHE_HOME or ~/.cache. (path, may be created)
fn candidates(explicit: Option<&Path>) -> Vec<(PathBuf, bool)> {
    if let Some(p) = explicit {
        return vec![(p.to_path_buf(), true)];
    }
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut v = Vec::new();
    if let Some(p) = env("FLASHCP_TMPDIR") {
        v.push((p, false));
    }
    v.push((PathBuf::from("/var/tmp"), false));
    v.push((env("TMPDIR").unwrap_or_else(|| PathBuf::from("/tmp")), false));
    if let Some(p) = env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache"))) {
        v.push((p, true));
    }
    let mut seen = HashSet::new();
    v.retain(|(p, _)| p.is_absolute() && seen.insert(fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
    v
}

/// The nearest existing ancestor of `p` (what a not-yet-created directory would live on).
fn existing_ancestor(p: &Path) -> Option<&Path> {
    p.ancestors().find(|a| a.exists())
}

fn fs_type(p: &Path) -> Result<i64> {
    let c = CString::new(p.as_os_str().as_bytes())?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::statfs(c.as_ptr(), &mut s) } == 0, "statfs: {}", std::io::Error::last_os_error());
    Ok(s.f_type as i64)
}

fn free_bytes(p: &Path) -> Result<u64> {
    if let Some(v) = std::env::var(FAKE_FREE_ENV).ok().and_then(|v| v.parse().ok()) {
        return Ok(v);
    }
    crate::util::statfs_free(p)
}

/// Why a filesystem of statfs type `t` may not survive a power loss (None: a known disk
/// filesystem, see [`DISK_FS`]).
fn not_disk_backed(t: i64) -> Option<String> {
    let t = t as u64 as u32;
    if DISK_FS.contains(&t) {
        None
    } else if t as i64 == TMPFS_MAGIC || t as i64 == RAMFS_MAGIC {
        Some("is in RAM (tmpfs)".into())
    } else if t == OVERLAYFS_MAGIC {
        Some("is on overlayfs (on live-USB systems usually backed by RAM)".into())
    } else {
        Some(format!("is on a filesystem flashcp does not know to be disk-backed (type {t:#x}: RAM, zram or network?)"))
    }
}

/// Flag that allows an explicit --temp on a filesystem not known to be disk-backed.
pub const VOLATILE_FLAG: &str = "--temp-volatile-i-accept-data-loss";

/// Picks the first candidate that is disk-backed (one of [`DISK_FS`]; tmpfs, overlayfs, zram
/// and the like may be gone after a power loss while the drive is erased; allowed only when
/// given explicitly AND `volatile` (or for a dry run, with a warning), not on the drive being
/// optimized, writable, and has `need` bytes free. Lists every rejection otherwise.
fn pick_base(
    cands: &[(PathBuf, bool)],
    need: u64,
    avoid: &[String],
    what: &str,
    volatile: bool,
    dry_run: bool,
) -> Result<PathBuf> {
    let explicit = cands.len() == 1 && cands[0].1;
    let mut why = Vec::new();
    for (p, create) in cands {
        let r = (|| -> Result<std::result::Result<(), String>> {
            let at = match existing_ancestor(p) {
                Some(a) if a == p.as_path() || *create => a,
                _ => return Ok(Err("does not exist".into())),
            };
            let md = fs::metadata(at).with_context(|| format!("stat {}", at.display()))?;
            if !md.is_dir() {
                return Ok(Err("is not a directory".into()));
            }
            if avoid.contains(&format!("{}:{}", libc::major(md.dev()), libc::minor(md.dev()))) {
                return Ok(Err(format!("is on {what} itself")));
            }
            let ram = not_disk_backed(fs_type(at)?);
            if let Some(why) = &ram
                && !explicit
            {
                return Ok(Err(format!(
                    "{why}: the copy could be lost on a power loss while the drive is rebuilt (only an explicit \
                     --temp DIR with {VOLATILE_FLAG} can use it)"
                )));
            }
            if let Some(why) = &ram
                && !volatile
                && !dry_run
            {
                return Ok(Err(format!(
                    "{why}: a power loss or crash while the drive is rebuilt would lose this copy, and with it the \
                     drive's data; use a directory on a disk, or add {VOLATILE_FLAG} to accept that risk"
                )));
            }
            let c = CString::new(at.as_os_str().as_bytes())?;
            if unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
                return Ok(Err("is not writable".into()));
            }
            let free = free_bytes(at)?;
            if free < need {
                return Ok(Err(format!("has only {} free", human_bytes(free as f64))));
            }
            if let Some(why) = ram {
                eprintln!(
                    "warning: {} {why}: if the power fails or the computer crashes while the drive is rebuilt, the \
                     copy and with it the drive's data are lost{}",
                    p.display(),
                    if volatile { String::new() } else { format!(" (a real run refuses it without {VOLATILE_FLAG})") }
                );
            }
            Ok(Ok(()))
        })();
        match r {
            Ok(Ok(())) => return Ok(p.clone()),
            Ok(Err(reason)) => why.push(format!("  {} {reason}", p.display())),
            Err(e) => why.push(format!("  {}: {e:#}", p.display())),
        }
    }
    bail!(
        "no place for the temporary copy: optimize needs {} free (outside {what}, on a disk filesystem) to hold a \
         verified copy of your files while the drive is rebuilt. Checked:\n{}\nFree some space or pass --temp DIR on \
         another disk",
        human_bytes(need as f64),
        why.join("\n")
    )
}

fn temp_name(key: &str) -> String {
    format!("flashcp-optimize-{key}")
}

/// Fails unless `p` is a real directory (not a symlink) owned by us: in a shared /tmp another
/// user could have planted it.
fn check_ours(p: &Path) -> Result<()> {
    let md = fs::symlink_metadata(p).with_context(|| format!("stat {}", p.display()))?;
    ensure!(
        md.is_dir() && md.uid() == unsafe { libc::geteuid() },
        "{} exists but is not a directory owned by you; remove it (or pass --temp elsewhere) and run again",
        p.display()
    );
    Ok(())
}

// ───────────────────────── the copy is read-only ─────────────────────────

/// Makes the copy in `tree` read-only (files 0400, directories 0500, the tree itself too), so
/// that nothing changes it by accident between verify and the rebuild (the build still hashes
/// every byte it reads, see [`crate::exfat::Guard`]). Only regular files and directories are
/// allowed in it. The temp dir above it stays 0700 (it holds the state file).
fn seal(tree: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![tree.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).with_context(|| format!("read directory {}", d.display()))? {
            let p = e.with_context(|| format!("read directory {}", d.display()))?.path();
            let md = fs::symlink_metadata(&p).with_context(|| format!("stat {}", p.display()))?;
            if md.is_dir() {
                stack.push(p);
            } else if md.is_file() {
                fs::set_permissions(&p, fs::Permissions::from_mode(0o400))
                    .with_context(|| format!("make {} read-only", p.display()))?;
            } else {
                bail!("{} is neither a regular file nor a directory", p.display());
            }
        }
        // After its entries were listed (0500 still allows listing and reading).
        fs::set_permissions(&d, fs::Permissions::from_mode(0o500))
            .with_context(|| format!("make {} read-only", d.display()))?;
    }
    Ok(())
}

/// Gives the owner write permission again on everything in `tree` (before it is deleted).
/// A missing tree is fine; symlinks (which [`seal`] never leaves) are not followed.
fn unseal(tree: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![tree.to_path_buf()];
    while let Some(d) = stack.pop() {
        let md = match fs::symlink_metadata(&d) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            r => r.with_context(|| format!("stat {}", d.display()))?,
        };
        if !md.is_dir() {
            continue;
        }
        // Before its entries are listed (a dir without u+rx could not be).
        fs::set_permissions(&d, fs::Permissions::from_mode(md.mode() & 0o7777 | 0o700))
            .with_context(|| format!("make {} writable", d.display()))?;
        for e in fs::read_dir(&d).with_context(|| format!("read directory {}", d.display()))? {
            let p = e.with_context(|| format!("read directory {}", d.display()))?.path();
            let md = fs::symlink_metadata(&p).with_context(|| format!("stat {}", p.display()))?;
            if md.is_dir() {
                stack.push(p);
            } else if md.is_file() {
                fs::set_permissions(&p, fs::Permissions::from_mode(md.mode() & 0o7777 | 0o600))
                    .with_context(|| format!("make {} writable", p.display()))?;
            }
        }
    }
    Ok(())
}

/// Deletes a temp dir (its tree read-only or not).
fn remove_temp(tmp: &Path) -> Result<()> {
    unseal(&tmp.join("tree"))?;
    fs::remove_dir_all(tmp).with_context(|| format!("remove {}", tmp.display()))
}

/// How to delete a kept copy by hand.
fn rm_hint(tmp: &Path) -> String {
    format!("`chmod -R u+w {0} && rm -rf {0}`; the copy is read-only", tmp.display())
}

/// Test hook: changes the first byte of `<tree>/<rel>`, keeping its size, mode and mtime (so
/// only the contents differ from the manifest), like another process could.
const TAMPER_ENV: &str = "FLASHCP_TEST_OPTIMIZE_TAMPER";

fn tamper(tree: &Path, rel: &str) {
    use std::os::unix::fs::{FileExt, PermissionsExt};
    let p = tree.join(rel);
    let md = fs::metadata(&p).expect("tamper: stat");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).expect("tamper: chmod");
    let f = fs::OpenOptions::new().read(true).write(true).open(&p).expect("tamper: open");
    let mut b = [0u8];
    f.read_exact_at(&mut b, 0).expect("tamper: read");
    f.write_all_at(&[b[0] ^ 0x5A], 0).expect("tamper: write");
    f.set_modified(md.modified().expect("tamper: mtime")).expect("tamper: set mtime");
    drop(f);
    fs::set_permissions(&p, md.permissions()).expect("tamper: chmod back");
}

/// Writes every dirty page of the temp dir's filesystem (the copy, its directories and modes,
/// the manifest) to disk. Called before anything on the device is erased.
fn sync_temp(tmp: &Path) -> Result<()> {
    let d = File::open(tmp).with_context(|| format!("open {}", tmp.display()))?;
    crate::dirfd::syncfs(std::os::fd::AsFd::as_fd(&d)).with_context(|| format!("syncfs {}", tmp.display()))?;
    if std::env::var_os(TRACE_READS_ENV).is_some() {
        eprintln!("flashcp-temp-synced");
    }
    Ok(())
}

// ───────────────────────── planning ─────────────────────────

/// What a node of the build plan is on the old volume.
#[derive(Clone, Copy)]
enum Ref {
    Dir(u32),
    File(u32),
}

/// The tree as `scan_tree` would see it after the pull (breadth-first, children sorted by
/// name bytes), so the planned layout is exactly what the rebuild will write. Refuses names
/// `build` cannot write (forbidden characters, names equal under flashcp's up-case table).
fn nodes_of(t: &VolTree) -> Result<(Vec<Node>, Vec<Ref>)> {
    let upc = upcase_map();
    let node = |name: Vec<u16>, src: PathBuf, is_dir: bool, size: u64| Node {
        name,
        src,
        is_dir,
        size,
        mtime: (0, 0),
        children: vec![],
        first_cluster: 0,
        clusters: 0,
    };
    let mut nodes = vec![node(vec![], PathBuf::new(), true, 0)];
    let mut refs = vec![Ref::Dir(0)];
    let mut queue = std::collections::VecDeque::from([(0usize, 0u32)]);
    while let Some((ni, di)) = queue.pop_front() {
        let d = &t.dirs[di as usize];
        let mut kids: Vec<(String, Ref)> =
            d.dirs.iter().map(|&i| (t.dirs[i as usize].path.clone(), Ref::Dir(i))).collect();
        kids.extend(d.files.iter().map(|&i| (t.files[i as usize].path.clone(), Ref::File(i))));
        let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
        kids.sort_by_key(|(p, _)| base(p));
        let mut seen = HashSet::new();
        for (path, r) in kids {
            let (name, is_dir, size) = match r {
                Ref::Dir(i) => (&t.dirs[i as usize].name, true, 0),
                Ref::File(i) => (&t.files[i as usize].name, false, t.files[i as usize].len),
            };
            validate_name(name, &upc, &mut seen, Path::new(&path))
                .context("flashcp build cannot write this name, so this volume cannot be optimized")?;
            // exFAT allows 255 UTF-16 units; the local copy needs ≤ 255 bytes of UTF-8 per name
            // and ≤ 4096 per path. Refused now rather than after copying gigabytes.
            ensure!(
                base(&path).len() <= 255 && path.len() < crate::pull::MAX_PATH,
                "{path}: the name is too long for a local copy (more than 255 bytes of UTF-8, or a path over \
                 {} bytes), so this volume cannot be optimized",
                crate::pull::MAX_PATH
            );
            let idx = nodes.len();
            nodes.push(node(name.clone(), PathBuf::from(&path), is_dir, size));
            refs.push(r);
            nodes[ni].children.push(idx);
            if let Ref::Dir(i) = r {
                queue.push_back((idx, i));
            }
        }
    }
    Ok((nodes, refs))
}

/// Whether the volume already is exactly what a rebuild would write: same cluster size and
/// geometry, system structures and directories where the plan puts them, every file one
/// contiguous NoFatChain extent at its planned cluster.
fn in_layout(v: &Vol, t: &VolTree, nodes: &[Node], refs: &[Ref], p: &crate::exfat::Plan, tgt: &Target) -> bool {
    let g = &p.g;
    let contiguous = |c: &[u32], first: u64| c.iter().enumerate().all(|(i, &x)| x as u64 == first + i as u64);
    let geometry = v.bps == SECTOR
        && v.cs == g.cluster
        && v.fat_off == g.fat_off * SECTOR
        && v.heap_off == g.heap_off * SECTOR
        && v.cc == g.cluster_count
        && v.vol_bytes == usable_bytes(tgt.bytes);
    let system = v.bitmap_clusters.len() as u64 == p.bitmap_clusters
        && contiguous(&v.bitmap_clusters, 2)
        && v.upcase_clusters.len() as u64 == p.upcase_clusters
        && contiguous(&v.upcase_clusters, 2 + p.bitmap_clusters)
        && t.root_clusters.len() as u64 == nodes[0].clusters
        && contiguous(&t.root_clusters, nodes[0].first_cluster as u64);
    geometry
        && system
        && nodes.iter().zip(refs).skip(1).all(|(n, r)| match *r {
            Ref::Dir(i) => {
                let d = &t.dirs[i as usize];
                d.no_fat_chain && d.first == n.first_cluster && d.len == n.clusters * g.cluster
            }
            Ref::File(i) => {
                let f = &t.files[i as usize];
                f.len == 0 || (f.no_fat_chain && f.first == n.first_cluster && f.valid == f.len)
            }
        })
}

fn build_opts(label: &str, serial: Option<u32>, job: Option<[u8; 16]>) -> BuildOpts {
    BuildOpts {
        cluster: None,
        label: label.to_string(),
        fat: FatMode::Prefix,
        readers: 12,
        dry_run: false,
        serial,
        job,
    }
}

// ───────────────────────── running phases ─────────────────────────

struct Run<'s> {
    tick: Ticker<'s>,
    steps: u32,
    step: u32,
    phases: Vec<(&'static str, f64)>,
    started: Option<(&'static str, Instant)>,
}

impl<'s> Run<'s> {
    fn phase(&mut self, name: &'static str) {
        self.end_phase();
        self.step += 1;
        crate::sim::trace::phase(name);
        if std::env::var_os(TRACE_READS_ENV).is_some() {
            eprintln!("flashcp-phase {name}");
        }
        self.tick.emit(Event::Phase { op: Op::Optimize, phase: name, step: self.step, steps: self.steps });
        self.started = Some((name, Instant::now()));
    }

    fn end_phase(&mut self) {
        if let Some((n, t)) = self.started.take() {
            self.phases.push((n, t.elapsed().as_secs_f64()));
        }
    }

    /// Runs `f` on a worker thread while this thread reports `progress()` about 10× a second.
    fn with_progress<T: Send>(&mut self, progress: impl Fn() -> (u64, u64), f: impl FnOnce() -> T + Send) -> T {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::scope(|s| {
            s.spawn(move || {
                let _ = tx.send(f());
            });
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(v) => {
                        let (f, b) = progress();
                        self.tick.progress(f, b, true);
                        return v;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        let (f, b) = progress();
                        self.tick.progress(f, b, false);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => panic!("hash worker died"),
                }
            }
        })
    }
}

/// Inner `pull` / `build` events: progress is forwarded, their own plan and done are not.
fn forward<'a>(tick: &'a mut Ticker<'_>) -> impl FnMut(&Event) + 'a {
    move |e: &Event| {
        if let Event::Progress { .. } = e {
            tick.emit(e.clone());
        }
    }
}

/// Hashes the walked volume on `dev` (read with pull's strategy `s`).
fn hash_device(run: &mut Run<'_>, dev: &File, t: &VolTree, s: &Strategy) -> Result<Manifest> {
    let p = Progress::default();
    run.with_progress(
        || (p.files.load(Ordering::Relaxed), p.bytes.load(Ordering::Relaxed)),
        || hash_volume(dev, t, s, &p),
    )
}

fn hash_local(run: &mut Run<'_>, tree: &Path) -> Result<Manifest> {
    let p = Progress::default();
    run.with_progress(|| (p.files.load(Ordering::Relaxed), p.bytes.load(Ordering::Relaxed)), || hash_tree(tree, &p))
}

fn first_diffs(d: &[String]) -> String {
    let mut s = d.iter().take(10).cloned().collect::<Vec<_>>().join("; ");
    if d.len() > 10 {
        s += &format!("; … ({} differences in all)", d.len());
    }
    s
}

/// Parses the device (clean volumes only) and walks it.
fn open_walk(held: &File, dev_len: u64) -> Result<(Vol, VolTree)> {
    set_direct(held, false); // the parser reads unaligned
    // Drop cached pages of the device, so the parser reads what is on the medium now (after a
    // build, not metadata cached while reading the old volume).
    unsafe { libc::posix_fadvise(std::os::fd::AsRawFd::as_raw_fd(held), 0, 0, libc::POSIX_FADV_DONTNEED) };
    let mut v = Vol::open(held.try_clone().context("dup target fd")?, dev_len, false, false)?;
    let t = walk(&mut v)?;
    Ok((v, t))
}

/// What every message says once the device may be half-written.
pub const FORMAT_WARNING: &str = "Until optimize finishes, the drive does not open anywhere, and other systems (notably \
     Windows) may offer to FORMAT it: decline (formatting erases the half-built volume), plug it back into this Linux \
     computer and re-run `flashcp optimize <device>` to resume";

/// Context marker on an optimize error after which the device may be half-written; its text
/// says where the data is and how to finish (`main` puts it into the JSON error event as
/// "recovery"). See [`recovery_of`].
#[derive(Debug)]
pub struct Recovery(pub String);

impl std::fmt::Display for Recovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Recovery {}

/// The recovery text of an optimize error (see [`Recovery`]), if the device may be half-written.
pub fn recovery_of(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<Recovery>().map(|r| r.0.as_str())
}

/// What to tell the user once the device may be half-written: the copy is safe, here is how
/// to finish.
fn recovery(what: &str, tmp: &Path, base: &Path, label: &str, fd: bool) -> Recovery {
    let again = if fd {
        format!("run the same `flashcp optimize` again on this drive with --temp {}", base.display())
    } else {
        format!("run `flashcp optimize {what} --yes --temp {}` again", base.display())
    };
    Recovery(format!(
        "{what} may be incomplete, but all your files are safe in {} (verified against {}). To finish, {again}: \
         it picks up where this run stopped. Or rebuild the drive yourself with `flashcp build {} {} --label {:?} \
         --yes-erase`. {FORMAT_WARNING}",
        tmp.join("tree").display(),
        tmp.join("manifest").display(),
        tmp.join("tree").display(),
        if fd { "<device>" } else { what },
        label
    ))
}

/// Fails if the image target is attached to a loop device now. open_target refused that at the
/// start, but a loop device ignores our flock, so a `losetup` (and mount) between phases is
/// only seen by looking again, right before the device is written and before it is checked.
fn not_looped(ctx: &Ctx, at: &str) -> Result<()> {
    if !ctx.image {
        return Ok(());
    }
    let md = ctx.held.metadata().context("fstat target")?;
    let dev = crate::add::loop_device_of(&md)
        .or_else(|| (std::env::var(LOOP_ENV).as_deref() == Ok(at)).then(|| "/dev/loop-test".to_string()));
    match dev {
        Some(d) => bail!(
            "{} was attached to loop device {d} while optimize ran (it may be mounted); stopped before the {at} \
             phase. Unmount and detach it (umount, then losetup -d {d})",
            ctx.what
        ),
        None => Ok(()),
    }
}

/// Reads `len` raw bytes at `off` of the held target (what is on the medium, not the cache);
/// shorter or empty when the device is.
fn read_raw(held: &File, off: u64, len: u64) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    set_direct(held, false);
    unsafe { libc::posix_fadvise(std::os::fd::AsRawFd::as_raw_fd(held), 0, 0, libc::POSIX_FADV_DONTNEED) };
    let mut b = vec![0u8; len as usize];
    let mut n = 0;
    crate::sim::trace::serial(crate::sim::trace::Op::Read, off, len, || {
        while n < b.len() {
            match held.read_at(&mut b[n..], off + n as u64) {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
        }
    });
    b.truncate(n);
    b
}

/// Everything about the target the phases share.
struct Ctx {
    what: String,
    fd: bool,
    /// The target is an image file (loop devices ignore our flock: see [`not_looped`]).
    image: bool,
    held: File,
    dev_len: u64,
    _flags: FlagGuard,
}

/// `optimize` of `target`: see the module docs. Temp location: `o.temp` if given, else the
/// first suitable of $FLASHCP_TMPDIR, /var/tmp, $TMPDIR or /tmp, $XDG_CACHE_HOME or ~/.cache.
pub fn optimize(target: &TargetSpec, o: &OptimizeOpts, sink: Sink<'_>) -> Result<OptimizeStats> {
    let what = target.describe();
    let untouched = |e: anyhow::Error| e.context(format!("nothing was changed on {what}"));
    let tgt = target.open_rw().map_err(untouched)?;
    // A dup of the exclusive fd (same open file description, so the same O_EXCL claim / flock)
    // is held to the very end: build_target consumes the Target, and between the phases nobody
    // may mount or write the drive.
    let held = tgt.file.try_clone().context("dup target fd").map_err(untouched)?;
    set_direct(&held, false);
    // Restores the description's flags (shared with a --target-fd helper) on every exit; the
    // Target's own guard only covers the build.
    let flags = FlagGuard::new(&held).map_err(untouched)?;
    let md = held.metadata().context("fstat target").map_err(untouched)?;
    let (key, avoid) = identity(&md, tgt.part_off).map_err(untouched)?;
    let ctx = Ctx {
        what: what.clone(),
        fd: matches!(target, TargetSpec::Fd(_)),
        image: md.file_type().is_file(),
        held,
        dev_len: tgt.bytes,
        _flags: flags,
    };
    let mut run = Run { tick: Ticker::new(sink), steps: 0, step: 0, phases: Vec::new(), started: None };
    let t0 = Instant::now();
    let cands = candidates(o.temp.as_deref());

    // ── an earlier, interrupted run? ──
    let mut building = Vec::new();
    // Every temp dir of this identity stays locked until the run ends (or it is removed).
    let mut locks = Vec::new();
    for (base, _) in &cands {
        let dir = base.join(temp_name(&key));
        match fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            _ => {}
        }
        check_ours(&dir).map_err(untouched)?;
        locks.push(lock_dir(&dir).map_err(untouched)?);
        let st = State::load(&dir).with_context(|| {
            format!(
                "{} holds a copy from an earlier optimize of this drive, but its state file is unreadable, so \
                 flashcp cannot tell whether the drive was already being rebuilt. Check the files in {} and \
                 remove the directory when you no longer need them",
                dir.display(),
                dir.join("tree").display()
            )
        })?;
        match st.as_ref().map(|s| &s.phase) {
            Some(Phase::Building) => building.push((base.clone(), dir, st.unwrap())),
            _ if o.dry_run => {
                eprintln!(
                    "note: {} holds an unfinished copy from an earlier run (the drive was not changed)",
                    dir.display()
                )
            }
            _ => {
                // The device was never written by that run (or the run finished and checked).
                eprintln!("note: discarding {} from an earlier run (the drive was not changed by it)", dir.display());
                remove_temp(&dir).map_err(untouched)?;
            }
        }
    }
    ensure!(
        building.len() <= 1,
        "more than one interrupted optimize of this drive exists ({}); keep the one you trust and remove the other",
        building.iter().map(|(_, d, _)| d.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    if let Some((base, dir, st)) = building.pop() {
        // The state must be this very drive's: the temp dir's name alone (a USB serial a
        // reader shares, a reused maj:min or inode) is not enough.
        let mut wrong = Vec::new();
        if st.device != key {
            wrong.push(format!("its identity is {key}, the copy is of {}", st.device));
        }
        if st.size != ctx.dev_len {
            wrong.push(format!("its size is {} bytes, the copy's drive had {}", ctx.dev_len, st.size));
        }
        if !wrong.is_empty() {
            bail!(
                "{} holds the copy of an interrupted optimize, but {what} is not the drive it was made from ({}), \
                 so flashcp will not write to it. Nothing was changed. The copy in {} is kept: plug in the drive that \
                 optimize was rebuilding and run it again, or, if that copy holds nothing you still need, delete it \
                 ({})",
                dir.display(),
                wrong.join("; "),
                dir.join("tree").display(),
                rm_hint(&dir)
            );
        }
        let r = resume(&mut run, ctx, tgt, &base, &dir, st, o, t0);
        drop(locks);
        return r;
    }

    // ── read and plan ──
    let (v, t) = open_walk(&ctx.held, ctx.dev_len)
        .context(
            "if an earlier `flashcp optimize` of this drive was interrupted and used --temp DIR, run it again with the \
             same --temp",
        )
        .map_err(untouched)?;
    let orig = fingerprint(&v.sector0);
    let serial = new_serial(&v.sector0);
    let label = v.label.clone();
    let old_cs = v.cs;
    let (mut nodes, refs) = nodes_of(&t).map_err(untouched)?;
    let (p, _why) = choose_cluster(&mut nodes, &tgt, &build_opts(&label, None, None))
        .context("flashcp build cannot lay out this data on the drive")
        .map_err(untouched)?;
    let already = in_layout(&v, &t, &nodes, &refs, &p, &tgt);
    let geom = Geom::planned(&p, &tgt);
    drop(v);
    let (files, dirs, data) = (t.files.len() as u64, t.dirs.len() as u64 - 1, t.data_bytes());
    let mut stats = OptimizeStats {
        files,
        dirs,
        data_bytes: data,
        written: 0,
        old_cluster: old_cs,
        cluster: p.g.cluster,
        already,
        resumed: false,
        temp: None,
        phases: Vec::new(),
        secs: 0.0,
    };
    let info = |temp: Option<&Path>, already: bool| {
        format!(
            r#"{{"temp":{},"cluster":{old_cs},"new_cluster":{},"label":{},"already_optimized":{already},"resume":false}}"#,
            temp.map_or("null".into(), |p| crate::events::json_str(&p.display().to_string())),
            p.g.cluster,
            crate::events::json_str(&label)
        )
    };
    if already && !o.force {
        let detail = format!(
            "optimize: {what} is already optimized ({files} files, {dirs} dirs, {} of data, cluster {}): every file \
             is contiguous and in place, and the cluster size is the optimizer's choice; nothing to do (--force \
             rebuilds anyway)",
            human_bytes(data as f64),
            human_bytes(old_cs as f64)
        );
        run.tick.emit(Event::Plan {
            op: Op::Optimize,
            files,
            dirs,
            bytes: data,
            write_bytes: 0,
            detail,
            strategy: Some(info(None, true)),
            sync: None,
        });
        stats.secs = t0.elapsed().as_secs_f64();
        run.tick.emit(Event::Done { files, bytes: data, secs: stats.secs });
        return Ok(stats);
    }
    let need = data + (files + dirs) * PER_ENTRY + MARGIN;
    let base = pick_base(&cands, need, &avoid, &what, o.temp_volatile, o.dry_run).map_err(untouched)?;
    let tmp = base.join(temp_name(&key));
    let detail = format!(
        "optimize: {files} files, {dirs} dirs, {} of data on {what} (label {label:?}, cluster {}) → rebuilt as one \
         contiguous flashcp layout with cluster {}, writing {} │ temporary copy in {} ({} needed) │ kept: contents, \
         names, empty dirs, modification times, label; lost: file attributes, creation and access times │ if it is \
         interrupted: {FORMAT_WARNING}",
        human_bytes(data as f64),
        human_bytes(old_cs as f64),
        human_bytes(p.g.cluster as f64),
        human_bytes(p.write_bytes as f64),
        tmp.display(),
        human_bytes(need as f64)
    );
    run.tick.emit(Event::Plan {
        op: Op::Optimize,
        files,
        dirs,
        bytes: data,
        write_bytes: p.write_bytes,
        detail,
        strategy: Some(info(Some(&tmp), already)),
        sync: None,
    });
    stats.temp = Some(tmp.clone());
    if o.dry_run {
        stats.secs = t0.elapsed().as_secs_f64();
        run.tick.emit(Event::Done { files, bytes: data, secs: stats.secs });
        return Ok(stats);
    }
    drop(nodes);

    // ── pull + verify: the device is only read; on any error the copy is deleted ──
    run.steps = 5;
    let st = State {
        phase: Phase::None,
        label: label.clone(),
        orig,
        serial,
        job: random_id().map_err(untouched)?,
        geom,
        device: key.clone(),
        size: ctx.dev_len,
    };
    // Created here, exclusively (EEXIST if anything appeared since the lookup above): only a
    // directory this run created is ever deleted on error.
    fs::DirBuilder::new()
        .recursive(true)
        .create(&base)
        .and_then(|_| fs::DirBuilder::new().mode(0o700).create(&tmp))
        .with_context(|| format!("create {}", tmp.display()))
        .and_then(|_| check_ours(&tmp))
        .map_err(untouched)?;
    // Not removed on failure: another run may have taken it over in the instant since mkdir.
    let _lock = lock_dir(&tmp).map_err(untouched)?;
    let copied = (|| -> Result<Manifest> {
        run.phase("pull");
        let tree = tmp.join("tree");
        let dup = ctx.held.try_clone().context("dup target fd")?;
        let (_, pulled) =
            pull_file_hashed(dup, ctx.dev_len, &what, &tree, &PullOpts::default(), &mut forward(&mut run.tick))?;
        if let Ok(rel) = std::env::var(CORRUPT_ENV) {
            let f = fs::OpenOptions::new().read(true).write(true).open(tree.join(rel))?;
            let mut b = [0u8];
            std::os::unix::fs::FileExt::read_exact_at(&f, &mut b, 0)?;
            std::os::unix::fs::FileExt::write_all_at(&f, &[b[0] ^ 0xFF], 0)?;
        }
        // Read-only from here on, before it is hashed: what verify proves is what the build
        // reads (which hashes it again as it reads it).
        seal(&tree)?;
        save_state(&tmp, &State { phase: Phase::Pulled, ..st.clone() })?;
        hook("pull")?;

        run.phase("verify");
        // Pull read and hashed every byte of the drive's files; that hash is the drive's
        // manifest. It must list exactly what the metadata walk lists.
        let d = t.listing().diff(&pulled.manifest.without_hashes(), "the drive", "what pull read");
        ensure!(d.is_empty(), "pull did not read what the drive lists ({} differences): {}", d.len(), first_diffs(&d));
        // Only if a device read had to be retried is the drive's data read again.
        let reread = pulled.retries > 0 || std::env::var_os(VERIFY_DEVICE_ENV).is_some();
        let (dev, local) = if reread {
            let (pd, pl) = (Progress::default(), Progress::default());
            let held = &ctx.held;
            let (tr, tree, strat) = (&t, &tree, &pulled.strategy);
            // The device and the local disk are read at the same time; progress is the slower.
            let (dev, local) = run.with_progress(
                || {
                    let f = pd.files.load(Ordering::Relaxed).min(pl.files.load(Ordering::Relaxed));
                    (f, pd.bytes.load(Ordering::Relaxed).min(pl.bytes.load(Ordering::Relaxed)))
                },
                || {
                    std::thread::scope(|s| {
                        let d = s.spawn(|| hash_volume(held, tr, strat, &pd));
                        let l = hash_tree(tree, &pl);
                        (d.join().expect("device hash thread panicked"), l)
                    })
                },
            );
            let dev = dev.context("hash the drive")?;
            let d = dev.diff(&pulled.manifest, "the drive", "what pull read");
            ensure!(
                d.is_empty(),
                "the drive reads differently now than while it was copied ({} differences): {}",
                d.len(),
                first_diffs(&d)
            );
            (dev, local)
        } else {
            (pulled.manifest, hash_local(&mut run, tree.as_path()))
        };
        let local = local.context("hash the temporary copy")?;
        let d = dev.diff(&local, "the drive", "the temporary copy");
        ensure!(
            d.is_empty(),
            "the temporary copy does not match the drive ({} differences): {}",
            d.len(),
            first_diffs(&d)
        );
        write_durable(&tmp, "manifest", dev.encode().as_bytes())?;
        save_state(&tmp, &State { phase: Phase::Verified, ..st.clone() })?;
        hook("verify")?;
        Ok(dev)
    })();
    let manifest = match copied {
        Ok(m) => m,
        Err(e) => {
            let _ = remove_temp(&tmp);
            return Err(e.context(format!(
                "optimize stopped before writing anything: your data on {what} is untouched (the temporary copy \
                 in {} was deleted)",
                tmp.display()
            )));
        }
    };
    drop(t);
    stats.phases = rebuild(&mut run, &ctx, tgt, &base, &tmp, &st, &manifest, true)?;
    stats.written = p.write_bytes;
    stats.secs = t0.elapsed().as_secs_f64();
    run.tick.emit(Event::Done { files, bytes: data, secs: stats.secs });
    Ok(stats)
}

/// Build from `<tmp>/tree` onto the held target, check the result against `manifest`, clean
/// up. `fresh`: this run just verified the copy (state is `verified`; a failure before
/// `building` is saved still leaves the device untouched).
#[allow(clippy::too_many_arguments)]
fn rebuild(
    run: &mut Run<'_>,
    ctx: &Ctx,
    tgt: Target,
    base: &Path,
    tmp: &Path,
    st: &State,
    manifest: &Manifest,
    fresh: bool,
) -> Result<Vec<(&'static str, f64)>> {
    let what = &ctx.what;
    let hint = || recovery(what, tmp, base, &st.label, ctx.fd);
    let tree = tmp.join("tree");
    run.phase("build");
    // Re-scan the copy exactly as build will, check that it lists exactly the manifest, plan
    // it, and put the copy (data, directories, modes, manifest) on disk, all before the device
    // is touched.
    let prepared = (|| -> Result<(Vec<Node>, Expected)> {
        seal(&tree)?; // (already, unless a resumed copy predates it)
        let mut nodes = scan_tree(&tree, &upcase_map()).context("scan the temporary copy")?;
        let (listed, expect) = expected(&nodes, &tree, manifest)?;
        let d = manifest.without_hashes().diff(&listed, "the manifest", "the temporary copy");
        ensure!(
            d.is_empty(),
            "the temporary copy {} no longer lists what was verified ({} differences): {}",
            tree.display(),
            d.len(),
            first_diffs(&d)
        );
        let (p, _) =
            choose_cluster(&mut nodes, &tgt, &build_opts(&st.label, None, None)).context("plan the rebuild")?;
        let g = Geom::planned(&p, &tgt);
        ensure!(
            g == st.geom,
            "the rebuild from the temporary copy would lay out the volume differently than this optimize planned \
             ({}); not writing it",
            st.geom.diff(&g)
        );
        sync_temp(tmp)?;
        Ok((nodes, expect))
    })();
    let (nodes, expect) = match prepared {
        Ok(n) => n,
        Err(e) if fresh => {
            let _ = remove_temp(tmp);
            return Err(e.context(format!(
                "optimize stopped before writing anything: your data on {what} is untouched (the temporary copy \
                 was deleted)"
            )));
        }
        Err(e) => return Err(e.context(hint())),
    };
    if fresh && let Err(e) = save_state(tmp, &State { phase: Phase::Building, ..st.clone() }) {
        let _ = remove_temp(tmp);
        return Err(e.context(format!("optimize stopped before writing anything: your data on {what} is untouched")));
    }
    // From here on the verified copy in <tmp> is never deleted until the check passes.
    hook("building").map_err(|e| e.context(hint()))?;
    not_looped(ctx, "build").map_err(|e| e.context(hint()))?;
    // Test hook: change the copy now ("REL"), or after N files went to the device ("N:REL").
    let tampering = std::env::var(TAMPER_ENV).ok();
    let (tamper_after, tamper_rel) = match tampering.as_deref().map(|t| t.split_once(':')) {
        Some(Some((n, rel))) => (n.parse::<usize>().ok(), Some(rel.to_string())),
        Some(None) => (None, tampering.clone()),
        None => (None, None),
    };
    if let (None, Some(rel)) = (tamper_after, &tamper_rel) {
        tamper(&tree, rel);
    }
    let after_file: Option<Box<dyn FnMut(usize)>> = match (tamper_after, tamper_rel) {
        (Some(n), Some(rel)) => {
            let tree = tree.clone();
            Some(Box::new(move |k| {
                if k == n {
                    tamper(&tree, &rel)
                }
            }))
        }
        _ => None,
    };
    if std::env::var_os(TRACE_READS_ENV).is_some() {
        eprintln!("flashcp-device-write");
    }
    set_direct(&tgt.file, tgt.direct);
    let guard = Guard { expect, after_file };
    build_target_guarded(
        nodes,
        tgt,
        &build_opts(&st.label, Some(st.serial), Some(st.job)),
        &mut forward(&mut run.tick),
        Some(guard),
    )
    .map_err(|e| match e.downcast_ref::<SourceChanged>() {
        Some(SourceChanged(p)) => {
            let rel = p.strip_prefix(&tree).unwrap_or(p).display().to_string();
            e.context(Recovery(format!(
                "the temporary copy in {} changed after it was verified ({rel}); the rebuild of {what} was \
                     stopped and left unfinished (not mountable). The copy was NOT deleted: restore it, then re-run \
                     `flashcp optimize` (it resumes), or copy it back with `flashcp add {} {}` after formatting it \
                     with flashcp. {FORMAT_WARNING}",
                tmp.display(),
                tree.display(),
                if ctx.fd { "<target>" } else { what.as_str() }
            )))
        }
        None => e.context(hint()),
    })?;
    hook("build").map_err(|e| e.context(hint()))?;

    run.phase("check");
    let checked = (|| -> Result<()> {
        not_looped(ctx, "check")?;
        let (v, t) = open_walk(&ctx.held, ctx.dev_len).context("the rebuilt drive does not parse as a clean volume")?;
        ensure!(v.label == st.label, "the rebuilt volume's label is {:?}, not {:?}", v.label, st.label);
        ensure!(
            serial_of(&v.sector0) == st.serial && job_of(&v.sector0) == Some(st.job),
            "the drive does not hold the volume this optimize wrote"
        );
        let strat = read_strategy(&ctx.held, &v);
        drop(v);
        let m = hash_device(run, &ctx.held, &t, &strat).context("hash the rebuilt drive")?;
        let d = manifest.diff(&m, "the original", "the rebuilt drive");
        ensure!(
            d.is_empty(),
            "the rebuilt drive differs from the original ({} differences): {}",
            d.len(),
            first_diffs(&d)
        );
        Ok(())
    })();
    checked.map_err(|e| e.context(hint()))?;

    run.phase("cleanup");
    cleanup(tmp).map_err(|e| {
        e.context(format!("{what} is optimized and checked; only removing the temporary copy {} failed", tmp.display()))
    })?;
    run.end_phase();
    Ok(std::mem::take(&mut run.phases))
}

/// Per node: (size, hash) its content must have.
type Expected = Vec<(u64, u128)>;

/// The listing of the scanned copy (hashes 0) and, per node, the (size, hash) the manifest
/// says its content must have (directories: (0, 0)).
fn expected(nodes: &[Node], tree: &Path, m: &Manifest) -> Result<(Manifest, Expected)> {
    let mut listed = Manifest::default();
    let mut expect = Vec::with_capacity(nodes.len());
    for n in nodes {
        let rel = n.src.strip_prefix(tree).context("scanned path outside the copy")?;
        let rel = rel.to_str().ok_or_else(|| anyhow!("{}: name is not UTF-8", n.src.display()))?.to_string();
        if rel.is_empty() {
            expect.push((0, 0)); // the root
        } else if n.is_dir {
            listed.dirs.insert(rel, Some(n.mtime));
            expect.push((0, 0));
        } else {
            expect.push(m.files.get(&rel).map_or((u64::MAX, 0), |f| (f.len, f.hash)));
            listed.files.insert(rel, crate::verify::FileSum { len: n.size, hash: 0, mtime: Some(n.mtime) });
        }
    }
    Ok((listed, expect))
}

/// state=done first (a crash from here on is "finished"), then the tree, the manifest, the
/// state file and the directory.
fn cleanup(tmp: &Path) -> Result<()> {
    let st = State::load(tmp)?.ok_or_else(|| anyhow!("state file vanished"))?;
    save_state(tmp, &State { phase: Phase::Done, ..st })?;
    let tree = tmp.join("tree");
    if tree.exists() {
        unseal(&tree)?;
        fs::remove_dir_all(&tree).with_context(|| format!("remove {}", tree.display()))?;
    }
    hook("cleanup")?;
    for f in ["manifest", "state", "lock"] {
        match fs::remove_file(tmp.join(f)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    fs::remove_dir(tmp).with_context(|| format!("remove {}", tmp.display()))
}

/// Continues a run that stopped in state `building` (see the module docs).
#[allow(clippy::too_many_arguments)]
fn resume(
    run: &mut Run<'_>,
    ctx: Ctx,
    tgt: Target,
    base: &Path,
    tmp: &Path,
    st: State,
    o: &OptimizeOpts,
    t0: Instant,
) -> Result<OptimizeStats> {
    let what = &ctx.what;
    let hint = || recovery(what, tmp, base, &st.label, ctx.fd);
    let tree = tmp.join("tree");
    let saved = fs::read_to_string(tmp.join("manifest"))
        .map_err(anyhow::Error::from)
        .and_then(|s| Manifest::decode(&s))
        .with_context(|| {
            format!(
                "an interrupted optimize of {what} had started rebuilding it, but its manifest {} is missing or \
                 damaged, so flashcp cannot prove the copy in {} is complete and will not continue on its own. \
                 Check the drive first: if it still opens and holds your files, keep it and delete {} (that copy may \
                 be older). Only if it does not, rebuild it from the copy with `flashcp build {} <device> --label \
                 {:?} --yes-erase`",
                tmp.join("manifest").display(),
                tree.display(),
                tmp.display(),
                tree.display(),
                st.label
            )
        })?;
    let (files, dirs, data) = (saved.files.len() as u64, saved.dirs.len() as u64, saved.bytes());
    // What a rebuild from the copy would write (a damaged copy fails later, with the hint).
    let planned = scan_tree(&tree, &upcase_map()).and_then(|mut n| {
        choose_cluster(&mut n, &tgt, &build_opts(&st.label, None, None)).map(|(p, _)| (p.g.cluster, p.write_bytes))
    });
    let (cluster, write_bytes) = planned.as_ref().map_or((0, 0), |x| *x);
    run.tick.emit(Event::Plan {
        op: Op::Optimize,
        files,
        dirs,
        bytes: data,
        write_bytes,
        detail: format!(
            "optimize: resuming an interrupted optimize of {what} from {} ({files} files, {dirs} dirs, {} of data) → \
             cluster {}, writing up to {}",
            tmp.display(),
            human_bytes(data as f64),
            human_bytes(cluster as f64),
            human_bytes(write_bytes as f64)
        ),
        strategy: Some(format!(
            r#"{{"temp":{},"cluster":null,"new_cluster":{cluster},"label":{},"already_optimized":false,"resume":true}}"#,
            crate::events::json_str(&tmp.display().to_string()),
            crate::events::json_str(&st.label)
        )),
        sync: None,
    });
    let mut stats = OptimizeStats {
        files,
        dirs,
        data_bytes: data,
        written: 0,
        old_cluster: 0,
        cluster,
        already: false,
        resumed: true,
        temp: Some(tmp.to_path_buf()),
        phases: Vec::new(),
        secs: 0.0,
    };
    let finish = |run: &mut Run<'_>, mut stats: OptimizeStats| {
        run.end_phase();
        stats.phases = std::mem::take(&mut run.phases);
        stats.secs = t0.elapsed().as_secs_f64();
        run.tick.emit(Event::Done { files, bytes: data, secs: stats.secs });
        stats
    };

    // What is on the drive now? Judged by the raw boot sector (see the module docs): only the
    // volume this optimize was writing (our saved serial), or the untouched original with
    // exactly the saved content, is ever written; anything else is refused.
    let walked = open_walk(&ctx.held, ctx.dev_len);
    let s0 = read_raw(&ctx.held, 0, 4096.min(ctx.dev_len));
    let bps = if is_exfat(&s0) && (9..=12).contains(&s0[108]) { 1usize << s0[108] } else { SECTOR as usize };
    let original = is_exfat(&s0) && s0.len() >= bps && fingerprint(&s0[..bps]) == st.orig;
    // build writes the backup boot sector (12 × 512 bytes in) first, and the main one last
    // (zeros until then): either one with our serial, our job id and our planned geometry
    // means our build had started. Our serial with anything else is refused.
    let backup = read_raw(&ctx.held, 12 * SECTOR, SECTOR);
    let keep = format!(
        "The temporary copy in {} is kept; it holds the files as they were when that optimize started (to delete \
         it: {})",
        tmp.display(),
        rm_hint(tmp)
    );
    let mut ours = false;
    for (name, s) in [("main", &s0), ("backup", &backup)] {
        if !is_exfat(s) || serial_of(s) != st.serial {
            continue;
        }
        let mut wrong = Vec::new();
        if job_of(s) != Some(st.job) {
            wrong.push(match job_of(s) {
                Some(j) => {
                    format!("it was written by another optimize job ({}, not {})", hex_bytes(&j), hex_bytes(&st.job))
                }
                None => "it carries no flashcp optimize job id".to_string(),
            });
        }
        let g = Geom::of(s);
        if g != st.geom {
            wrong.push(format!("its geometry differs from the planned one ({})", st.geom.diff(&g)));
        }
        if !wrong.is_empty() {
            bail!(
                "the {name} boot sector of {what} has the serial number of the volume the interrupted optimize was \
                 writing, but {}: it is not that optimize's volume, so flashcp will not write to it. {keep}. If the \
                 drive holds what you want, check that the copy holds nothing you still need and delete {}",
                wrong.join("; "),
                tmp.display()
            );
        }
        ours = true;
    }
    match walked {
        Ok((v, t)) if original || ours => {
            stats.old_cluster = v.cs;
            let strat = read_strategy(&ctx.held, &v);
            drop(v);
            // Finished: check, cleanup. Still the original volume: check, verify, build, check, cleanup.
            run.steps = if original { 5 } else { 2 };
            run.phase("check");
            let m = hash_device(run, &ctx.held, &t, &strat).context("hash the drive")?;
            let d = saved.diff(&m, "the saved manifest", "the drive");
            if !d.is_empty() && original {
                bail!(
                    "{what} still holds the original volume, but its files changed after the interrupted optimize \
                     made its copy ({} differences, e.g. {}): the drive holds newer data than the copy. flashcp will \
                     not erase it; keep the drive as it is. The copy in {} is stale: check that it holds nothing you \
                     still need, delete it, and run optimize again to start over",
                    d.len(),
                    first_diffs(&d[..d.len().min(3)]),
                    tmp.display()
                );
            }
            if !d.is_empty() {
                bail!(
                    "{what} holds the volume the interrupted optimize built, complete, but its files differ from \
                     that optimize's copy ({} differences, e.g. {}): it was probably written to since, so flashcp \
                     will not erase it. {keep}. Check the drive and the copy, and delete {} when you no longer need \
                     it",
                    d.len(),
                    first_diffs(&d[..d.len().min(3)]),
                    tmp.display()
                );
            }
            if !original {
                // The rebuilt volume, complete and checked: only the cleanup was missing.
                stats.cluster = stats.old_cluster;
                if o.dry_run {
                    return Ok(finish(run, stats));
                }
                eprintln!("note: the interrupted optimize of {what} had finished; removing its temporary copy");
                run.phase("cleanup");
                cleanup(tmp).with_context(|| format!("{what} is optimized; only removing {} failed", tmp.display()))?;
                return Ok(finish(run, stats));
            }
            eprintln!("note: {what} still holds the original volume; resuming the interrupted optimize");
        }
        Err(e) if ours => {
            eprintln!("note: {what} holds an unfinished rebuild ({e:#}); resuming the interrupted optimize");
            run.steps = 4;
        }
        Err(e) if original => bail!(
            "{what} still holds the original volume, but it is not clean now ({e:#}): it was mounted, and perhaps \
             written to, after the interrupted optimize made its copy, so the drive may hold newer data than the \
             copy. flashcp will not erase it. Plug it into a computer, let it check the drive (or run fsck.exfat), \
             eject it safely, and run optimize again: if its files are unchanged it continues, otherwise keep the \
             drive and delete the stale copy ({})",
            rm_hint(tmp)
        ),
        _ => bail!(
            "{what} holds neither the original volume nor the one the interrupted optimize was writing (reformatted, \
             written by another program, or a different card or drive?), so flashcp will not write to it. {keep}. \
             If the drive holds what you want, check that the copy holds nothing you still need and delete {}",
            tmp.display()
        ),
    }
    if o.dry_run {
        return Ok(finish(run, stats));
    }
    planned.with_context(hint)?;
    run.phase("verify");
    seal(&tree).with_context(hint)?;
    let local = hash_local(run, &tree).with_context(hint)?;
    let d = saved.diff(&local, "the saved manifest", "the temporary copy");
    if !d.is_empty() {
        let msg = format!(
            "an interrupted optimize left {what} unfinished, and its temporary copy {} no longer matches its \
             manifest ({} differences: {}). flashcp will not write that copy to the drive. It is kept as it is: \
             restore the files that changed, then run optimize again. {FORMAT_WARNING}",
            tree.display(),
            d.len(),
            first_diffs(&d)
        );
        return Err(anyhow::Error::new(Recovery(msg)));
    }
    stats.phases = rebuild(run, &ctx, tgt, base, tmp, &st, &saved, false)?;
    stats.written = write_bytes;
    stats.secs = t0.elapsed().as_secs_f64();
    run.tick.emit(Event::Done { files, bytes: data, secs: stats.secs });
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips() {
        let d = std::env::temp_dir().join(format!("flashcp-optstate-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        assert!(State::load(&d).unwrap().is_none());
        let geom =
            Geom { vol_len: 131072, part_off: 2048, fat_off: 128, fat_len: 8, heap_off: 256, clusters: 4000, shift: 3 };
        let st = State {
            phase: Phase::Building,
            label: "Mÿ\n=Stick".into(),
            orig: 0xDEAD_BEEF_0123,
            serial: 0x89AB_CDEF,
            job: *b"0123456789abcdef",
            geom,
            device: "usb-X\n=1-p2048".into(),
            size: 64 << 20,
        };
        save_state(&d, &st).unwrap();
        let back = State::load(&d).unwrap().unwrap();
        assert_eq!(
            (back.phase, back.label, back.orig, back.serial, back.job, back.geom, back.device, back.size),
            (st.phase.clone(), st.label.clone(), st.orig, st.serial, st.job, st.geom, st.device.clone(), st.size)
        );
        // A state without the serial or the job id is not trusted.
        fs::write(d.join("state"), st.encode().replace("serial=89abcdef\n", "")).unwrap();
        assert!(State::load(&d).is_err());
        let enc = st.encode();
        let job_line = enc.lines().find(|l| l.starts_with("job=")).unwrap().to_string();
        fs::write(d.join("state"), enc.replace(&format!("{job_line}\n"), "")).unwrap();
        assert!(State::load(&d).is_err());
        // An older format (version 2: serial only) is unreadable.
        fs::write(d.join("state"), enc.replace(STATE_MAGIC, "flashcp-optimize-state 2")).unwrap();
        assert!(State::load(&d).is_err());
        fs::write(d.join("state"), "garbage").unwrap();
        assert!(State::load(&d).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn job_id_and_geometry_come_from_the_boot_sector() {
        let a = random_id().unwrap();
        assert_ne!(a, random_id().unwrap());
        let mut s = vec![0xF4u8; 512];
        assert_eq!(job_of(&s), None);
        s[120..131].copy_from_slice(crate::exfat::JOB_TAG);
        s[131..147].copy_from_slice(&a);
        assert_eq!(job_of(&s), Some(a));
        s[64..72].copy_from_slice(&2048u64.to_le_bytes());
        s[72..80].copy_from_slice(&131072u64.to_le_bytes());
        for (o, v) in [(80, 128u32), (84, 8), (88, 256), (92, 4000)] {
            s[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        s[109] = 3;
        let g =
            Geom { vol_len: 131072, part_off: 2048, fat_off: 128, fat_len: 8, heap_off: 256, clusters: 4000, shift: 3 };
        assert_eq!(Geom::of(&s), g);
        assert_eq!(Geom::decode(&g.encode()), Some(g));
        let h = Geom { fat_len: 9, ..g };
        assert_eq!(g.diff(&h), "FatLength 9 instead of 8");
        assert!(Geom::decode("1,2,3").is_none());
    }

    #[test]
    fn recovery_is_found_under_more_context() {
        let e = anyhow!("write failed").context(Recovery("re-run it".into())).context("outer");
        assert_eq!(recovery_of(&e), Some("re-run it"));
        assert_eq!(recovery_of(&anyhow!("other")), None);
        assert_eq!(recovery_of(&anyhow::Error::new(Recovery("x".into()))), Some("x"));
    }

    #[test]
    fn only_known_disk_filesystems_hold_the_copy() {
        for t in [0xEF53i64, 0x5846_5342, 0x9123_683E, 0xF2F5_2010, 0x2FC1_2FC1, 0x2011_BAB0] {
            assert_eq!(not_disk_backed(t), None, "{t:#x}");
        }
        assert!(not_disk_backed(TMPFS_MAGIC).unwrap().contains("tmpfs"));
        assert!(not_disk_backed(RAMFS_MAGIC).is_some());
        assert!(not_disk_backed(OVERLAYFS_MAGIC as i64).unwrap().contains("overlayfs"));
        assert!(not_disk_backed(0x6969).is_some(), "nfs"); // network / unknown: needs --temp
        assert!(not_disk_backed(0x9fa0).is_some(), "proc");
        // f_type as a sign-extended 32-bit word (32-bit platforms) is still recognized.
        assert_eq!(not_disk_backed(0xF2F5_2010u32 as i32 as i64), None);
    }

    #[test]
    fn new_serial_differs_from_the_original() {
        let mut s0 = vec![0u8; 512];
        for _ in 0..100 {
            let s = new_serial(&s0);
            assert!(s != 0 && s != serial_of(&s0));
            s0[100..104].copy_from_slice(&s.to_le_bytes());
            assert_ne!(new_serial(&s0), s);
        }
    }

    #[test]
    fn a_locked_temp_dir_is_refused() {
        let d = std::env::temp_dir().join(format!("flashcp-optlock-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let held = lock_dir(&d).unwrap();
        let e = lock_dir(&d).unwrap_err().to_string();
        assert!(e.contains("in use by another"), "{e}");
        drop(held);
        lock_dir(&d).unwrap();
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn fingerprint_ignores_flags_and_percent_only() {
        let s: Vec<u8> = (0..512).map(|i| i as u8).collect();
        let mut t = s.clone();
        t[106] ^= 2;
        t[112] = 99;
        assert_eq!(fingerprint(&s), fingerprint(&t));
        t[100] ^= 1; // serial
        assert_ne!(fingerprint(&s), fingerprint(&t));
    }
}
