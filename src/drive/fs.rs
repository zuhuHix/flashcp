//! [`Fs`]: the driver as a thread-safe object (what the FUSE adapter and the tests call), with
//! the commit thread, the bounded write window and the durability waits.

use super::commit::{self, Plan, Seg, Src};
use super::core::{Attr, Core, Errno, Ino, R, RenameFlags, SetAttr, Statfs};
use super::dev::Dev;
use super::djournal::{self, Place};
use crate::volume::Vol;
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct FsOpts {
    /// Commit this long after the last write.
    pub idle: Duration,
    /// Writers block while this many bytes are staged.
    pub max_dirty: u64,
    /// Staged bytes kept in RAM before spilling to the cache directory.
    pub ram_cap: u64,
    /// Run the commit thread (tests drive commits by hand when false).
    pub auto_commit: bool,
    /// Spill directory (default: $XDG_CACHE_HOME/flashcp).
    pub spill_dir: Option<std::path::PathBuf>,
    /// Physical-order readahead of committed data (pull's strategy for the device).
    pub readahead: bool,
    /// Readahead cache, bytes.
    pub ra_cap: u64,
}

impl Default for FsOpts {
    fn default() -> Self {
        FsOpts {
            idle: Duration::from_millis(1000),
            max_dirty: 256 << 20,
            ram_cap: 512 << 20,
            auto_commit: true,
            spill_dir: None,
            readahead: true,
            ra_cap: 64 << 20,
        }
    }
}

/// What a commit reports (for events).
#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    Dirty { bytes: u64, files: u64 },
    Committing { bytes: u64, files: u64, reason: &'static str },
    Committed { bytes: u64, files: u64, secs: f64 },
    Error { message: String, fatal: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub state: &'static str,
    pub dirty_bytes: u64,
    pub dirty_files: u64,
    pub open_files: u64,
    pub free_bytes: u64,
}

type NoteFn = Box<dyn FnMut(Note) + Send>;

struct Shared {
    core: Mutex<Core>,
    /// Signalled when a commit finishes, a write lands, or state changes.
    cv: Condvar,
    st: Mutex<Ctl>,
    dev: Arc<dyn Dev>,
    opts: FsOpts,
    notes: Mutex<Option<NoteFn>>,
    /// Held (shared) by readers of committed data while they read outside the core lock;
    /// a commit's planning takes it exclusively, so clusters it reuses have no reader left.
    gate: std::sync::RwLock<()>,
    /// The admission gate of mutations (see [`Fs::mutate`]) and the eject state it guards.
    admit: std::sync::RwLock<()>,
    ejecting: std::sync::atomic::AtomicBool,
    /// Test hook: runs inside every mutation, after admission.
    #[cfg(test)]
    hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    ra: Option<Arc<super::readahead::Ra>>,
    /// Device handle for reads outside the core lock.
    rdev: std::fs::File,
    track: Mutex<Track>,
}

/// What readahead knows about the readers: per directory, files whose reading started at
/// offset 0 (and whether its remaining files were queued); per file, where a sequential
/// reader is and how far ahead of it is queued.
#[derive(Default)]
struct Track {
    dirs: std::collections::HashMap<Ino, DirTrack>,
    files: std::collections::HashMap<Ino, (u64, u64)>,
    /// When a bulk copy was last seen.
    bulk: Option<Instant>,
}

#[derive(Default)]
struct DirTrack {
    opened: u32,
    queued: bool,
    next: bool,
}

/// Device ranges of `dir`'s committed files after set offset `after`, in its order (up to
/// [`RA_DIR_FILE_MAX`] each, [`RA_DIR_MAX`] in all); marks them queued for the sequential
/// readahead.
fn dir_ranges(c: &mut Core, tr: &mut Track, dir: Ino, after: usize) -> Vec<(u64, u64)> {
    let mut files: Vec<(usize, Ino)> = c
        .ino(dir)
        .ok()
        .and_then(|p| p.kids.as_ref())
        .map(|k| {
            k.values()
                .filter_map(|&k| {
                    let x = c.inodes.get(&k)?;
                    let d = x.disk?;
                    (!x.is_dir && d.dir == dir && d.off > after && x.ov.is_empty() && x.keep > 0).then_some((d.off, k))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let mut out = Vec::new();
    let mut total = 0u64;
    for (_, k) in files {
        let keep = c.inodes[&k].keep.min(RA_DIR_FILE_MAX);
        total += keep;
        if total > RA_DIR_MAX {
            break;
        }
        out.extend(base_ranges(c, k, 0, keep));
        tr.files.insert(k, (0, keep));
    }
    out
}

/// The directory after `dir` in its parent's order (readdir order: `cp -r` goes there next).
fn next_sibling(c: &Core, dir: Ino) -> Option<Ino> {
    let n = c.ino(dir).ok()?;
    let (parent, me) = (n.parent, n.disk?.off);
    let p = c.ino(parent).ok()?;
    p.kids
        .as_ref()?
        .values()
        .filter_map(|&k| {
            let x = c.inodes.get(&k)?;
            let d = x.disk?;
            (x.is_dir && d.dir == parent && d.off > me).then_some((d.off, k))
        })
        .min()
        .map(|(_, k)| k)
}

/// Bytes of a big file queued ahead of its sequential reader.
const RA_WINDOW: u64 = 16 << 20;
/// A directory's files are queued up to this many bytes each (the rest follows as they are
/// read sequentially).
const RA_DIR_FILE_MAX: u64 = 2 << 20;
/// Queued payload of one directory, at most.
const RA_DIR_MAX: u64 = 1 << 30;

#[derive(Default)]
struct Ctl {
    last_write: Option<Instant>,
    /// Commit requests: a generation number per request, the last one completed.
    want: u64,
    done: u64,
    committing: bool,
    stop: bool,
    reason: &'static str,
    fatal: Option<String>,
}

#[derive(Clone)]
pub struct Fs {
    sh: Arc<Shared>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

/// Opens the volume, recovers an interrupted commit, and builds the in-memory state.
pub(crate) fn open_core(
    file: std::fs::File,
    len: u64,
    dev: &dyn Dev,
    opts: &FsOpts,
) -> Result<(Core, djournal::Recovered)> {
    let mut v = Vol::open_boot(file.try_clone().context("dup")?, len, true, false)?;
    let mut rec = djournal::Recovered::Nothing;
    if let Some(p) = djournal::locate_raw(&mut v, crate::volume::MAX_DIR_BYTES)? {
        rec = djournal::recover(&v, dev, &p)?;
    }
    drop(v);
    let v = Vol::open(file, len, true, true)?;
    if v.flags & crate::volume::FLAG_DIRTY != 0 {
        anyhow::bail!(super::Fallback::Dirty);
    }
    let core = Core::new(v, super::stage::Stager::new(opts.ram_cap, opts.spill_dir.clone()))?;
    Ok((core, rec))
}

/// Opens the volume in `file` (`len` bytes, already claimed by the caller) for serving:
/// recovery first, then the in-memory state. Errors: [`super::Fallback`] (exit 3: the kernel's),
/// [`djournal::Refused`] (exit 6), anything else (exit 1).
pub fn mount_file(file: std::fs::File, len: u64, opts: FsOpts) -> Result<(Fs, Recovered)> {
    let dev: Arc<dyn Dev> = Arc::new(super::dev::FileDev::new(Arc::new(file.try_clone().context("dup")?)));
    let (core, rec) = match open_core(file, len, &*dev, &opts) {
        Ok(x) => x,
        Err(e) if e.downcast_ref::<super::Fallback>().is_some() || e.downcast_ref::<djournal::Refused>().is_some() => {
            return Err(e);
        }
        Err(e) => return Err(anyhow::Error::new(super::check::classify(&e)).context(format!("{e:#}"))),
    };
    let mut core = core;
    if let Some(why) = cannot_bootstrap(&core) {
        // never accept writes that no commit could make durable
        eprintln!("flashcp mount: read-only: {why}");
        core.read_only = Some(libc::EROFS);
    } else if core.journal.is_none() {
        // now rather than in the first commit, which would hold the lock through three syncs
        if let Err(e) = bootstrap(&mut core, &*dev) {
            eprintln!("flashcp mount: read-only: creating the drive journal failed: {e:#}");
            core.read_only = Some(libc::EROFS);
        }
    }
    Ok((Fs::new(core, dev, opts), rec))
}

/// Why the first commit could not create the journal file (None: it can, or it exists).
pub(crate) fn cannot_bootstrap(c: &Core) -> Option<String> {
    if c.journal.is_some() {
        return None;
    }
    let cs = c.vol.cs;
    let cap = djournal::capacity(c.vol.vol_bytes, cs);
    let free = crate::add::free_extents(&c.bm, c.vol.cc);
    if !free.iter().any(|e| e.1 >= cap / cs) {
        return Some(format!("no contiguous free space for the drive journal ({cap} bytes)"));
    }
    let root = c.inodes.get(&super::core::ROOT)?.dd.as_ref()?;
    let n = djournal::NAME.encode_utf16().count();
    if djournal::one_sector_slot(&root.data, root.end, super::set::set_bytes(n)).is_none() {
        return Some("the root directory has no room for the drive journal's entry".into());
    }
    None
}

pub use djournal::Recovered;

impl Fs {
    pub(crate) fn new(core: Core, dev: Arc<dyn Dev>, opts: FsOpts) -> Fs {
        let rdev = core.vol.dev.try_clone().expect("dup the device handle");
        let ra = if opts.readahead {
            // its own open file description: O_DIRECT is per description, and the metadata
            // handle must stay buffered
            use std::os::unix::io::AsRawFd;
            let own = std::fs::File::open(format!("/proc/self/fd/{}", core.vol.dev.as_raw_fd()));
            let f = own.unwrap_or_else(|_| core.vol.dev.try_clone().expect("dup the device handle"));
            let strat = crate::pull::read_strategy(&f, &core.vol);
            let separate = !std::ptr::eq(&f, &core.vol.dev)
                && unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) & libc::O_DIRECT } == 0;
            let direct = strat.direct
                && separate
                && crate::exfat::set_direct(&f, true)
                && unsafe { libc::fcntl(core.vol.dev.as_raw_fd(), libc::F_GETFL) & libc::O_DIRECT } == 0;
            if !direct {
                crate::exfat::set_direct(&f, false);
            }
            let mut strat = strat;
            strat.direct = direct;
            // benchmark hook: the readahead depth
            if let Some(n) = std::env::var("FLASHCP_RA_INFLIGHT").ok().and_then(|v| v.parse().ok()) {
                strat.inflight = n;
            }
            Some(super::readahead::Ra::new(Arc::new(f), strat, opts.ra_cap))
        } else {
            None
        };
        let sh = Arc::new(Shared {
            gate: std::sync::RwLock::new(()),
            admit: std::sync::RwLock::new(()),
            ejecting: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            hook: Mutex::new(None),
            ra,
            rdev,
            track: Mutex::new(Track::default()),
            core: Mutex::new(core),
            cv: Condvar::new(),
            st: Mutex::new(Ctl::default()),
            dev,
            opts: opts.clone(),
            notes: Mutex::new(None),
        });
        let fs = Fs { sh, thread: Arc::new(Mutex::new(None)) };
        if opts.auto_commit {
            let f = fs.clone();
            *fs.thread.lock().unwrap() = Some(std::thread::spawn(move || f.commit_loop()));
        }
        fs
    }

    pub fn on_note(&self, f: impl FnMut(Note) + Send + 'static) {
        *self.sh.notes.lock().unwrap() = Some(Box::new(f));
    }

    fn note(&self, n: Note) {
        if let Some(f) = self.sh.notes.lock().unwrap().as_mut() {
            f(n);
        }
    }

    fn core(&self) -> MutexGuard<'_, Core> {
        self.sh.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ───────────── operations ─────────────

    pub fn lookup(&self, parent: Ino, name: &std::ffi::OsStr) -> R<Attr> {
        let name = super::core::utf16(name)?;
        let mut c = self.core();
        let i = c.lookup(parent, &name)?;
        c.ino_mut(i)?.nlookup += 1;
        c.attr(i)
    }
    pub fn forget(&self, ino: Ino, n: u64) {
        let mut c = self.core();
        if let Some(x) = c.inodes.get_mut(&ino) {
            x.nlookup = x.nlookup.saturating_sub(n);
        }
        c.gc(ino);
    }
    pub fn getattr(&self, ino: Ino) -> R<Attr> {
        self.core().attr(ino)
    }
    pub fn setattr(&self, ino: Ino, a: &SetAttr) -> R<Attr> {
        let r = self.mutate(|c| c.setattr(ino, a));
        if r.is_ok() && a.size.is_some() {
            self.wrote();
        }
        r
    }
    pub fn readdir(&self, ino: Ino) -> R<Vec<(Ino, String, bool)>> {
        let v = self.core().readdir(ino)?;
        Ok(v.into_iter().map(|(i, n, d)| (i, String::from_utf16_lossy(&n), d)).collect())
    }
    pub fn mkdir(&self, parent: Ino, name: &std::ffi::OsStr) -> R<Attr> {
        let name = super::core::utf16(name)?;
        let a = self.mutate(|c| {
            let i = c.create(parent, &name, true)?;
            c.ino_mut(i)?.nlookup += 1;
            c.attr(i)
        })?;
        self.wrote();
        Ok(a)
    }
    /// Creates (or with `excl == false` opens an existing) file, open once more.
    pub fn create(&self, parent: Ino, name: &std::ffi::OsStr, excl: bool, trunc: bool) -> R<Attr> {
        let name = super::core::utf16(name)?;
        let a = self.mutate(|c| {
            let i = match c.lookup(parent, &name) {
                Ok(_) if excl => return Err(libc::EEXIST),
                Ok(i) => {
                    if c.ino(i)?.is_dir {
                        return Err(libc::EISDIR);
                    }
                    if trunc {
                        c.truncate(i, 0)?;
                    }
                    i
                }
                Err(libc::ENOENT) => c.create(parent, &name, false)?,
                Err(e) => return Err(e),
            };
            let n = c.ino_mut(i)?;
            n.nlookup += 1;
            n.opens += 1;
            c.opened += 1;
            c.attr(i)
        })?;
        self.wrote();
        Ok(a)
    }
    pub fn open(&self, ino: Ino, write: bool, trunc: bool) -> R<()> {
        if write || trunc {
            return self.mutate(|c| Self::open_in(c, ino, write, trunc)).inspect(|_| {
                if trunc {
                    self.wrote();
                }
            });
        }
        Self::open_in(&mut self.core(), ino, false, false)
    }

    fn open_in(c: &mut Core, ino: Ino, write: bool, trunc: bool) -> R<()> {
        let n = c.ino(ino)?;
        if write && n.is_dir {
            return Err(libc::EISDIR);
        }
        if write {
            if let Some(e) = c.read_only {
                return Err(e);
            }
            if n.attrs & super::set::ATTR_RO != 0 {
                return Err(libc::EACCES);
            }
        }
        if trunc {
            c.truncate(ino, 0)?;
        }
        c.ino_mut(ino)?.opens += 1;
        c.opened += 1;
        Ok(())
    }
    pub fn release(&self, ino: Ino) {
        let mut c = self.core();
        if let Some(n) = c.inodes.get_mut(&ino)
            && n.opens > 0
        {
            n.opens -= 1;
            c.opened -= 1;
        }
        c.gc(ino);
    }
    pub fn read(&self, ino: Ino, off: u64, size: u32) -> R<Vec<u8>> {
        let (plan, _gate) = {
            let mut c = self.core();
            let plan = c.read_plan(ino, off, size)?;
            if let Some(ra) = &self.sh.ra {
                // queued under the lock: a commit's invalidation cannot slip in between
                let pf = self.prefetch_ranges(&mut c, ino, off, size);
                if !pf.is_empty() {
                    ra.prefetch(&pf);
                }
            }
            (plan, self.sh.gate.read().unwrap_or_else(|e| e.into_inner()))
        };
        let total = plan.last().map_or(0, |p| p.0 + p.1);
        let mut buf = vec![0u8; total];
        super::core::exec_read(&self.sh.rdev, self.sh.ra.as_deref(), &plan, &mut buf)?;
        Ok(buf)
    }

    /// Device ranges to queue for a read of `ino` at `off` (see [`Track`]).
    fn prefetch_ranges(&self, c: &mut Core, ino: Ino, off: u64, size: u32) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let Ok(n) = c.ino(ino) else { return out };
        let (parent, fsize) = (n.parent, n.size);
        let mut tr = self.sh.track.lock().unwrap();
        let end = off + size as u64;
        let f = tr.files.entry(ino).or_insert((0, 0));
        let sequential = off == 0 || off == f.0;
        f.0 = end;
        if sequential && fsize > RA_DIR_FILE_MAX.min(1 << 20) {
            let target = (end + RA_WINDOW).min(fsize);
            let from = f.1.max(end);
            if from < target && (f.1 <= end + RA_WINDOW / 2) {
                f.1 = target;
                out.extend(base_ranges(c, ino, from, target));
            }
        }
        if off == 0 {
            // a bulk copy (a second file of a directory read from its start, or any first
            // file while one was seen within the last 2 s): queue this directory's remaining
            // files and its next sibling directory's, so the device never idles at a boundary
            let bulk = tr.bulk.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
            let d = tr.dirs.entry(parent).or_default();
            d.opened += 1;
            if d.opened >= 2 || bulk {
                tr.bulk = Some(Instant::now());
                let my = c.ino(ino).ok().and_then(|n| n.disk).map_or(0, |d| d.off);
                let d = tr.dirs.get_mut(&parent).unwrap();
                let (queue_me, queue_next) = (!d.queued, !d.next);
                d.queued = true;
                d.next = true;
                if queue_me {
                    out.extend(dir_ranges(c, &mut tr, parent, my));
                }
                if queue_next && let Some(sib) = next_sibling(c, parent) {
                    let sd = tr.dirs.entry(sib).or_default();
                    if !sd.queued {
                        sd.queued = true;
                        if c.load(sib).is_ok() {
                            out.extend(dir_ranges(c, &mut tr, sib, 0));
                        }
                    }
                }
            }
        }
        out
    }
    pub fn write(&self, ino: Ino, off: u64, data: &[u8]) -> R<u32> {
        self.window(data.len() as u64)?;
        let r = self.mutate(|c| c.write(ino, off, data));
        let r = match r {
            Err(libc::ENOSPC) if self.has_pending_frees() => {
                // freed space becomes usable once the commit that frees it is durable
                self.flush().map_err(|_| libc::EIO)?;
                self.mutate(|c| c.write(ino, off, data))
            }
            r => r,
        };
        r?;
        self.wrote();
        Ok(data.len() as u32)
    }
    pub fn unlink(&self, parent: Ino, name: &std::ffi::OsStr) -> R<()> {
        let name = super::core::utf16(name)?;
        self.mutate(|c| c.unlink(parent, &name, false))?;
        self.wrote();
        Ok(())
    }
    pub fn rmdir(&self, parent: Ino, name: &std::ffi::OsStr) -> R<()> {
        let name = super::core::utf16(name)?;
        self.mutate(|c| c.unlink(parent, &name, true))?;
        self.wrote();
        Ok(())
    }
    pub fn rename(&self, p1: Ino, n1: &std::ffi::OsStr, p2: Ino, n2: &std::ffi::OsStr, fl: RenameFlags) -> R<()> {
        let (n1, n2) = (super::core::utf16(n1)?, super::core::utf16(n2)?);
        self.mutate(|c| c.rename(p1, &n1, p2, &n2, fl))?;
        self.wrote();
        Ok(())
    }
    pub fn statfs(&self) -> Statfs {
        self.core().statfs()
    }
    pub fn label(&self) -> String {
        self.core().label.clone()
    }
    pub fn geometry(&self) -> (u64, u64, u32) {
        let c = self.core();
        (c.vol.cc * c.vol.cs, c.vol.cs, crate::volume::le32(&c.vol.sector0, 100))
    }

    /// Every mutation runs inside the admission gate (shared side), from its `ejecting` check
    /// to its end, so `prepare-eject` (exclusive side) never reports safe while one is pending.
    fn mutate<T>(&self, f: impl FnOnce(&mut Core) -> R<T>) -> R<T> {
        let _admitted = self.sh.admit.read().unwrap_or_else(|e| e.into_inner());
        if self.sh.ejecting.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(libc::EROFS);
        }
        #[cfg(test)]
        if let Some(h) = self.sh.hook.lock().unwrap().clone() {
            h();
        }
        let mut c = self.core();
        f(&mut c)
    }

    fn has_pending_frees(&self) -> bool {
        let c = self.core();
        c.dirty.iter().any(|i| c.inodes.get(i).is_some_and(|n| !n.linked || n.data_dirty))
    }

    /// Blocks while the staged bytes are at the window (a commit is asked for).
    fn window(&self, add: u64) -> R<()> {
        let max = self.sh.opts.max_dirty;
        loop {
            let staged = self.core().stager.staged;
            if staged + add <= max || staged == 0 {
                return Ok(());
            }
            let mut st = self.sh.st.lock().unwrap();
            if let Some(m) = &st.fatal {
                let _ = m;
                return Err(libc::EIO);
            }
            if !self.sh.opts.auto_commit {
                drop(st);
                self.commit_now("pressure").map_err(|_| libc::EIO)?;
                continue;
            }
            if st.want == st.done {
                st.want += 1;
                st.reason = "pressure";
                self.sh.cv.notify_all();
            }
            let _ = self.sh.cv.wait_timeout(st, Duration::from_millis(200)).unwrap();
        }
    }

    /// Lock order everywhere: `core` before `st`, never `core` while holding `st`.
    fn wrote(&self) {
        let staged = self.core().stager.staged;
        let mut st = self.sh.st.lock().unwrap();
        st.last_write = Some(Instant::now());
        // Pipelining: past half the window, start committing.
        // (No wakeup otherwise: the commit thread polls the idle deadline every 250 ms, and a
        // wakeup per write would cost the writer more than the write.)
        if staged > self.sh.opts.max_dirty / 2 && st.want == st.done && !st.committing {
            st.want += 1;
            st.reason = "pressure";
            self.sh.cv.notify_all();
        }
    }

    pub fn status(&self) -> Status {
        let c = self.core();
        let st = self.sh.st.lock().unwrap();
        let ejecting = self.sh.ejecting.load(std::sync::atomic::Ordering::SeqCst);
        let open = c.opened;
        Status {
            state: if st.fatal.is_some() || c.read_only.is_some() && !ejecting {
                "read_only"
            } else if ejecting {
                "ejecting"
            } else if st.committing {
                "committing"
            } else if !c.dirty.is_empty() {
                "dirty"
            } else {
                "idle"
            },
            dirty_bytes: c.stager.staged,
            dirty_files: c.dirty.len() as u64,
            open_files: open,
            free_bytes: c.free_clusters() * c.vol.cs,
        }
    }

    /// Commits everything written so far; returns once it is durable.
    pub fn flush(&self) -> Result<()> {
        self.flush_why("flush")
    }

    pub fn fsync(&self, _ino: Ino) -> R<()> {
        self.flush_why("fsync").map_err(|_| libc::EIO)
    }

    fn flush_why(&self, why: &'static str) -> Result<()> {
        if !self.sh.opts.auto_commit {
            commit_all(self, why)?;
            return Ok(());
        }
        let mut st = self.sh.st.lock().unwrap();
        st.want += 1;
        st.reason = why;
        let target = st.want;
        self.sh.cv.notify_all();
        loop {
            if let Some(m) = &st.fatal {
                anyhow::bail!("{m}");
            }
            if st.done >= target {
                return Ok(());
            }
            st = self.sh.cv.wait(st).unwrap();
        }
    }

    /// Flush, refuse new writes (EROFS), flush again: then nothing is pending.
    pub fn prepare_eject(&self) -> Result<()> {
        self.flush_why("eject")?;
        {
            // no mutation is in flight once the gate is held exclusively; later ones see
            // `ejecting` and fail with EROFS
            let _all = self.sh.admit.write().unwrap_or_else(|e| e.into_inner());
            self.sh.ejecting.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.flush_why("eject")?;
        let c = self.core();
        ensure!(c.dirty.is_empty() && c.stager.staged == 0, "changes are still pending");
        Ok(())
    }

    pub fn resume(&self) {
        self.sh.ejecting.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Final commit and stop of the commit thread.
    pub fn shutdown(&self) -> Result<()> {
        let r = self.flush_why("unmount");
        {
            let mut st = self.sh.st.lock().unwrap();
            st.stop = true;
            self.sh.cv.notify_all();
        }
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
        if let Some(ra) = &self.sh.ra {
            ra.stop();
        }
        r
    }

    /// Where the drive journal is (once created).
    #[cfg(test)]
    pub(crate) fn journal_place(&self) -> Option<Place> {
        self.core().journal.clone()
    }

    /// Test hook: `f` runs inside every mutation, after admission (to hold one there).
    #[cfg(test)]
    pub(crate) fn set_hook(&self, f: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.sh.hook.lock().unwrap() = f;
    }

    /// Commits completed so far.
    pub fn commits(&self) -> u64 {
        self.core().commits
    }

    /// Readahead cache hits and misses (tests, benchmarks).
    pub fn ra_stats(&self) -> Option<(u64, u64)> {
        self.sh.ra.as_ref().map(|r| r.stats())
    }

    /// Bytes still pending (for "lost" reports).
    pub fn pending_bytes(&self) -> u64 {
        self.core().stager.staged
    }

    fn commit_loop(&self) {
        let mut last_dirty_note = (u64::MAX, u64::MAX);
        loop {
            let mut st = self.sh.st.lock().unwrap();
            loop {
                if st.stop {
                    return;
                }
                if st.fatal.is_some() {
                    // nothing more can be committed; wake waiters
                    st.done = st.want;
                    self.sh.cv.notify_all();
                }
                if st.want > st.done {
                    break;
                }
                let idle_due = st.last_write.map(|t| t.elapsed() >= self.sh.opts.idle).unwrap_or(false);
                if idle_due {
                    st.last_write = None;
                    st.want += 1;
                    st.reason = "idle";
                    break;
                }
                let wait = st.last_write.map_or(Duration::from_millis(250), |t| {
                    self.sh.opts.idle.saturating_sub(t.elapsed()).max(Duration::from_millis(5))
                });
                st = self.sh.cv.wait_timeout(st, wait.min(Duration::from_millis(250))).unwrap().0;
                drop(st);
                let s = self.status();
                if (s.dirty_bytes, s.dirty_files) != last_dirty_note {
                    last_dirty_note = (s.dirty_bytes, s.dirty_files);
                    self.note(Note::Dirty { bytes: s.dirty_bytes, files: s.dirty_files });
                }
                st = self.sh.st.lock().unwrap();
            }
            let target = st.want;
            let why = st.reason;
            st.committing = true;
            drop(st);
            let r = (|| -> Result<()> {
                commit_all(self, why)?;
                Ok(())
            })();
            let mut st = self.sh.st.lock().unwrap();
            st.committing = false;
            if let Err(e) = r {
                let m = format!("{e:#}");
                st.fatal = Some(m.clone());
                drop(st);
                self.core().read_only = Some(libc::EIO);
                self.note(Note::Error { message: m, fatal: true });
                st = self.sh.st.lock().unwrap();
            }
            st.done = st.done.max(target);
            self.sh.cv.notify_all();
            drop(st);
            let s = self.status();
            if (s.dirty_bytes, s.dirty_files) != last_dirty_note {
                last_dirty_note = (s.dirty_bytes, s.dirty_files);
                self.note(Note::Dirty { bytes: s.dirty_bytes, files: s.dirty_files });
            }
        }
    }

    /// Plans, executes and finalizes one commit. Ok(false) when there was nothing to do.
    pub(crate) fn commit_now(&self, why: &'static str) -> Result<bool> {
        if let Some(m) = &self.sh.st.lock().unwrap().fatal {
            anyhow::bail!("an earlier commit failed: {m}");
        }
        let t0 = Instant::now();
        let (plan, place, seq) = {
            let mut c = self.core();
            if c.dirty.is_empty() {
                return Ok(false);
            }
            if c.journal.is_none() {
                bootstrap(&mut c, &*self.sh.dev)?;
            }
            // no reader of committed data may still be reading what this plan can reuse
            drop(self.sh.gate.write().unwrap_or_else(|e| e.into_inner()));
            // the commit's journal must fit the journal file: plan fewer inodes until it does
            let cap = c.journal.as_ref().map_or(0, |p| p.len);
            let mut limit = None;
            let plan = loop {
                let Some(plan) = commit::plan(&mut c, limit)? else { return Ok(false) };
                let est = djournal::encode_body(&c.vol, &plan.writes, &[])?.len() as u64
                    + 2 * 20 * plan.segs.len() as u64
                    + 1024;
                let raw = djournal::raw_len(&c.vol, &plan.writes, 2 * plan.segs.len());
                if est <= cap.min(djournal::MAX_BODY) && raw <= djournal::MAX_RAW {
                    break plan;
                }
                let next = limit.unwrap_or(plan.inodes).min(plan.inodes) / 2;
                anyhow::ensure!(next >= 1, "one change needs more journal than the volume's journal file holds");
                limit = Some(next);
            };
            if let Some(ra) = &self.sh.ra {
                let r: Vec<(u64, u64)> = plan.segs.iter().map(|s| (s.dev, s.len)).collect();
                ra.invalidate(&r);
            }
            commit::begin(&mut c, &plan);
            (plan, c.journal.clone().unwrap(), c.commits + 1)
        };
        self.note(Note::Committing { bytes: plan.bytes, files: plan.files, reason: why });
        // The executor reads the volume through its own handle on the same file.
        let vol_view = { self.core().vol.view()? };
        execute(&*self.sh.dev, &vol_view, &plan, &place, seq)?;
        let (bytes, files) = (plan.bytes, plan.files);
        let mut c = self.core();
        if let Some(ra) = &self.sh.ra {
            // chunks read or queued while the commit ran may hold what was there before
            let r: Vec<(u64, u64)> = plan.segs.iter().map(|s| (s.dev, s.len)).collect();
            ra.invalidate(&r);
        }
        commit::finalize(&mut c, plan);
        drop(c);
        self.note(Note::Committed { bytes, files, secs: t0.elapsed().as_secs_f64() });
        Ok(true)
    }
}

/// Commits until every change made before the call is durable (a split batch takes several
/// commits; changes made meanwhile may wait for the next request).
fn commit_all(fs: &Fs, why: &'static str) -> Result<()> {
    let target = fs.core().seq;
    while fs.core().durable_seq < target {
        if !fs.commit_now(why)? {
            return Ok(());
        }
    }
    Ok(())
}

/// Device ranges of committed bytes [a, b) of `ino` (clamped to what is still content).
fn base_ranges(c: &mut Core, ino: Ino, a: u64, b: u64) -> Vec<(u64, u64)> {
    let Ok(n) = c.ino(ino) else { return Vec::new() };
    let b = b.min(n.keep);
    if a >= b {
        return Vec::new();
    }
    let Ok(runs) = c.runs(ino) else { return Vec::new() };
    let cs = c.vol.cs;
    let mut out = Vec::new();
    let mut logical = 0u64;
    for (s, cnt) in runs {
        let rl = cnt as u64 * cs;
        let (ra, rb) = (logical.max(a), (logical + rl).min(b));
        if ra < rb {
            out.push((c.vol.cluster_off(s) + (ra - logical), rb - ra));
        }
        logical += rl;
        if logical >= b {
            break;
        }
    }
    out
}

/// Writes a commit: data, journal, the journaled phases, the idle header.
pub(crate) fn execute(dev: &dyn Dev, v: &Vol, plan: &Plan, place: &Place, seq: u64) -> Result<()> {
    crate::sim::trace::phase("data");
    let mut samples = match dev.file() {
        Some(f) if !plan.segs.is_empty() => {
            let cs = v.cs;
            let direct_ok = cs.is_multiple_of(4096) && v.heap_off.is_multiple_of(4096);
            // one probe segment per contiguous run (whole clusters)
            let mut probe: Vec<crate::add::ProbeSeg> = Vec::new();
            for s in &plan.segs {
                match probe.last_mut() {
                    Some(l) if l.dev + l.payload == s.dev => l.payload += s.len,
                    _ => probe.push(crate::add::ProbeSeg { dev: s.dev, len: 0, payload: s.len, file_end: true }),
                }
            }
            for p in &mut probe {
                p.len = p.payload.div_ceil(cs) * cs;
            }
            let pol = crate::add::choose_gap_policy(f, direct_ok, v, 0, false, &probe);
            if std::env::var_os("FLASHCP_DEBUG_DATA").is_some() {
                eprintln!("data: {} segments, {} runs, cs {cs}, {}", plan.segs.len(), probe.len(), pol.describe());
            }
            let direct = direct_ok && crate::exfat::set_direct(f, true);
            let w = if direct {
                crate::exfat::SeqWriter::new(f.clone())
            } else {
                crate::exfat::SeqWriter::with_align(f.clone(), 1)
            };
            let mut out = Out::Seq {
                w: Some(w),
                pol,
                pre_bm: plan.pre_bm.clone(),
                heap: v.heap_off,
                cs,
                align: if direct { 4096 } else { 1 },
            };
            let r = write_data(dev, &plan.segs, &mut out);
            if direct {
                crate::exfat::set_direct(f, false);
            }
            r?
        }
        _ => write_data(dev, &plan.segs, &mut Out::Dev)?,
    };
    // Data that later metadata lands on (a directory's extension) is checked by the journal.
    samples.retain(|&(off, len, _)| {
        len > 0 && !plan.writes.iter().any(|w| w.off < off + len as u64 && off < w.off + w.post.len() as u64)
    });
    let body = djournal::encode_body(v, &plan.writes, &samples)?;
    crate::sim::trace::phase("journal");
    djournal::write_active(dev, v, place, seq, &body)?;
    djournal::apply(dev, &plan.writes, |_| {})?;
    crate::sim::trace::phase("idle");
    djournal::write_idle(dev, v, place, seq)
}

/// Writes the data segments (ascending) in buffers of up to 4 MB per contiguous run, then
/// syncs. Returns samples (head and tail ≤ 512 bytes of every run) for the journal.
/// Where the data step's bytes go: plain device writes (tests' crash device), or add's
/// sequential O_DIRECT writer with its gap policy (fill a gap with zeros when that is cheaper
/// than a new request: only our own slack, or clusters the pre-commit bitmap calls free).
#[allow(clippy::large_enum_variant)]
enum Out {
    Dev,
    Seq {
        w: Option<crate::exfat::SeqWriter>,
        pol: crate::add::GapPolicy,
        pre_bm: Arc<Vec<u8>>,
        heap: u64,
        cs: u64,
        align: u64,
    },
}

impl Out {
    fn begin_run(&mut self, start: u64) -> Result<()> {
        if let Out::Seq { w: Some(w), pol, pre_bm, heap, cs, .. } = self {
            let cur = w.cur();
            if start > cur {
                let first = cur > 0;
                let own_end = *heap + (cur.saturating_sub(*heap)).div_ceil(*cs) * *cs;
                let foreign = (own_end - *heap) / *cs + 2..(start - *heap) / *cs + 2;
                if first && crate::add::fill_gap(start - cur, foreign, pre_bm, pol) {
                    w.zeros_to(start)?;
                } else {
                    w.skip_to(start)?;
                }
            }
        }
        Ok(())
    }
    fn write(&mut self, dev: &dyn Dev, at: u64, buf: &[u8]) -> Result<()> {
        match self {
            Out::Dev => dev.write_at(buf, at).with_context(|| format!("write data at {at}")),
            Out::Seq { w: Some(w), .. } => {
                anyhow::ensure!(w.cur() == at, "internal error: data stream at {} not {at}", w.cur());
                w.write(buf)
            }
            Out::Seq { .. } => unreachable!(),
        }
    }
    fn end_run(&mut self) -> Result<()> {
        if let Out::Seq { w: Some(w), heap, cs, align, .. } = self {
            let cur = w.cur();
            let cluster_end = *heap + (cur - *heap).div_ceil(*cs) * *cs;
            w.zeros_to((cur.div_ceil(*align) * *align).min(cluster_end))?;
        }
        Ok(())
    }
    fn finish(&mut self, dev: &dyn Dev) -> Result<()> {
        match self {
            Out::Dev => dev.sync().context("sync"),
            Out::Seq { w, .. } => w.take().unwrap().finish().map(|_| ()),
        }
    }
}

fn write_data(dev: &dyn Dev, segs: &[Seg], out: &mut Out) -> Result<Vec<djournal::Sample>> {
    const BUF: u64 = 4 << 20;
    let mut samples = Vec::new();
    if segs.is_empty() {
        return Ok(samples);
    }
    let mut k = 0;
    while k < segs.len() {
        // one contiguous run
        let mut j = k + 1;
        while j < segs.len() && segs[j].dev == segs[j - 1].dev + segs[j - 1].len {
            j += 1;
        }
        let (start, end) = (segs[k].dev, segs[j - 1].dev + segs[j - 1].len);
        out.begin_run(start)?;
        let mut head: Vec<u8> = Vec::new();
        let mut tail: Vec<u8> = Vec::new();
        let mut at = start;
        let mut si = k;
        let mut s_off = 0u64;
        while at < end {
            let n = (end - at).min(BUF);
            let mut buf = vec![0u8; n as usize];
            let mut filled = 0u64;
            while filled < n {
                let s = &segs[si];
                let take = (s.len - s_off).min(n - filled);
                let dst = &mut buf[filled as usize..(filled + take) as usize];
                match &s.src {
                    Src::Zero => {}
                    Src::Base(off) => dev.read_at(dst, off + s_off).context("read committed data")?,
                    Src::Piece(p, po) => p.read(po + s_off, dst).context("read staged data")?,
                    Src::Mem(m, mo) => {
                        let a = mo + s_off as usize;
                        let avail = m.len().saturating_sub(a).min(dst.len());
                        dst[..avail].copy_from_slice(&m[a..a + avail]);
                    }
                }
                filled += take;
                s_off += take;
                if s_off == s.len {
                    si += 1;
                    s_off = 0;
                }
            }
            if head.len() < 512 {
                let want = (512 - head.len()).min(buf.len());
                head.extend_from_slice(&buf[..want]);
            }
            let tl = buf.len().min(512);
            if tl == 512 || tail.len() + tl > 512 {
                tail = buf[buf.len() - tl..].to_vec();
            } else {
                tail.extend_from_slice(&buf[buf.len() - tl..]);
            }
            out.write(dev, at, &buf)?;
            at += n;
        }
        out.end_run()?;
        let tail = if tail.len() > 512 { tail[tail.len() - 512..].to_vec() } else { tail };
        samples.push((start, head.len() as u32, xxhash_rust::xxh3::xxh3_64(&head)));
        samples.push((end - tail.len() as u64, tail.len() as u32, xxhash_rust::xxh3::xxh3_64(&tail)));
        k = j;
    }
    out.finish(dev)?;
    Ok(samples)
}

/// Creates the journal file (see [`djournal`]): header, sync; bitmap bits, sync; entry set in
/// one root sector, sync.
fn bootstrap(c: &mut Core, dev: &dyn Dev) -> Result<()> {
    let cs = c.vol.cs;
    let cap = djournal::capacity(c.vol.vol_bytes, cs);
    let n = cap / cs;
    let free = crate::add::free_extents(&c.bm, c.vol.cc);
    let Some(&(start, _)) = free.iter().filter(|e| e.1 >= n).min_by_key(|e| (e.1, e.0)) else {
        anyhow::bail!("no contiguous free space for the drive journal ({} bytes)", cap);
    };
    let first = start as u32;
    let set = djournal::entry(&c.vol.upc, first, cap);
    let root = c.inodes.get(&super::core::ROOT).unwrap().dd.clone().context("root not loaded")?;
    let Some((off, pad)) = djournal::one_sector_slot(&root.data, root.end, set.len()) else {
        anyhow::bail!("the root directory has no room for the drive journal's entry");
    };
    // 1. header
    let base = c.vol.cluster_off(first);
    let mut h = vec![0u8; 512];
    {
        // an IDLE header with sequence 0
        let place = Place { first, len: cap, nfc: true };
        let v = c.vol.view()?;
        djournal::write_idle(dev, &v, &place, 0)?;
        let _ = (base, &mut h);
    }
    // 2. bitmap bits
    let mut bm = c.bm.clone();
    let (a, b) = (((start - 2) / 8) as usize, ((start - 2 + n - 1) / 8) as usize + 1);
    for x in start..start + n {
        let i = (x - 2) as usize;
        bm[i / 8] |= 1 << (i % 8);
    }
    write_object(dev, &c.vol, &c.vol.bitmap_clusters.clone(), a, &bm[a..b])?;
    dev.sync()?;
    // 3. entry set (and deleted-entry padding up to it), one sector
    let mut data = root.data.clone();
    let mut lo = off;
    if pad > 0 {
        lo = root.end;
        for k in 0..pad {
            let e = root.end + 32 * k;
            data[e..e + 32].fill(0);
            data[e] = 0x05;
        }
    }
    data[off..off + set.len()].copy_from_slice(&set);
    let hi = off + set.len();
    write_object(dev, &c.vol, &root.clusters, lo, &data[lo..hi])?;
    dev.sync()?;
    c.bm = bm;
    c.used += n;
    let r = c.inodes.get_mut(&super::core::ROOT).unwrap();
    let dd = r.dd.as_mut().unwrap();
    dd.data = data;
    if off >= dd.end {
        dd.end = hi;
    }
    c.journal = Some(Place { first, len: cap, nfc: true });
    Ok(())
}

fn write_object(dev: &dyn Dev, v: &Vol, clusters: &[u32], a: usize, bytes: &[u8]) -> Result<()> {
    let cs = v.cs as usize;
    let mut k = 0;
    while k < bytes.len() {
        let at = a + k;
        let n = (cs - at % cs).min(bytes.len() - k);
        dev.write_at(&bytes[k..k + n], v.cluster_off(clusters[at / cs]) + (at % cs) as u64)?;
        k += n;
    }
    Ok(())
}

impl Drop for Shared {
    fn drop(&mut self) {}
}

#[allow(dead_code)]
fn _errno(e: Errno) -> Errno {
    e
}
