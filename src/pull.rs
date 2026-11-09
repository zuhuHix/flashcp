//! `flashcp pull`: copy files OFF an exFAT volume into a local directory.
//!
//! The volume is opened read-only and parsed with the validating reader shared with `add`
//! ([`crate::volume`]). Then:
//!
//! 1. **Plan.** Every file's clusters (NoFatChain runs and FAT chains, up to its
//!    ValidDataLength) become extents (physical offset, length, file, file offset). The extents
//!    are sorted by physical offset and coalesced into chunks of at most `chunk` bytes, reading
//!    through gaps of up to `gap` bytes (other files' slack, skipped clusters) instead of
//!    issuing a new request.
//! 2. **Read.** `inflight` reader threads read the chunks in ascending order with large
//!    aligned (O_DIRECT where available) reads, so the device sees a few big sequential
//!    requests in flight.
//! 3. **Scatter.** `writers` threads write each chunk's extents into the destination files with
//!    `pwrite` at the right offset. A file is created on its first extent (preallocated with
//!    fallocate, sized to DataLength so bytes past ValidDataLength read as zeros) and closed
//!    with its mtime set after its last one. No per-file fsync: one `syncfs` at the end.
//!
//! The source volume is UNTRUSTED (any USB stick). Every directory's and file's allocation is
//! walked and marked in one visited-cluster set, so FAT loops, cross-links and directory cycles
//! fail at once and the planned data can never exceed the cluster heap. Entries, depth, path
//! length and fragments have hard limits, names must be single safe components, the
//! destination's free space is checked before anything is written, and nothing past
//! VolumeLength is ever read. The destination is written only through directory fds
//! ([`crate::dirfd`]: openat2 RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS, O_NOFOLLOW), so a symlink
//! swapped in concurrently cannot redirect a write outside it, and `--overwrite` writes a temp
//! file and renames it over the old one (never truncating through a hard link).
//!
//! A mounted source is read as it is on the device: fine when nothing is writing to it (the
//! kernel driver's metadata goes through the block device's page cache, which buffered
//! metadata reads see, and O_DIRECT data reads flush that cache first), but files written
//! since the last `sync` may be missing or partial. An unmounted volume is preferred, and a
//! read-write mount gets a warning.

use crate::dirfd;
use crate::events::{Event, Op, Sink, Ticker};
use crate::exfat::{FlagGuard, SYS_BLOCK, TargetSpec, blk_size, open_target_fd, related_devs, set_direct};
use crate::util::{AlignedBuf, human_bytes};
use crate::volume::*;
use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{RecvTimeoutError, bounded};
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Read buffers in use (read and not yet written out) never exceed this many bytes.
pub const READ_BUDGET: usize = 64 << 20;

/// Alignment of every device read (O_DIRECT needs the logical block size; 4 KB covers it).
const ALIGN: u64 = 4096;

#[derive(Clone, Debug)]
pub struct PullOpts {
    /// Directory inside the volume to copy from ("a/b"); its contents land in the destination.
    pub from: Option<String>,
    /// Leave files that already exist in the destination alone.
    pub skip_existing: bool,
    /// Replace files that already exist in the destination.
    pub overwrite: bool,
    /// Largest single device read, in bytes.
    pub chunk: usize,
    /// Gaps between extents up to this many bytes are read through rather than skipped
    /// (None: chosen per device, see [`Strategy`]).
    pub gap: Option<u64>,
    /// Device reads in flight, at most [`READ_BUDGET`] bytes of them (None: chosen per device).
    pub inflight: Option<usize>,
    /// Buffered (page cache + readahead) reads instead of O_DIRECT (None: chosen per device).
    pub buffered: Option<bool>,
    /// Threads writing destination files.
    pub writers: usize,
}

impl Default for PullOpts {
    fn default() -> Self {
        PullOpts {
            from: None,
            skip_existing: false,
            overwrite: false,
            chunk: 8 << 20,
            gap: None,
            inflight: None,
            buffered: None,
            writers: 4,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PullStats {
    pub files: u64,
    pub dirs: u64,
    pub skipped: u64,
    /// File bytes written to the destination (DataLength, zeros past ValidDataLength included).
    pub data_bytes: u64,
    /// Bytes read from the volume in the data phase (gaps and alignment included).
    pub read_bytes: u64,
    pub reads: u64,
    pub cluster: u64,
    pub secs: f64,
}

/// Hard limits on what an untrusted volume can make pull hold in memory or create. Generous
/// for real volumes, finite for crafted ones (memory stays about bitmap/8 + O(entries)).
pub const MAX_ENTRIES: u64 = 8 << 20;
pub const MAX_EXTENTS: usize = 16 << 20;
pub const MAX_DEPTH: usize = 256;
/// Longest relative path created under the destination (PATH_MAX).
pub const MAX_PATH: usize = 4096;
/// Test hook: a shell command run after planning, before anything is written.
const AFTER_PLAN_ENV: &str = "FLASHCP_TEST_AFTER_PLAN";
/// Test hook: print every data read ("flashcp-read OFFSET LEN") to stderr.
const TRACE_READS_ENV: &str = "FLASHCP_TEST_TRACE_READS";

/// A destination directory: index 0 is the destination root itself.
struct DirNode {
    parent: u32,
    name: CString,
    mtime: Option<(i64, u32)>,
}

/// How the volume is read, chosen from the device (see [`choose_strategy`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Strategy {
    /// "uas", "usb-storage" (Bulk-Only Transport: one command at a time), or "other".
    pub transport: String,
    pub gap: u64,
    pub inflight: usize,
    pub direct: bool,
    /// Measured by the micro-probe (BOT only): extra cost of one more request in a forward
    /// stream, and sequential bandwidth.
    pub overhead_ms: Option<f64>,
    pub mbps: Option<f64>,
}

impl Strategy {
    pub fn to_json(&self) -> String {
        let f = |v: Option<f64>| v.map_or("null".to_string(), |v| format!("{v:.3}"));
        format!(
            r#"{{"transport":{},"gap_kb":{},"inflight":{},"direct":{},"overhead_ms":{},"mbps":{}}}"#,
            crate::events::json_str(&self.transport),
            self.gap >> 10,
            self.inflight,
            self.direct,
            f(self.overhead_ms),
            f(self.mbps)
        )
    }
}

/// Queued transports (UAS, NVMe, SATA): many requests in flight hide the per-request cost,
/// so only small gaps are worth reading through (measured on a UAS SSD: 16–64 KB best).
const QUEUED_GAP: u64 = 32 << 10;
const QUEUED_INFLIGHT: usize = 32;
/// Bulk-Only Transport executes one command at a time; a second request only keeps the next
/// command ready (measured on a USB 2 stick: 2 ≈ 4 > 1).
const BOT_INFLIGHT: usize = 2;
/// The BOT gap is latency × bandwidth, clamped to this.
const BOT_GAP_MAX: u64 = 4 << 20;

/// Measures, with O_DIRECT inside the cluster heap, the sequential bandwidth (one 4 MB read)
/// and the extra cost of one more request in a forward stream: 256 back-to-back 4 KB reads
/// against one 1 MB read of the next megabyte. That cost, not random-read latency, is what
/// reading through a gap saves. About 0.5 s on a USB 2 stick. Returns (overhead s, bytes/s).
/// Read-only and bounded (≈ 6 MB in 259 reads); there is no wall-clock timeout, so a device
/// that hangs on a read hangs here exactly as it would in the data phase.
fn micro_probe(file: &File, v: &Vol) -> Option<(f64, f64)> {
    let start = v.heap_off.div_ceil(ALIGN) * ALIGN;
    let end = (v.heap_off + v.cc * v.cs) / ALIGN * ALIGN;
    if end < start + (6 << 20) || !set_direct(file, true) {
        return None;
    }
    let mut buf = AlignedBuf::new(4 << 20);
    let timed = |buf: &mut [u8], off: u64| -> Option<f64> {
        let t = Instant::now();
        (read_full(file, buf, off).ok()? == buf.len()).then(|| t.elapsed().as_secs_f64().max(1e-6))
    };
    timed(&mut buf.as_mut()[..4096], start)?; // wake the device up
    let bw = (4u64 << 20) as f64 / timed(buf.as_mut(), start)?;
    let one = timed(&mut buf.as_mut()[..1 << 20], start + (4 << 20))?;
    let mut many = 0.0;
    for k in 0..256u64 {
        many += timed(&mut buf.as_mut()[..4096], start + (5 << 20) + k * 4096)?;
    }
    Some(((many - one).max(0.0) / 255.0, bw))
}

/// Picks gap, depth and I/O mode from the source's transport (sysfs), and on BOT from a
/// micro-probe: a gap is read through only when reading it costs less than the request it
/// saves (gap / bandwidth < per-request overhead). Explicit options override every choice.
fn choose_strategy(file: &File, v: &Vol, o: &PullOpts) -> Strategy {
    let dev = file.metadata().map(|m| if m.file_type().is_block_device() { m.rdev() } else { m.dev() }).unwrap_or(0);
    let transport = crate::probe::device_info_of(dev).transport.unwrap_or_else(|| "other".into());
    let mut s =
        Strategy { transport, gap: QUEUED_GAP, inflight: QUEUED_INFLIGHT, direct: true, overhead_ms: None, mbps: None };
    if s.transport == "usb-storage" {
        s.inflight = BOT_INFLIGHT;
        s.gap = 0;
        if o.gap.is_none()
            && let Some((lat, bw)) = micro_probe(file, v)
        {
            s.overhead_ms = Some(lat * 1e3);
            s.mbps = Some(bw / 1e6);
            s.gap = ((lat * bw) as u64 / ALIGN * ALIGN).min(BOT_GAP_MAX);
        }
    }
    if let Some(g) = o.gap {
        s.gap = g;
    }
    if let Some(n) = o.inflight {
        s.inflight = n.max(1);
    }
    if let Some(b) = o.buffered {
        s.direct = !b;
    }
    s
}

/// A volume name as a single, safe path component.
fn component(name: &[u16], parent: &str) -> Result<String> {
    let s = String::from_utf16(name).map_err(|_| {
        anyhow!("{parent}: a name is not valid UTF-16; refusing to guess a local name (run fsck.exfat)")
    })?;
    ensure!(
        !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\0']),
        "{parent}: entry name {s:?} cannot be used as a file name"
    );
    Ok(s)
}

fn systime((secs, ns): (i64, u32)) -> Option<SystemTime> {
    let d = Duration::new(secs.try_into().ok()?, ns);
    UNIX_EPOCH.checked_add(d)
}

/// (device, mount point, read-write) of a mount.
pub(crate) type MountState = (String, String, bool);

/// The first mount of the volume's device (or the loop device its image is attached to), if any.
fn mount_state(md: &fs::Metadata) -> Option<MountState> {
    let rdev = if md.file_type().is_block_device() {
        md.rdev()
    } else {
        let dev = crate::add::loop_device_of(md)?;
        fs::metadata(dev).ok()?.rdev()
    };
    let devs = related_devs(Path::new(SYS_BLOCK), &format!("{}:{}", libc::major(rdev), libc::minor(rdev))).ok()?;
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    info.lines().find_map(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        let dev = *f.get(2)?;
        let rw = f.get(5)?.split(',').any(|o| o == "rw");
        devs.iter().any(|d| d == dev).then(|| (dev.to_string(), f.get(4).unwrap_or(&"?").to_string(), rw))
    })
}

/// Opens the source read-only: (file, length, flag guard, mount state).
fn open_source(target: &TargetSpec) -> Result<(File, u64, FlagGuard, Option<MountState>)> {
    let (file, len, guard) = match target {
        TargetSpec::Fd(fd) => {
            let t = open_target_fd(*fd, false)?;
            (t.file, t.bytes, t._flags)
        }
        TargetSpec::Path(p) => {
            let f = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(p)
                .with_context(|| format!("open {}", p.display()))?;
            let md = f.metadata()?;
            let len = if md.file_type().is_block_device() {
                blk_size(&f)?
            } else if md.is_file() {
                md.len()
            } else {
                bail!("{} is neither a block device nor a regular image file", p.display());
            };
            let g = FlagGuard::new(&f)?;
            (f, len, g)
        }
    };
    let ms = mount_state(&file.metadata()?);
    Ok((file, len, guard, ms))
}

fn raise_nofile() {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) == 0 && r.rlim_cur < r.rlim_max {
            r.rlim_cur = r.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &r);
        }
    }
}

/// Aligned read buffers in power-of-two size classes, reused, with at most `budget` bytes
/// handed out at a time (one buffer is always allowed, whatever its size).
struct Pool {
    st: Mutex<PoolState>,
    cv: Condvar,
    budget: usize,
}

#[derive(Default)]
struct PoolState {
    used: usize,
    free: Vec<AlignedBuf>,
    free_bytes: usize,
    closed: bool,
}

impl Pool {
    fn new(budget: usize) -> Pool {
        Pool { st: Mutex::new(PoolState::default()), cv: Condvar::new(), budget }
    }

    /// A buffer of at least `len` bytes; None once the pool is closed.
    fn get(&self, len: usize) -> Option<AlignedBuf> {
        let cap = len.next_power_of_two().max(64 << 10);
        let mut g = self.st.lock().unwrap();
        while !g.closed && g.used > 0 && g.used + cap > self.budget {
            g = self.cv.wait(g).unwrap();
        }
        if g.closed {
            return None;
        }
        g.used += cap;
        if let Some(i) = g.free.iter().position(|b| b.as_ref().len() == cap) {
            g.free_bytes -= cap;
            return Some(g.free.swap_remove(i));
        }
        drop(g);
        Some(AlignedBuf::new(cap))
    }

    fn put(&self, b: AlignedBuf) {
        let mut g = self.st.lock().unwrap();
        let cap = b.as_ref().len();
        g.used -= cap;
        if g.free_bytes + cap <= self.budget {
            g.free_bytes += cap;
            g.free.push(b);
        }
        drop(g);
        self.cv.notify_all();
    }

    fn close(&self) {
        self.st.lock().unwrap().closed = true;
        self.cv.notify_all();
    }
}

/// Reads `buf.len()` bytes at `off`, or fewer at the end of the device. Returns bytes read.
fn read_full(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], off + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Coalesces extents (sorted by `phys`) into aligned reads of at most `max` bytes, reading
/// through gaps of up to `gap` bytes. Every extent must be at most `max - 2 * ALIGN` bytes.
fn coalesce(exts: &[Ext], max: usize, gap: u64) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < exts.len() {
        let start = exts[i].phys / ALIGN * ALIGN;
        let mut end = exts[i].phys + exts[i].len as u64;
        let mut j = i + 1;
        while j < exts.len() {
            let e = exts[j];
            let e_end = (e.phys + e.len as u64).max(end);
            if e.phys.saturating_sub(end) > gap || e_end.div_ceil(ALIGN) * ALIGN - start > max as u64 {
                break;
            }
            end = e_end;
            j += 1;
        }
        let len = (end.div_ceil(ALIGN) * ALIGN - start) as usize;
        debug_assert!(len <= max, "chunk of {len} bytes exceeds {max}");
        out.push(Chunk { start, len, exts: i..j });
        i = j;
    }
    out
}

/// One destination file.
struct Job {
    parent: u32,
    name: CString,
    len: u64,
    /// ValidDataLength: bytes read from the volume; the rest of `len` stays zero.
    valid: u64,
    mtime: Option<(i64, u32)>,
    /// An existing file is replaced (--overwrite): written to a temp file, renamed over it.
    replace: bool,
    st: Mutex<JobState>,
}

struct JobState {
    file: Option<File>,
    /// Bytes still to write (starts at ValidDataLength).
    left: u64,
    /// Temp file name (in the parent) while a replacement is being written.
    tmp: Option<CString>,
}

/// `len` bytes at physical offset `phys` belong at offset `foff` of file `file`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ext {
    phys: u64,
    foff: u64,
    len: u32,
    file: u32,
}

/// One device read: `len` bytes at `start` (both aligned), serving `exts`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Chunk {
    start: u64,
    len: usize,
    exts: std::ops::Range<usize>,
}

/// Pushes the extents of `bytes` bytes at physical `phys` (file offset `foff`), split so no
/// extent exceeds `max` or crosses physical offset `cut`.
fn push_run(exts: &mut Vec<Ext>, file: u32, mut phys: u64, mut foff: u64, mut bytes: u64, max: u64, cut: u64) {
    while bytes > 0 {
        let mut n = bytes.min(max);
        if phys < cut && phys + n > cut {
            n = cut - phys;
        }
        exts.push(Ext { phys, foff, len: n as u32, file });
        (phys, foff, bytes) = (phys + n, foff + n, bytes - n);
    }
}

/// The destination: the root directory fd and the planned tree under it.
struct Dest {
    root: OwnedFd,
    path: PathBuf,
    dirs: Vec<DirNode>,
}

impl Dest {
    /// Relative path of directory `i`.
    fn rel(&self, i: u32) -> PathBuf {
        dest_rel(&self.dirs, i)
    }

    fn shown(&self, parent: u32, name: &CStr) -> String {
        self.path.join(self.rel(parent)).join(std::ffi::OsStr::from_bytes(name.to_bytes())).display().to_string()
    }
}

/// Each writer's most recent parent-directory fd (files of one directory tend to be adjacent).
#[derive(Default)]
struct DirCache(Option<(u32, OwnedFd)>);

impl DirCache {
    fn get<'a>(&'a mut self, d: &'a Dest, i: u32) -> Result<BorrowedFd<'a>> {
        if i == 0 {
            return Ok(d.root.as_fd());
        }
        if self.0.as_ref().is_none_or(|(c, _)| *c != i) {
            let rel = d.rel(i);
            let fd = dirfd::open_dir_beneath(d.root.as_fd(), &rel)
                .with_context(|| format!("open {} (it must stay a real directory)", d.path.join(&rel).display()))?;
            self.0 = Some((i, fd));
        }
        Ok(self.0.as_ref().unwrap().1.as_fd())
    }
}

fn tmp_name() -> CString {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut r = 0u64;
    let got = unsafe { libc::getrandom(&mut r as *mut u64 as *mut libc::c_void, 8, 0) };
    if got != 8 {
        // No randomness: pid + counter + time is still unique enough, and O_EXCL refuses a clash.
        r = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    }
    let n = N.fetch_add(1, Ordering::Relaxed);
    CString::new(format!(".flashcp-{:x}-{:016x}.tmp", std::process::id(), r ^ n)).unwrap()
}

/// Creates the file (preallocated, sized to DataLength) on first use.
fn ensure_open<'a>(st: &'a mut JobState, job: &Job, d: &Dest, cache: &mut DirCache) -> Result<&'a File> {
    if st.file.is_none() {
        let dir = cache.get(d, job.parent)?;
        let shown = || d.shown(job.parent, &job.name);
        let f = if job.replace {
            let tmp = tmp_name();
            let f = dirfd::create_at(dir, &tmp).with_context(|| format!("create a temp file next to {}", shown()))?;
            st.tmp = Some(tmp);
            f
        } else {
            dirfd::create_at(dir, &job.name).with_context(|| format!("create {}", shown()))?
        };
        dirfd::preallocate(&f, job.len).with_context(|| format!("preallocate {} bytes for {}", job.len, shown()))?;
        f.set_len(job.len).with_context(|| format!("size {}", shown()))?;
        st.file = Some(f);
    }
    Ok(st.file.as_ref().unwrap())
}

/// After the last byte: mtime, close, and for a replacement the rename over the old file.
fn finish(st: &mut JobState, job: &Job, d: &Dest, cache: &mut DirCache) -> Result<()> {
    let f = st.file.take().ok_or_else(|| anyhow!("internal error: finishing a file that is not open"))?;
    if let Some(t) = job.mtime.and_then(systime) {
        f.set_modified(t).with_context(|| format!("set mtime of {}", d.shown(job.parent, &job.name)))?;
    }
    drop(f);
    if let Some(tmp) = st.tmp.take() {
        let dir = cache.get(d, job.parent)?;
        if let Err(e) = dirfd::rename_at(dir, &tmp, &job.name) {
            let _ = dirfd::unlink_at(dir, &tmp);
            return Err(e).with_context(|| format!("replace {}", d.shown(job.parent, &job.name)));
        }
    }
    Ok(())
}

/// Writes one extent's bytes into its file; returns true when that completed the file.
fn write_ext(job: &Job, bytes: &[u8], foff: u64, d: &Dest, cache: &mut DirCache) -> Result<bool> {
    let mut st = job.st.lock().unwrap();
    let f = ensure_open(&mut st, job, d, cache)?;
    f.write_all_at(bytes, foff).with_context(|| format!("write {}", d.shown(job.parent, &job.name)))?;
    st.left -= bytes.len() as u64;
    if st.left == 0 {
        finish(&mut st, job, d, cache)?;
        return Ok(true);
    }
    Ok(false)
}

/// What the tree walk produced.
struct Plan {
    dirs: Vec<DirNode>,
    jobs: Vec<Job>,
    exts: Vec<Ext>,
    skipped: u64,
    collisions: Vec<String>,
}

/// Walks the volume from `start` (whose clusters are already in `seen`) and plans every
/// directory, file and extent. Every directory's and file's whole allocation is marked in
/// `seen`, so a cross-link, a FAT loop or a directory cycle fails as soon as it is reached,
/// and the planned data can never exceed the cluster heap.
fn plan(v: &mut Vol, start: Vec<Child>, seen: &mut [u8], root: Option<&OwnedFd>, o: &PullOpts) -> Result<Plan> {
    // Aligning a chunk's start down and its end up adds less than 2 × ALIGN.
    let max = o.chunk as u64 - 2 * ALIGN;
    // Reads stop at the last aligned offset of the volume; the rest is read buffered.
    let cut = v.vol_bytes / ALIGN * ALIGN;
    let capacity = v.cc * v.cs;
    let mut p = Plan {
        dirs: vec![DirNode { parent: 0, name: CString::default(), mtime: None }],
        jobs: Vec::new(),
        exts: Vec::new(),
        skipped: 0,
        collisions: Vec::new(),
    };
    // (children, dir index, volume path, depth, relative path length, exists in the destination)
    let mut stack: Vec<(Vec<Child>, u32, String, usize, usize, bool)> =
        vec![(start, 0, String::new(), 0, 0, root.is_some())];
    let mut entries = 0u64;
    while let Some((kids, di, vpath, depth, plen, exists)) = stack.pop() {
        let shown_dir = if vpath.is_empty() { "/".to_string() } else { vpath.clone() };
        // The existing destination directory, to look for collisions in.
        let dfd = match (exists, root) {
            (true, Some(r)) => {
                let rel = dest_rel(&p.dirs, di);
                Some(dirfd::open_dir_beneath(r.as_fd(), &rel).with_context(|| {
                    format!("open destination directory {} (it must be a real directory)", rel.display())
                })?)
            }
            _ => None,
        };
        for ch in kids {
            entries += 1;
            ensure!(entries <= MAX_ENTRIES, "the volume has more than {MAX_ENTRIES} files and directories");
            let name = component(&ch.name, &shown_dir)?;
            let cname = CString::new(name.clone()).expect("component() refuses NUL");
            let clen = plen + usize::from(plen > 0) + name.len();
            let what = format!("{vpath}/{name}");
            ensure!(clen <= MAX_PATH, "{what}: the path is longer than {MAX_PATH} bytes");
            let existing = match &dfd {
                Some(fd) => {
                    dirfd::stat_at(fd.as_fd(), &cname).with_context(|| format!("stat {} in the destination", what))?
                }
                None => None,
            };
            let mtime = decode_ts(ch.mtime.0, ch.mtime.1, ch.mtime.2);
            ensure!(
                ch.len <= capacity,
                "{what}: DataLength {} is larger than the volume's cluster heap ({capacity}); run fsck.exfat",
                ch.len
            );
            if ch.is_dir {
                ensure!(depth < MAX_DEPTH, "{what}: directories are nested more than {MAX_DEPTH} deep");
                if let Some(st) = &existing {
                    ensure!(dirfd::is_dir(st), "{what} exists in the destination and is not a directory");
                }
                let sub = v.read_child(0, &vpath, &ch)?;
                for &c in &sub.clusters {
                    ensure!(
                        mark(seen, c),
                        "{what}: cluster {c} is used twice (cross-linked, or a directory cycle); run fsck.exfat"
                    );
                }
                let idx = u32::try_from(p.dirs.len()).context("too many directories")?;
                p.dirs.push(DirNode { parent: di, name: cname, mtime });
                // Only the children are kept; the directory's raw data is dropped here.
                stack.push((sub.children.into_values().collect(), idx, what, depth + 1, clen, existing.is_some()));
                continue;
            }
            let mut replace = false;
            if let Some(st) = &existing {
                ensure!(!dirfd::is_dir(st), "{what} exists in the destination and is a directory");
                if o.skip_existing {
                    p.skipped += 1;
                    continue;
                }
                if !o.overwrite {
                    p.collisions.push(what);
                    continue;
                }
                replace = true;
            }
            ensure!(
                ch.valid_len <= ch.len,
                "{what}: ValidDataLength {} exceeds DataLength {}; run fsck.exfat",
                ch.valid_len,
                ch.len
            );
            let fi = u32::try_from(p.jobs.len()).context("too many files")?;
            // The whole allocation is walked and marked (even past ValidDataLength); only the
            // clusters up to ValidDataLength become extents.
            let total = ch.len.div_ceil(v.cs);
            let need = ch.valid_len.div_ceil(v.cs);
            if total > 0 && ch.no_fat_chain {
                contiguous_run(ch.first, total, v.cc)
                    .with_context(|| format!("{what}: contiguous extent runs past the end of the volume"))?;
                mark_range(seen, ch.first, total)
                    .map_err(|c| anyhow!("{what}: cluster {c} is used twice (cross-linked); run fsck.exfat"))?;
                push_run(&mut p.exts, fi, v.cluster_off(ch.first), 0, ch.valid_len, max, cut);
            } else if total > 0 {
                let (mut c, mut run_first, mut run_n, mut foff) = (ch.first, ch.first, 0u64, 0u64);
                for k in 0..total {
                    ensure!(v.valid(c), "{what}: FAT chain points at invalid cluster {c:#x}");
                    ensure!(
                        mark(seen, c),
                        "{what}: cluster {c} is used twice (FAT loop or cross-link); run fsck.exfat"
                    );
                    if k < need {
                        if run_n > 0 && c as u64 != run_first as u64 + run_n {
                            let bytes = run_n * v.cs;
                            push_run(&mut p.exts, fi, v.cluster_off(run_first), foff, bytes, max, cut);
                            (foff, run_n) = (foff + bytes, 0);
                        }
                        if run_n == 0 {
                            run_first = c;
                        }
                        run_n += 1;
                    }
                    let next = v.fat(c)?;
                    if k + 1 == total {
                        ensure!(next == EOC, "{what}: FAT chain is longer than its DataLength");
                    } else {
                        ensure!(next != EOC, "{what}: FAT chain is shorter than its DataLength");
                        c = next;
                    }
                }
                if need > 0 {
                    push_run(&mut p.exts, fi, v.cluster_off(run_first), foff, ch.valid_len - foff, max, cut);
                }
            }
            ensure!(p.exts.len() <= MAX_EXTENTS, "the volume's files have more than {MAX_EXTENTS} fragments");
            p.jobs.push(Job {
                parent: di,
                name: cname,
                len: ch.len,
                valid: ch.valid_len,
                mtime,
                replace,
                st: Mutex::new(JobState { file: None, left: ch.valid_len, tmp: None }),
            });
        }
    }
    Ok(p)
}

fn dest_rel(dirs: &[DirNode], mut i: u32) -> PathBuf {
    let mut parts = Vec::new();
    while i != 0 {
        parts.push(std::ffi::OsStr::from_bytes(dirs[i as usize].name.as_bytes()));
        i = dirs[i as usize].parent;
    }
    parts.iter().rev().collect()
}

/// Removes the temp files of replacements that never completed.
fn cleanup_temps(jobs: &[Job], d: &Dest) {
    let mut cache = DirCache::default();
    for job in jobs {
        let mut st = job.st.lock().unwrap();
        st.file = None;
        if let Some(tmp) = st.tmp.take()
            && let Ok(dir) = cache.get(d, job.parent)
        {
            let _ = dirfd::unlink_at(dir, &tmp);
        }
    }
}

pub fn pull(target: &Path, dest: &Path, o: &PullOpts, sink: Sink<'_>) -> Result<PullStats> {
    pull_from(&TargetSpec::from(target), dest, o, sink)
}

/// [`pull`] from a [`TargetSpec`] (a path, or an inherited block-device fd, O_RDONLY is enough).
pub fn pull_from(target: &TargetSpec, dest: &Path, o: &PullOpts, sink: Sink<'_>) -> Result<PullStats> {
    let (file, dev_len, guard, mounted) = open_source(target)?;
    let r = pull_file(file, dev_len, mounted, &target.describe(), dest, o, sink);
    drop(guard);
    r
}

/// [`pull_from`] of an already opened source (`file`, `dev_len` bytes). `mounted` only words
/// warnings; `what` names the source in them.
pub(crate) fn pull_file(
    file: File,
    dev_len: u64,
    mounted: Option<MountState>,
    what: &str,
    dest: &Path,
    o: &PullOpts,
    sink: Sink<'_>,
) -> Result<PullStats> {
    let t0 = Instant::now();
    ensure!(!(o.skip_existing && o.overwrite), "--skip-existing and --overwrite are mutually exclusive");
    ensure!(
        o.chunk >= 4 * ALIGN as usize && o.chunk.is_multiple_of(ALIGN as usize) && o.chunk <= 1 << 30,
        "chunk must be a multiple of 4 KB, between 16 KB and 1 GB"
    );
    let mut tick = Ticker::new(sink);
    match &mounted {
        Some((dev, at, true)) => eprintln!(
            "warning: {} is mounted read-write at {at} (device {dev}); files written since the last `sync` \
             may be missing or partial. Unmount it (or at least run `sync`) for a consistent copy.",
            what
        ),
        Some((_, at, false)) => {
            eprintln!("note: {what} is mounted read-only at {at}; reading it is safe")
        }
        None => {}
    }
    set_direct(&file, false);
    // The allocation bitmap is not needed (and not loaded): `seen` is the only cluster-sized set.
    let mut v = Vol::open(file.try_clone().context("dup source fd")?, dev_len, true, false)?;
    if v.flags & FLAG_DIRTY != 0 && mounted.is_none() {
        eprintln!("warning: the volume is marked dirty (not cleanly unmounted); consider fsck.exfat");
    }
    let cs = v.cs;
    let mut seen = cluster_set(v.cc);
    let mut cur = v.dirs.pop().ok_or_else(|| anyhow!("no root directory"))?;
    let system = [(&cur.clusters, "the root directory"), (&v.bitmap_clusters, "the allocation bitmap")];
    for (clusters, what) in system.into_iter().chain([(&v.upcase_clusters, "the up-case table")]) {
        for &c in clusters {
            ensure!(mark(&mut seen, c), "cluster {c} of {what} is used twice (cross-linked); run fsck.exfat");
        }
    }

    // ── --from ──
    for comp in o.from.as_deref().unwrap_or("").split('/').filter(|c| !c.is_empty() && *c != ".") {
        ensure!(comp != "..", "--from must not contain '..'");
        let key = upcase(&comp.encode_utf16().collect::<Vec<_>>(), &v.upc);
        let shown = format!("{}/{comp}", cur.path);
        match cur.children.get(&key).cloned() {
            Some(ch) if ch.is_dir => {
                cur = v.read_child(0, &cur.path, &ch)?;
                for &c in &cur.clusters {
                    ensure!(mark(&mut seen, c), "--from: {shown}: cluster {c} is used twice; run fsck.exfat");
                }
            }
            Some(_) => bail!("--from: {shown} is a file on the volume, not a directory"),
            None => bail!("--from: {shown} not found on the volume"),
        }
    }

    // ── plan ──
    // A destination that does not exist yet cannot collide with anything.
    let root = match fs::symlink_metadata(dest) {
        Ok(m) => {
            ensure!(m.is_dir(), "destination {} is not a directory (or is a symlink)", dest.display());
            Some(dirfd::open_root(dest).with_context(|| format!("open {}", dest.display()))?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("stat {}", dest.display())),
    };
    let start: Vec<Child> = std::mem::take(&mut cur.children).into_values().collect();
    drop(cur);
    let mut p = plan(&mut v, start, &mut seen, root.as_ref(), o)?;
    drop(seen);
    if !p.collisions.is_empty() {
        p.collisions.sort();
        let n = p.collisions.len();
        let list: Vec<_> = p.collisions.into_iter().take(10).collect();
        bail!(
            "{n} file(s) already exist in the destination {}, e.g. {}; pass --skip-existing or --overwrite",
            dest.display(),
            list.join(", ")
        );
    }
    p.exts.sort_unstable_by_key(|e| e.phys);
    let cut = v.vol_bytes / ALIGN * ALIGN;
    let tail_at = p.exts.partition_point(|e| e.phys < cut);
    let strat = choose_strategy(&file, &v, o);
    let chunks = coalesce(&p.exts[..tail_at], o.chunk, strat.gap);
    let tail_bytes = p.exts[tail_at..].iter().map(|e| e.phys + e.len as u64).max().map_or(0, |e| e - cut);
    let read_bytes: u64 = chunks.iter().map(|c| c.len as u64).sum::<u64>() + tail_bytes;
    let data_bytes: u64 = p.jobs.iter().map(|j| j.len).sum();
    let (files, dirs) = (p.jobs.len() as u64, p.dirs.len() as u64 - 1);
    let root = match root {
        Some(r) => r,
        None => {
            fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
            dirfd::open_root(dest).with_context(|| format!("open {}", dest.display()))?
        }
    };
    // Room for every byte, plus a block per directory and file of metadata slack.
    let need = data_bytes + (files + dirs) * 4096;
    let free = dirfd::free_bytes(root.as_fd()).with_context(|| format!("statvfs {}", dest.display()))?;
    ensure!(
        need <= free,
        "not enough space in {}: this pull needs {} ({} of file data), only {} is free",
        dest.display(),
        human_bytes(need as f64),
        human_bytes(data_bytes as f64),
        human_bytes(free as f64)
    );
    let detail = format!(
        "pull: {files} files, {dirs} dirs, {} of data → {}{}; reading {} in {} requests of up to {} (cluster {}; {}: gap {}, {} in flight{})",
        human_bytes(data_bytes as f64),
        dest.display(),
        if p.skipped > 0 { format!(" ({} existing skipped)", p.skipped) } else { String::new() },
        human_bytes(read_bytes as f64),
        chunks.len() + usize::from(tail_bytes > 0),
        human_bytes(o.chunk as f64),
        human_bytes(cs as f64),
        strat.transport,
        human_bytes(strat.gap as f64),
        strat.inflight,
        if strat.direct { "" } else { ", buffered" },
    );
    tick.emit(Event::Plan {
        op: Op::Pull,
        files,
        dirs,
        bytes: data_bytes,
        write_bytes: read_bytes,
        detail,
        strategy: Some(strat.to_json()),
    });
    if let Ok(cmd) = std::env::var(AFTER_PLAN_ENV) {
        let ok = std::process::Command::new("sh").arg("-c").arg(&cmd).status().is_ok_and(|s| s.success());
        ensure!(ok, "{AFTER_PLAN_ENV} command failed");
    }
    let trace = std::env::var_os(TRACE_READS_ENV).is_some();
    let d = Dest { root, path: dest.to_path_buf(), dirs: p.dirs };
    let jobs = p.jobs;
    let exts = p.exts;

    let res = transfer(&file, &d, &jobs, &exts, &chunks, tail_at, cut, &strat, o, trace, &mut tick);
    if let Err(e) = res {
        cleanup_temps(&jobs, &d);
        return Err(e.context("pull failed; the destination holds a partial copy"));
    }
    // Directory mtimes last (creating entries changed them), deepest first.
    let mut cache = DirCache::default();
    for dn in d.dirs.iter().skip(1).rev() {
        if let Some((secs, ns)) = dn.mtime {
            let dir = cache.get(&d, dn.parent)?;
            dirfd::set_mtime_at(dir, &dn.name, secs, ns)
                .with_context(|| format!("set mtime of {}", d.shown(dn.parent, &dn.name)))?;
        }
    }
    dirfd::syncfs(d.root.as_fd()).context("syncfs on the destination")?;
    tick.progress(files, data_bytes, true);
    let secs = t0.elapsed().as_secs_f64();
    tick.emit(Event::Done { files, bytes: data_bytes, secs });
    Ok(PullStats {
        files,
        dirs,
        skipped: p.skipped,
        data_bytes,
        read_bytes,
        reads: chunks.len() as u64 + u64::from(tail_bytes > 0),
        cluster: cs,
        secs,
    })
}

/// Directories, empty files, then the data: readers → writers, then the buffered tail.
#[allow(clippy::too_many_arguments)]
fn transfer(
    file: &File,
    d: &Dest,
    jobs: &[Job],
    exts: &[Ext],
    chunks: &[Chunk],
    tail_at: usize,
    cut: u64,
    strat: &Strategy,
    o: &PullOpts,
    trace: bool,
    tick: &mut Ticker<'_>,
) -> Result<()> {
    let mut cache = DirCache::default();
    for (i, dn) in d.dirs.iter().enumerate().skip(1) {
        let parent = cache.get(d, dn.parent)?;
        dirfd::mkdir_at(parent, &dn.name).with_context(|| {
            format!("create directory {} (it must not be a file or a symlink)", d.shown(dn.parent, &dn.name))
        })?;
        debug_assert!(dn.parent < i as u32);
    }
    raise_nofile();
    let files_done = AtomicU64::new(0);
    let bytes_done = AtomicU64::new(0);
    for job in jobs {
        let mut st = job.st.lock().unwrap();
        if st.left == 0 {
            ensure_open(&mut st, job, d, &mut cache)?;
            finish(&mut st, job, d, &mut cache)?;
            files_done.fetch_add(1, Ordering::Relaxed);
            bytes_done.fetch_add(job.len, Ordering::Relaxed);
        }
    }

    let direct = AtomicBool::new(set_direct(file, strat.direct) && strat.direct);
    if !strat.direct {
        // Buffered: let the kernel's readahead stream the device.
        unsafe { libc::posix_fadvise(std::os::fd::AsRawFd::as_raw_fd(file), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let pool = Pool::new(READ_BUDGET);
    let fail = |e: anyhow::Error| {
        failed.store(true, Ordering::Relaxed);
        first_err.lock().unwrap().get_or_insert(e);
        pool.close();
    };
    let done = |job: &Job, n: u64, completed: bool| {
        bytes_done.fetch_add(n, Ordering::Relaxed);
        if completed {
            files_done.fetch_add(1, Ordering::Relaxed);
            // Zeros past ValidDataLength count as done too.
            bytes_done.fetch_add(job.len - job.valid, Ordering::Relaxed);
        }
    };
    let (full_tx, full_rx) = bounded::<(usize, AlignedBuf)>(strat.inflight);
    let (alive_tx, alive_rx) = bounded::<()>(0);
    std::thread::scope(|s| {
        for _ in 0..strat.inflight {
            let (full_tx, alive, pool) = (full_tx.clone(), alive_tx.clone(), &pool);
            let (next, failed, direct, fail) = (&next, &failed, &direct, &fail);
            s.spawn(move || {
                let _alive = alive;
                while !failed.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = chunks.get(i) else { return };
                    let Some(mut buf) = pool.get(c.len) else { return };
                    let need = exts[c.exts.clone()].iter().map(|e| e.phys + e.len as u64).max().unwrap() - c.start;
                    if trace {
                        eprintln!("flashcp-read {} {}", c.start, c.len);
                    }
                    let mut r = read_full(file, &mut buf.as_mut()[..c.len], c.start);
                    if matches!(&r, Err(e) if e.raw_os_error() == Some(libc::EINVAL))
                        && direct.swap(false, Ordering::Relaxed)
                    {
                        set_direct(file, false); // the device refused this alignment
                        r = read_full(file, &mut buf.as_mut()[..c.len], c.start);
                    } else if r.is_err() && !direct.load(Ordering::Relaxed) {
                        r = read_full(file, &mut buf.as_mut()[..c.len], c.start);
                    }
                    match r {
                        Ok(n) if n as u64 >= need => {}
                        Ok(n) => return fail(anyhow!("short read at byte {}: {n} of {need} bytes", c.start)),
                        Err(e) => {
                            return fail(anyhow::Error::new(e).context(format!("read {} bytes at {}", c.len, c.start)));
                        }
                    }
                    if full_tx.send((i, buf)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(full_tx);
        for _ in 0..o.writers.max(1) {
            let (full_rx, alive, pool) = (full_rx.clone(), alive_tx.clone(), &pool);
            let (failed, fail, done) = (&failed, &fail, &done);
            s.spawn(move || {
                let _alive = alive;
                let mut cache = DirCache::default();
                for (i, buf) in full_rx.iter() {
                    if failed.load(Ordering::Relaxed) {
                        return;
                    }
                    let c = &chunks[i];
                    for e in &exts[c.exts.clone()] {
                        let job = &jobs[e.file as usize];
                        let at = (e.phys - c.start) as usize;
                        match write_ext(job, &buf.as_ref()[at..at + e.len as usize], e.foff, d, &mut cache) {
                            Ok(completed) => done(job, e.len as u64, completed),
                            Err(e) => return fail(e),
                        }
                    }
                    pool.put(buf);
                }
            });
        }
        // Only the threads hold channel ends now, so a side that stops unblocks the other.
        drop((alive_tx, full_rx));
        loop {
            match alive_rx.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Disconnected) => break,
                _ => tick.progress(files_done.load(Ordering::Relaxed), bytes_done.load(Ordering::Relaxed), false),
            }
        }
    });
    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    // The unaligned end of the volume: one small buffered read, never past VolumeLength.
    let tail = &exts[tail_at..];
    if let Some(end) = tail.iter().map(|e| e.phys + e.len as u64).max() {
        set_direct(file, false);
        let mut buf = vec![0u8; (end - cut) as usize];
        if trace {
            eprintln!("flashcp-read {cut} {}", buf.len());
        }
        let n = read_full(file, &mut buf, cut).with_context(|| format!("read {} bytes at {cut}", buf.len()))?;
        ensure!(n == buf.len(), "short read at byte {cut}: {n} of {} bytes", buf.len());
        for e in tail {
            let job = &jobs[e.file as usize];
            let at = (e.phys - cut) as usize;
            let completed = write_ext(job, &buf[at..at + e.len as usize], e.foff, d, &mut cache)?;
            done(job, e.len as u64, completed);
        }
    }
    for job in jobs {
        let st = job.st.lock().unwrap();
        ensure!(
            st.file.is_none() && st.left == 0,
            "internal error: {} was not completed",
            d.shown(job.parent, &job.name)
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn ext(phys: u64, len: u32, file: u32) -> Ext {
        Ext { phys, foff: 0, len, file }
    }

    #[test]
    fn push_run_splits_at_max() {
        let mut v = Vec::new();
        push_run(&mut v, 7, 1000, 50, 25, 10, u64::MAX);
        assert_eq!(
            v,
            vec![
                Ext { phys: 1000, foff: 50, len: 10, file: 7 },
                Ext { phys: 1010, foff: 60, len: 10, file: 7 },
                Ext { phys: 1020, foff: 70, len: 5, file: 7 }
            ]
        );
        push_run(&mut v, 7, 0, 0, 0, 10, u64::MAX);
        assert_eq!(v.len(), 3, "an empty run adds nothing");
        // A run crossing the cut is split there.
        let mut v = Vec::new();
        push_run(&mut v, 1, 4000, 0, 200, 1000, 4096);
        assert_eq!(
            v,
            vec![Ext { phys: 4000, foff: 0, len: 96, file: 1 }, Ext { phys: 4096, foff: 96, len: 104, file: 1 }]
        );
    }

    #[test]
    fn coalesce_reads_through_small_gaps_only() {
        let k = ALIGN;
        // Two extents 4 KB apart (merged), then one 1 MB further (separate with a 64 KB gap).
        let e = [ext(k, 100, 0), ext(2 * k + 10, 50, 1), ext(2 * k + (1 << 20), 4096, 2)];
        let c = coalesce(&e, 8 << 20, 64 << 10);
        assert_eq!(
            c,
            vec![
                Chunk { start: k, len: 2 * k as usize, exts: 0..2 },
                Chunk { start: 2 * k + (1 << 20), len: k as usize, exts: 2..3 }
            ]
        );
        // With a huge gap threshold everything is one read.
        assert_eq!(coalesce(&e, 8 << 20, 2 << 20).len(), 1);
    }

    #[test]
    fn coalesce_respects_max_and_alignment() {
        let max = 16 * ALIGN as usize;
        // Unaligned extents of max - 2 * ALIGN bytes, back to back: one per chunk, never over max.
        let n = (max as u64 - 2 * ALIGN) as u32;
        let e: Vec<Ext> = (0..5).map(|i| ext(512 + i * n as u64, n, i as u32)).collect();
        let c = coalesce(&e, max, 1 << 30);
        assert_eq!(c.len(), 5);
        for ch in &c {
            assert!(ch.len <= max && ch.start.is_multiple_of(ALIGN) && ch.len.is_multiple_of(ALIGN as usize));
            for x in &e[ch.exts.clone()] {
                assert!(x.phys >= ch.start && x.phys + x.len as u64 <= ch.start + ch.len as u64);
            }
        }
        // Overlapping extents (a cross-linked volume) stay inside one read.
        let e = [ext(0, 8192, 0), ext(4096, 100, 1)];
        assert_eq!(coalesce(&e, max, 0), vec![Chunk { start: 0, len: 8192, exts: 0..2 }]);
    }

    #[test]
    fn component_refuses_unsafe_names() {
        let u = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        assert_eq!(component(&u("ok 😀.txt"), "/").unwrap(), "ok 😀.txt");
        for bad in [".", "..", "a/b", "a\0b", ""] {
            assert!(component(&u(bad), "/").is_err(), "{bad:?} accepted");
        }
        assert!(component(&[0xD800], "/").is_err(), "lone surrogate accepted");
    }
}
