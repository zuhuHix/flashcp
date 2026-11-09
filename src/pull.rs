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
//! A mounted source is read as it is on the device: fine when nothing is writing to it (the
//! kernel driver's metadata goes through the block device's page cache, which buffered
//! metadata reads see, and O_DIRECT data reads flush that cache first), but files written
//! since the last `sync` may be missing or partial. An unmounted volume is preferred, and a
//! read-write mount gets a warning.

use crate::events::{Event, Op, Sink, Ticker};
use crate::exfat::{FlagGuard, SYS_BLOCK, TargetSpec, blk_size, open_target_fd, related_devs, set_direct};
use crate::util::{AlignedBuf, human_bytes};
use crate::volume::*;
use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{RecvTimeoutError, bounded};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Gaps between extents up to this many bytes are read through rather than skipped.
    pub gap: u64,
    /// Device reads in flight.
    pub inflight: usize,
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
            gap: 1 << 20,
            inflight: 4,
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

/// One destination file.
struct Job {
    rel: PathBuf,
    len: u64,
    /// ValidDataLength: bytes read from the volume; the rest of `len` stays zero.
    valid: u64,
    mtime: Option<SystemTime>,
    st: Mutex<JobState>,
}

struct JobState {
    file: Option<File>,
    /// Bytes still to write (starts at ValidDataLength).
    left: u64,
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

/// Pushes the extents of `bytes` bytes at physical `phys` (file offset `foff`), split so no
/// extent exceeds `max`.
fn push_run(exts: &mut Vec<Ext>, file: u32, mut phys: u64, mut foff: u64, mut bytes: u64, max: u64) {
    while bytes > 0 {
        let n = bytes.min(max);
        exts.push(Ext { phys, foff, len: n as u32, file });
        (phys, foff, bytes) = (phys + n, foff + n, bytes - n);
    }
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
type MountState = (String, String, bool);

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

/// What the tree walk produced.
struct Plan {
    /// Destination directories (relative), parents first, with their mtimes.
    dirs: Vec<(PathBuf, Option<SystemTime>)>,
    jobs: Vec<Job>,
    exts: Vec<Ext>,
    skipped: u64,
    collisions: Vec<String>,
}

/// Walks the volume from `start` and plans every directory, file and extent.
fn plan(v: &mut Vol, start: Dir, dest: &Path, fresh_root: bool, o: &PullOpts) -> Result<Plan> {
    // Aligning a chunk's start down and its end up adds less than 2 × ALIGN.
    let max = o.chunk as u64 - 2 * ALIGN;
    let mut p = Plan { dirs: Vec::new(), jobs: Vec::new(), exts: Vec::new(), skipped: 0, collisions: Vec::new() };
    let mut stack: Vec<(Dir, PathBuf, bool)> = vec![(start, PathBuf::new(), fresh_root)];
    let mut ndirs = 0u64;
    while let Some((d, rel, fresh)) = stack.pop() {
        let shown = if d.path.is_empty() { "/" } else { d.path.as_str() };
        for ch in d.children.values() {
            let name = component(&ch.name, shown)?;
            let crel = rel.join(&name);
            let existing = if fresh { None } else { fs::symlink_metadata(dest.join(&crel)).ok() };
            let mtime = decode_ts(ch.mtime.0, ch.mtime.1, ch.mtime.2).and_then(systime);
            if ch.is_dir {
                ndirs += 1;
                // Every directory owns at least one cluster, so more means a cycle.
                ensure!(ndirs <= v.cc, "directory tree has a cycle; run fsck.exfat");
                if let Some(m) = &existing {
                    ensure!(
                        m.is_dir(),
                        "{} exists in the destination and is not a directory",
                        dest.join(&crel).display()
                    );
                }
                let sub = v.read_child(0, &d.path, ch)?;
                p.dirs.push((crel.clone(), mtime));
                stack.push((sub, crel, existing.is_none()));
                continue;
            }
            let what = || format!("{}/{name}", d.path);
            if let Some(m) = &existing {
                ensure!(!m.is_dir(), "{} exists in the destination and is a directory", dest.join(&crel).display());
                if o.skip_existing {
                    p.skipped += 1;
                    continue;
                }
                if !o.overwrite {
                    p.collisions.push(dest.join(&crel).display().to_string());
                    continue;
                }
            }
            ensure!(
                ch.valid_len <= ch.len,
                "{}: ValidDataLength {} exceeds DataLength {}; run fsck.exfat",
                what(),
                ch.valid_len,
                ch.len
            );
            let fi = u32::try_from(p.jobs.len()).context("too many files")?;
            let need = ch.valid_len.div_ceil(v.cs);
            if need > 0 {
                if ch.no_fat_chain {
                    // The whole allocation must lie inside the heap, even past ValidDataLength.
                    contiguous_run(ch.first, ch.len.div_ceil(v.cs), v.cc)
                        .with_context(|| format!("{}: contiguous extent runs past the end of the volume", what()))?;
                    push_run(&mut p.exts, fi, v.cluster_off(ch.first), 0, ch.valid_len, max);
                } else {
                    let (mut c, mut run_first, mut run_n, mut foff) = (ch.first, ch.first, 0u64, 0u64);
                    for k in 0..need {
                        ensure!(v.valid(c), "{}: FAT chain points at invalid cluster {c:#x}", what());
                        if run_n > 0 && c as u64 != run_first as u64 + run_n {
                            let bytes = run_n * v.cs;
                            push_run(&mut p.exts, fi, v.cluster_off(run_first), foff, bytes, max);
                            (foff, run_first, run_n) = (foff + bytes, c, 0);
                        }
                        if run_n == 0 {
                            run_first = c;
                        }
                        run_n += 1;
                        if k + 1 < need {
                            let next = v.fat(c)?;
                            ensure!(next != EOC, "{}: FAT chain is shorter than its DataLength", what());
                            c = next;
                        }
                    }
                    push_run(&mut p.exts, fi, v.cluster_off(run_first), foff, ch.valid_len - foff, max);
                }
            }
            p.jobs.push(Job {
                rel: crel,
                len: ch.len,
                valid: ch.valid_len,
                mtime,
                st: Mutex::new(JobState { file: None, left: ch.valid_len }),
            });
        }
    }
    Ok(p)
}

fn open_dest(path: &Path, overwrite: bool) -> Result<File> {
    let mut oo = OpenOptions::new();
    oo.write(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    if overwrite {
        oo.create(true).truncate(true);
    } else {
        oo.create_new(true);
    }
    oo.open(path).with_context(|| format!("create {}", path.display()))
}

/// Creates the file (preallocated, sized to DataLength) on first use.
fn ensure_open<'a>(st: &'a mut JobState, job: &Job, dest: &Path, overwrite: bool) -> Result<&'a File> {
    if st.file.is_none() {
        let path = dest.join(&job.rel);
        let f = open_dest(&path, overwrite)?;
        crate::util::preallocate(&f, job.len);
        f.set_len(job.len).with_context(|| format!("size {}", path.display()))?;
        st.file = Some(f);
    }
    Ok(st.file.as_ref().unwrap())
}

fn finish(st: &mut JobState, job: &Job, dest: &Path) -> Result<()> {
    if let (Some(f), Some(t)) = (st.file.take(), job.mtime) {
        f.set_modified(t).with_context(|| format!("set mtime of {}", dest.join(&job.rel).display()))?;
    }
    Ok(())
}

pub fn pull(target: &Path, dest: &Path, o: &PullOpts, sink: Sink<'_>) -> Result<PullStats> {
    pull_from(&TargetSpec::from(target), dest, o, sink)
}

/// [`pull`] from a [`TargetSpec`] (a path, or an inherited block-device fd, O_RDONLY is enough).
pub fn pull_from(target: &TargetSpec, dest: &Path, o: &PullOpts, sink: Sink<'_>) -> Result<PullStats> {
    let t0 = Instant::now();
    ensure!(!(o.skip_existing && o.overwrite), "--skip-existing and --overwrite are mutually exclusive");
    ensure!(
        o.chunk >= 4 * ALIGN as usize && o.chunk.is_multiple_of(ALIGN as usize),
        "chunk must be a multiple of 4 KB and at least 16 KB"
    );
    let mut tick = Ticker::new(sink);
    let (file, dev_len, _guard, mounted) = open_source(target)?;
    match &mounted {
        Some((dev, at, true)) => eprintln!(
            "warning: {} is mounted read-write at {at} (device {dev}); files written since the last `sync` \
             may be missing or partial. Unmount it (or at least run `sync`) for a consistent copy.",
            target.describe()
        ),
        Some((_, at, false)) => {
            eprintln!("note: {} is mounted read-only at {at}; reading it is safe", target.describe())
        }
        None => {}
    }
    set_direct(&file, false);
    let mut v = Vol::open(file.try_clone().context("dup source fd")?, dev_len, true)?;
    if v.flags & FLAG_DIRTY != 0 && mounted.is_none() {
        eprintln!("warning: the volume is marked dirty (not cleanly unmounted); consider fsck.exfat");
    }
    let cs = v.cs;

    // ── --from ──
    let mut cur = v.dirs.pop().ok_or_else(|| anyhow!("no root directory"))?;
    for comp in o.from.as_deref().unwrap_or("").split('/').filter(|c| !c.is_empty() && *c != ".") {
        ensure!(comp != "..", "--from must not contain '..'");
        let key = upcase(&comp.encode_utf16().collect::<Vec<_>>(), &v.upc);
        let shown = format!("{}/{comp}", cur.path);
        match cur.children.get(&key).cloned() {
            Some(ch) if ch.is_dir => cur = v.read_child(0, &cur.path, &ch)?,
            Some(_) => bail!("--from: {shown} is a file on the volume, not a directory"),
            None => bail!("--from: {shown} not found on the volume"),
        }
    }

    // ── plan ──
    // A destination that does not exist yet (or is empty) cannot collide with anything.
    let fresh = match fs::symlink_metadata(dest) {
        Ok(m) => {
            ensure!(m.is_dir(), "destination {} is not a directory", dest.display());
            fs::read_dir(dest).with_context(|| format!("read {}", dest.display()))?.next().is_none()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e).with_context(|| format!("stat {}", dest.display())),
    };
    let mut p = plan(&mut v, cur, dest, fresh, o)?;
    if !p.collisions.is_empty() {
        p.collisions.sort();
        let n = p.collisions.len();
        let list: Vec<_> = p.collisions.into_iter().take(10).collect();
        bail!(
            "{n} file(s) already exist in the destination, e.g. {}; pass --skip-existing or --overwrite",
            list.join(", ")
        );
    }
    p.exts.sort_unstable_by_key(|e| e.phys);
    let chunks = coalesce(&p.exts, o.chunk, o.gap);
    let read_bytes: u64 = chunks.iter().map(|c| c.len as u64).sum();
    let data_bytes: u64 = p.jobs.iter().map(|j| j.len).sum();
    let (files, dirs) = (p.jobs.len() as u64, p.dirs.len() as u64);
    let detail = format!(
        "pull: {files} files, {dirs} dirs, {} of data → {}{}; reading {} in {} requests of up to {} (cluster {})",
        human_bytes(data_bytes as f64),
        dest.display(),
        if p.skipped > 0 { format!(" ({} existing skipped)", p.skipped) } else { String::new() },
        human_bytes(read_bytes as f64),
        chunks.len(),
        human_bytes(o.chunk as f64),
        human_bytes(cs as f64),
    );
    tick.emit(Event::Plan { op: Op::Pull, files, dirs, bytes: data_bytes, write_bytes: read_bytes, detail });

    // ── directories, then files with nothing to read ──
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for (rel, _) in &p.dirs {
        let path = dest.join(rel);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Checked during planning; re-checked so a racing symlink is never followed.
                let m = fs::symlink_metadata(&path)?;
                ensure!(m.is_dir(), "{} is not a directory", path.display());
            }
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
    raise_nofile();
    let files_done = AtomicU64::new(0);
    let bytes_done = AtomicU64::new(0);
    for job in &p.jobs {
        let mut st = job.st.lock().unwrap();
        if st.left == 0 {
            ensure_open(&mut st, job, dest, o.overwrite)?;
            finish(&mut st, job, dest)?;
            files_done.fetch_add(1, Ordering::Relaxed);
            bytes_done.fetch_add(job.len, Ordering::Relaxed);
        }
    }

    // ── data: readers → writers ──
    let direct = AtomicBool::new(set_direct(&file, true));
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let fail = |e: anyhow::Error| {
        failed.store(true, Ordering::Relaxed);
        first_err.lock().unwrap().get_or_insert(e);
    };
    let nbuf = o.inflight.max(1) + o.writers.max(1);
    let (free_tx, free_rx) = bounded::<AlignedBuf>(nbuf);
    for _ in 0..nbuf.min(chunks.len()) {
        free_tx.send(AlignedBuf::new(o.chunk)).unwrap();
    }
    let (full_tx, full_rx) = bounded::<(usize, AlignedBuf)>(nbuf);
    let (alive_tx, alive_rx) = bounded::<()>(0);
    std::thread::scope(|s| {
        for _ in 0..o.inflight.max(1) {
            let (free_rx, full_tx, alive) = (free_rx.clone(), full_tx.clone(), alive_tx.clone());
            let (file, chunks, next, failed, direct, fail) = (&file, &chunks, &next, &failed, &direct, &fail);
            let exts = &p.exts;
            s.spawn(move || {
                let _alive = alive;
                while !failed.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = chunks.get(i) else { return };
                    let Ok(mut buf) = free_rx.recv() else { return };
                    let need = exts[c.exts.clone()].iter().map(|e| e.phys + e.len as u64).max().unwrap() - c.start;
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
                        Ok(n) => {
                            return fail(anyhow!("short read at byte {}: {n} of {need} bytes", c.start));
                        }
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
            let (full_rx, free_tx, alive) = (full_rx.clone(), free_tx.clone(), alive_tx.clone());
            let (chunks, failed, fail, jobs, exts) = (&chunks, &failed, &fail, &p.jobs, &p.exts);
            let (files_done, bytes_done) = (&files_done, &bytes_done);
            let overwrite = o.overwrite;
            s.spawn(move || {
                let _alive = alive;
                for (i, buf) in full_rx.iter() {
                    if failed.load(Ordering::Relaxed) {
                        return;
                    }
                    let c = &chunks[i];
                    for e in &exts[c.exts.clone()] {
                        let job = &jobs[e.file as usize];
                        let at = (e.phys - c.start) as usize;
                        let res = (|| -> Result<()> {
                            let mut st = job.st.lock().unwrap();
                            let f = ensure_open(&mut st, job, dest, overwrite)?;
                            f.write_all_at(&buf.as_ref()[at..at + e.len as usize], e.foff)
                                .with_context(|| format!("write {}", dest.join(&job.rel).display()))?;
                            st.left -= e.len as u64;
                            bytes_done.fetch_add(e.len as u64, Ordering::Relaxed);
                            if st.left == 0 {
                                finish(&mut st, job, dest)?;
                                files_done.fetch_add(1, Ordering::Relaxed);
                                // Zeros past ValidDataLength count as done too.
                                bytes_done.fetch_add(job.len - job.valid, Ordering::Relaxed);
                            }
                            Ok(())
                        })();
                        if let Err(e) = res {
                            return fail(e);
                        }
                    }
                    let _ = free_tx.send(buf);
                }
            });
        }
        // Only the threads hold channel ends now, so a side that stops unblocks the other.
        drop((alive_tx, free_tx, free_rx, full_rx));
        loop {
            match alive_rx.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Disconnected) => break,
                _ => tick.progress(files_done.load(Ordering::Relaxed), bytes_done.load(Ordering::Relaxed), false),
            }
        }
    });
    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e.context("pull failed; the destination holds a partial copy"));
    }
    drop(_guard);
    for job in &p.jobs {
        ensure!(job.st.lock().unwrap().file.is_none(), "internal error: {} was not completed", job.rel.display());
    }
    // Directory mtimes last (creating entries changed them), deepest first.
    for (rel, t) in p.dirs.iter().rev() {
        if let Some(t) = t {
            let path = dest.join(rel);
            File::open(&path)
                .and_then(|f| f.set_modified(*t))
                .with_context(|| format!("set mtime of {}", path.display()))?;
        }
    }
    crate::util::syncfs(dest)?;
    tick.progress(files, data_bytes, true);
    let secs = t0.elapsed().as_secs_f64();
    tick.emit(Event::Done { files, bytes: data_bytes, secs });
    Ok(PullStats {
        files,
        dirs,
        skipped: p.skipped,
        data_bytes,
        read_bytes,
        reads: chunks.len() as u64,
        cluster: cs,
        secs,
    })
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
        push_run(&mut v, 7, 1000, 50, 25, 10);
        assert_eq!(
            v,
            vec![
                Ext { phys: 1000, foff: 50, len: 10, file: 7 },
                Ext { phys: 1010, foff: 60, len: 10, file: 7 },
                Ext { phys: 1020, foff: 70, len: 5, file: 7 }
            ]
        );
        push_run(&mut v, 7, 0, 0, 0, 10);
        assert_eq!(v.len(), 3, "an empty run adds nothing");
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
