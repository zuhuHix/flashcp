//! `flashcp mount` (SPIKE): serve an exFAT volume through FUSE ("drive mode").
//!
//! Reads go through the validating parser in `volume.rs`. New files and directories are staged
//! (RAM-backed tmpfs up to a cap, then a directory on the local disk) and committed in one
//! sequential pass with `add`'s engine ([`crate::add::add_tree`]) when writes go idle, on
//! fsync, or on unmount. Between commits the volume on the device is untouched, so a crash
//! loses only staged data; the add engine's crash ordering covers a crash mid-commit.
//!
//! Deliberately missing in the spike (see DESIGN.md): writing into, truncating, renaming or
//! deleting files that are already committed; renaming non-empty staged directories; xattrs,
//! symlinks, hard links, mmap writes of committed files.

use crate::add::{AddOpts, add_tree};
use crate::exfat::{Node, TargetSpec, validate_name};
use crate::volume::{Child, Vol, decode_ts, upcase};
use anyhow::{Context, Result, anyhow};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct MountOpts {
    /// Commit after this long without writes (and no file open for writing).
    pub idle: Duration,
    /// Stage new data in RAM (tmpfs) up to this many bytes, then on the local disk.
    pub ram_cap: u64,
    /// Ask the kernel for FUSE writeback caching (bigger, fewer writes).
    pub writeback: bool,
    pub readers: usize,
    /// FUSE session threads.
    pub threads: usize,
}

impl Default for MountOpts {
    fn default() -> Self {
        MountOpts { idle: Duration::from_millis(1500), ram_cap: 512 << 20, writeback: false, readers: 4, threads: 1 }
    }
}

type Key = Vec<Vec<u16>>; // up-cased path components

const TTL: Duration = Duration::from_secs(1);

struct Staged {
    name: Vec<u16>,
    is_dir: bool,
    /// Staging file (files only).
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
}

struct Handle {
    ino: u64,
    file: Option<File>,
    write: bool,
}

struct State {
    target: PathBuf,
    vol: Vol,
    /// Free clusters of `vol` (counting the bitmap per write was ~1 ms on a 1 TB volume).
    vol_free: u64,
    uid: u32,
    gid: u32,
    // inode table: ino → key; key → ino. Never shrinks (spike).
    keys: Vec<Key>,
    inos: HashMap<Key, u64>,
    staged: HashMap<Key, Staged>,
    /// parent key → staged-or-merge children (key → original name).
    overlay: HashMap<Key, BTreeMap<Vec<u16>, Vec<u16>>>,
    staged_bytes: u64,
    clusters: HashMap<u64, (Vec<u32>, u64)>,
    handles: HashMap<u64, Handle>,
    dir_snaps: HashMap<u64, Vec<(u64, FileType, String)>>,
    next_fh: u64,
    next_stage: u64,
    open_writers: usize,
    last_write: Instant,
    dirty: bool,
    commit_failed: bool,
    /// Inodes whose data is committed while a write handle stays open (after fsync): further
    /// writes on them are refused.
    frozen: HashSet<u64>,
    ram_dir: PathBuf,
    disk_dir: PathBuf,
    o: MountOpts,
    commits: u32,
    /// Physical-order readahead: (device offset, bytes), oldest first, at most RA_CAP in total.
    ra: std::collections::VecDeque<(u64, Vec<u8>)>,
    ra_bytes: u64,
    /// Debug counters: (readahead secs, read handler secs, reads, ra hits).
    prof: (f64, f64, u64, u64),
    /// Files of each committed directory opened for reading; the third triggers readahead.
    dir_reads: HashMap<Key, u32>,
}

/// Readahead: files of one directory closer than RA_GAP on the device are read as one run
/// (16 KB: on a USB 2 stick with 32 KB clusters, reading through slack made reads 1.5x
/// slower; `FLASHCP_RA_GAP_KB` overrides it for experiments).
const RA_GAP: u64 = 16 << 10;
const RA_RUN_MAX: u64 = 64 << 20;
const RA_CAP: u64 = 256 << 20;
const RA_TRIGGER: u32 = 3;

fn open_vol(target: &Path) -> Result<Vol> {
    let f = File::open(target).with_context(|| format!("open {}", target.display()))?;
    let md = f.metadata()?;
    let len = if std::os::unix::fs::FileTypeExt::is_block_device(&md.file_type()) {
        crate::exfat::blk_size(&f)?
    } else {
        md.len()
    };
    Vol::open(f, len, false, true)
}

fn ts(t: SystemTime) -> (i64, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64, d.subsec_nanos())
}

fn os_name(name: &OsStr) -> Option<Vec<u16>> {
    name.to_str().map(|s| s.encode_utf16().collect())
}

enum Obj {
    Staged { is_dir: bool, size: u64, mtime: SystemTime },
    Committed(Child),
    Root,
}

impl State {
    fn ino_of(&mut self, key: &Key) -> u64 {
        if let Some(&i) = self.inos.get(key) {
            return i;
        }
        self.keys.push(key.clone());
        let i = self.keys.len() as u64; // ino 1 = root = keys[0]
        self.inos.insert(key.clone(), i);
        i
    }

    fn key(&self, ino: u64) -> Option<Key> {
        self.keys.get((ino as usize).checked_sub(1)?).cloned()
    }

    /// Index in vol.dirs of a committed directory, parsing it on first use.
    fn dir_idx(&mut self, key: &[Vec<u16>]) -> Result<Option<usize>> {
        if let Some(&i) = self.vol.kept.get(key) {
            return Ok(Some(i));
        }
        let Some((last, parent)) = key.split_last() else { return Ok(Some(0)) };
        let Some(p) = self.dir_idx(parent)? else { return Ok(None) };
        let Some(ch) = self.vol.dirs[p].children.get(last).cloned() else { return Ok(None) };
        if !ch.is_dir {
            return Ok(None);
        }
        let path = self.vol.dirs[p].path.clone();
        let mut d = self.vol.read_child(p, &path, &ch)?;
        d.key = key.to_vec();
        self.vol.dirs.push(d);
        let i = self.vol.dirs.len() - 1;
        self.vol.kept.insert(key.to_vec(), i);
        Ok(Some(i))
    }

    fn committed(&mut self, key: &Key) -> Result<Option<Child>> {
        let Some((last, parent)) = key.split_last() else { return Ok(None) };
        let Some(p) = self.dir_idx(parent)? else { return Ok(None) };
        Ok(self.vol.dirs[p].children.get(last).cloned())
    }

    fn obj(&mut self, key: &Key) -> Result<Option<Obj>> {
        if key.is_empty() {
            return Ok(Some(Obj::Root));
        }
        if let Some(s) = self.staged.get(key) {
            return Ok(Some(Obj::Staged { is_dir: s.is_dir, size: s.size, mtime: s.mtime }));
        }
        Ok(self.committed(key)?.map(Obj::Committed))
    }

    fn attr(&self, ino: u64, o: &Obj) -> FileAttr {
        let (is_dir, size, mtime) = match o {
            Obj::Root => (true, 0, UNIX_EPOCH),
            Obj::Staged { is_dir, size, mtime } => (*is_dir, *size, *mtime),
            Obj::Committed(c) => {
                let m = decode_ts(c.mtime.0, c.mtime.1, c.mtime.2)
                    .map(|(s, n)| UNIX_EPOCH + Duration::new(s.max(0) as u64, n))
                    .unwrap_or(UNIX_EPOCH);
                (c.is_dir, if c.is_dir { 0 } else { c.len }, m)
            }
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: if is_dir { FileType::Directory } else { FileType::RegularFile },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: if is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: self.vol.cs as u32,
            flags: 0,
        }
    }

    fn child_key(&self, parent: u64, name: &OsStr) -> Result<(Key, Vec<u16>), Errno> {
        let mut k = self.key(parent).ok_or(Errno::ENOENT)?;
        let n = os_name(name).ok_or(Errno::EINVAL)?;
        k.push(upcase(&n, &self.vol.upc));
        Ok((k, n))
    }

    fn free_bytes(&self) -> u64 {
        let cs = self.vol.cs;
        (self.vol_free * cs).saturating_sub(self.staged_bytes.div_ceil(cs) * cs)
    }

    /// Registers a new staged object and the merge chain of its parents.
    fn stage(&mut self, key: Key, name: Vec<u16>, is_dir: bool) -> Result<(), Errno> {
        let parent: Key = key[..key.len() - 1].to_vec();
        match self.obj(&parent) {
            Ok(Some(Obj::Root)) | Ok(Some(Obj::Staged { is_dir: true, .. })) => {}
            Ok(Some(Obj::Committed(c))) if c.is_dir => {}
            Ok(_) => return Err(Errno::ENOTDIR),
            Err(e) => {
                eprintln!("flashcp mount: {e:#}");
                return Err(Errno::EIO);
            }
        }
        validate_name(&name, &self.vol.upc, &mut HashSet::new(), Path::new("?")).map_err(|_| Errno::EINVAL)?;
        let path = if is_dir {
            PathBuf::new()
        } else {
            self.next_stage += 1;
            let dir = if self.staged_bytes < self.o.ram_cap { &self.ram_dir } else { &self.disk_dir };
            dir.join(self.next_stage.to_string())
        };
        // overlay chain: parent's parents get "merge" entries (original names from the volume).
        let mut k = key.clone();
        let mut nm = name.clone();
        while let Some(last) = k.pop() {
            let fresh = self.overlay.entry(k.clone()).or_default().insert(last, nm.clone()).is_none();
            if !fresh || k.is_empty() {
                break;
            }
            nm = match self.staged.get(&k) {
                Some(s) => s.name.clone(),
                None => match self.committed(&k) {
                    Ok(Some(c)) => c.name,
                    _ => return Err(Errno::EIO),
                },
            };
        }
        self.staged.insert(key, Staged { name, is_dir, path, size: 0, mtime: SystemTime::now() });
        self.dirty = true;
        self.commit_failed = false;
        self.last_write = Instant::now();
        Ok(())
    }

    fn build_tree(&self) -> Vec<Node> {
        let mut nodes = vec![Node {
            name: vec![],
            src: PathBuf::new(),
            is_dir: true,
            size: 0,
            mtime: (0, 0),
            children: vec![],
            first_cluster: 0,
            clusters: 0,
        }];
        let mut stack = vec![(0usize, Key::new())];
        while let Some((n, key)) = stack.pop() {
            let Some(kids) = self.overlay.get(&key) else { continue };
            for (up, name) in kids {
                let mut ck = key.clone();
                ck.push(up.clone());
                let node = match self.staged.get(&ck) {
                    Some(s) => Node {
                        name: s.name.clone(),
                        src: s.path.clone(),
                        is_dir: s.is_dir,
                        size: s.size,
                        mtime: ts(s.mtime),
                        children: vec![],
                        first_cluster: 0,
                        clusters: 0,
                    },
                    None => Node {
                        // existing directory on the volume: merged into, not created
                        name: name.clone(),
                        src: PathBuf::new(),
                        is_dir: true,
                        size: 0,
                        mtime: ts(SystemTime::now()),
                        children: vec![],
                        first_cluster: 0,
                        clusters: 0,
                    },
                };
                let is_dir = node.is_dir;
                nodes.push(node);
                let i = nodes.len() - 1;
                nodes[n].children.push(i);
                if is_dir {
                    stack.push((i, ck));
                }
            }
        }
        nodes
    }

    /// Commits every staged object with add's engine, then re-reads the volume.
    fn commit(&mut self, why: &str) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let t0 = Instant::now();
        let nodes = self.build_tree();
        let n_files = self.staged.values().filter(|s| !s.is_dir).count();
        let bytes = self.staged_bytes;
        eprintln!("flashcp mount: commit start ({why}): {n_files} files, {:.1} MB", bytes as f64 / 1e6);
        // Release our read fd's parse state before the exclusive open.
        let o = AddOpts { to: None, skip_existing: false, readers: self.o.readers, dry_run: false };
        let mut sink = |_e: &crate::events::Event| {};
        let r = add_tree(&TargetSpec::Path(self.target.clone()), &o, &mut sink, move |_upc| Ok(nodes));
        match r {
            Ok(s) => {
                self.vol = open_vol(&self.target)?;
                self.vol_free = self.vol.free_clusters();
                for s in self.staged.values() {
                    if !s.is_dir {
                        let _ = fs::remove_file(&s.path);
                    }
                }
                self.staged.clear();
                self.overlay.clear();
                self.clusters.clear();
                self.ra.clear();
                self.ra_bytes = 0;
                self.dir_reads.clear();
                self.staged_bytes = 0;
                self.dirty = false;
                self.commits += 1;
                eprintln!(
                    "flashcp mount: commit #{} ({why}): {n_files} files, {} dirs, {:.1} MB in {:.3}s (engine {:.3}s, wrote {:.1} MB)",
                    self.commits,
                    s.dirs,
                    bytes as f64 / 1e6,
                    t0.elapsed().as_secs_f64(),
                    s.secs,
                    s.written as f64 / 1e6
                );
                Ok(())
            }
            Err(e) => {
                self.commit_failed = true;
                // The volume may have been partially written (phases before linking): re-read.
                if let Ok(v) = open_vol(&self.target) {
                    self.vol = v;
                    self.vol_free = self.vol.free_clusters();
                    self.clusters.clear();
                }
                Err(e)
            }
        }
    }

    /// Reads the contiguous files of a directory in physical order, a few large reads.
    fn readahead(&mut self, files: &[Child]) {
        if std::env::var_os("FLASHCP_NO_RA").is_some() {
            return;
        }
        let t0 = Instant::now();
        self.readahead_inner(files);
        self.prof.0 += t0.elapsed().as_secs_f64();
    }

    fn readahead_inner(&mut self, files: &[Child]) {
        let mut spans: Vec<(u64, u64)> = files
            .iter()
            .filter(|c| !c.is_dir && c.no_fat_chain && c.valid_len > 0 && c.valid_len <= RA_RUN_MAX)
            .map(|c| {
                let a = self.vol.cluster_off(c.first);
                (a, a + c.valid_len.min(c.len))
            })
            .filter(|&(a, _)| !self.ra.iter().any(|(o, b)| a >= *o && a < *o + b.len() as u64))
            .collect();
        spans.sort();
        let gap =
            std::env::var("FLASHCP_RA_GAP_KB").ok().and_then(|v| v.parse::<u64>().ok()).map_or(RA_GAP, |k| k << 10);
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for (a, b) in spans {
            match runs.last_mut() {
                Some(r) if a <= r.1 + gap && b - r.0 <= RA_RUN_MAX => r.1 = r.1.max(b),
                _ => runs.push((a, b)),
            }
        }
        for (a, b) in runs {
            if b - a < 64 << 10 && files.len() < 4 {
                continue; // not worth it
            }
            let mut buf = vec![0u8; (b - a) as usize];
            if b > self.vol.dev_len || self.vol.dev.read_exact_at(&mut buf, a).is_err() {
                continue;
            }
            if std::env::var_os("FLASHCP_RA_DEBUG").is_some() {
                eprintln!("ra {} files -> run {a}..{b} ({} KB)", files.len(), (b - a) >> 10);
            }
            self.ra_bytes += buf.len() as u64;
            self.ra.push_back((a, buf));
            while self.ra_bytes > RA_CAP {
                let (_, old) = self.ra.pop_front().unwrap();
                self.ra_bytes -= old.len() as u64;
            }
        }
    }

    fn read_dev(&self, out: &mut [u8], dev: u64) -> std::io::Result<()> {
        let end = dev + out.len() as u64;
        for (o, b) in self.ra.iter().rev() {
            if dev >= *o && end <= *o + b.len() as u64 {
                out.copy_from_slice(&b[(dev - o) as usize..(end - o) as usize]);
                return Ok(());
            }
        }
        self.vol.dev.read_exact_at(out, dev)
    }

    fn read_committed(&mut self, ino: u64, key: &Key, off: u64, size: u32) -> Result<Vec<u8>> {
        if !self.clusters.contains_key(&ino) {
            let ch = self.committed(key)?.ok_or_else(|| anyhow!("gone"))?;
            let cl = self.vol.object_clusters(ch.first, ch.no_fat_chain, ch.len, "file")?;
            self.clusters.insert(ino, (cl, ch.valid_len.min(ch.len)));
        }
        let (cl, valid) = &self.clusters[&ino];
        let cs = self.vol.cs;
        let len_total = cl.len() as u64 * cs; // reads past DataLength are trimmed below by the caller
        let end = (off + size as u64).min(len_total);
        let mut out = vec![0u8; end.saturating_sub(off) as usize];
        let mut pos = off;
        while pos < end.min(*valid) {
            let ci = (pos / cs) as usize;
            // extend over physically contiguous clusters
            let mut cj = ci + 1;
            while cj < cl.len() && cl[cj] == cl[cj - 1] + 1 && (cj as u64) * cs < end {
                cj += 1;
            }
            let run_end = ((cj as u64) * cs).min(end).min(*valid);
            let dev = self.vol.cluster_off(cl[ci]) + pos % cs;
            let n = (run_end - pos) as usize;
            let o = (pos - off) as usize;
            self.read_dev(&mut out[o..o + n], dev)?;
            pos = run_end;
        }
        Ok(out)
    }
}

pub struct DriveFs {
    st: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    writeback: bool,
}

macro_rules! tri {
    ($r:expr, $reply:expr) => {
        match $r {
            Ok(v) => v,
            Err(e) => {
                $reply.error(e);
                return;
            }
        }
    };
}

fn io<T>(r: Result<T>) -> Result<T, Errno> {
    r.map_err(|e| {
        eprintln!("flashcp mount: {e:#}");
        Errno::EIO
    })
}

impl Filesystem for DriveFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.set_max_write(1 << 20);
        let _ = config.set_max_readahead(1 << 20);
        if self.writeback {
            let _ = config.add_capabilities(InitFlags::FUSE_WRITEBACK_CACHE);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut st = self.st.lock().unwrap();
        if st.prof.2 > 0 {
            let p = st.prof;
            eprintln!("flashcp mount: readahead {:.3}s, read handlers {:.3}s for {} reads", p.0, p.1, p.2);
        }
        if let Err(e) = st.commit("unmount") {
            eprintln!("flashcp mount: FINAL COMMIT FAILED, staged data was NOT written: {e:#}");
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut st = self.st.lock().unwrap();
        let (k, _) = tri!(st.child_key(parent.0, name), reply);
        let o = tri!(io(st.obj(&k)), reply);
        let Some(o) = o else { return reply.error(Errno::ENOENT) };
        let ino = st.ino_of(&k);
        reply.entry(&TTL, &st.attr(ino, &o), Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let mut st = self.st.lock().unwrap();
        let k = tri!(st.key(ino.0).ok_or(Errno::ENOENT), reply);
        let o = tri!(io(st.obj(&k)), reply);
        let Some(o) = o else { return reply.error(Errno::ENOENT) };
        reply.attr(&TTL, &st.attr(ino.0, &o));
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut st = self.st.lock().unwrap();
        let k = tri!(st.key(ino.0).ok_or(Errno::ENOENT), reply);
        let o = tri!(io(st.obj(&k)), reply);
        let Some(o) = o else { return reply.error(Errno::ENOENT) };
        match o {
            Obj::Staged { .. } => {
                let st = &mut *st;
                let s = st.staged.get_mut(&k).unwrap();
                if let Some(sz) = size {
                    if s.is_dir {
                        return reply.error(Errno::EISDIR);
                    }
                    if OpenOptions::new().write(true).open(&s.path).and_then(|f| f.set_len(sz)).is_err() {
                        return reply.error(Errno::EIO);
                    }
                    st.staged_bytes = st.staged_bytes - s.size + sz;
                    s.size = sz;
                    st.last_write = Instant::now();
                }
                if let Some(m) = mtime {
                    s.mtime = match m {
                        TimeOrNow::SpecificTime(t) => t,
                        TimeOrNow::Now => SystemTime::now(),
                    };
                }
                let o = Obj::Staged { is_dir: s.is_dir, size: s.size, mtime: s.mtime };
                reply.attr(&TTL, &st.attr(ino.0, &o));
            }
            other => {
                // Committed: size changes are refused; mode/owner/time changes are ignored (spike).
                if let (Some(sz), Obj::Committed(c)) = (size, &other)
                    && sz != c.len
                {
                    return reply.error(Errno::EOPNOTSUPP);
                }
                reply.attr(&TTL, &st.attr(ino.0, &other));
            }
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let mut st = self.st.lock().unwrap();
        let (k, n) = tri!(st.child_key(parent.0, name), reply);
        if tri!(io(st.obj(&k)), reply).is_some() {
            return reply.error(Errno::EEXIST);
        }
        tri!(st.stage(k.clone(), n, true), reply);
        let ino = st.ino_of(&k);
        let o = tri!(io(st.obj(&k)), reply).unwrap();
        reply.entry(&TTL, &st.attr(ino, &o), Generation(0));
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let mut st = self.st.lock().unwrap();
        let (k, n) = tri!(st.child_key(parent.0, name), reply);
        if tri!(io(st.obj(&k)), reply).is_some() {
            return reply.error(Errno::EEXIST);
        }
        tri!(st.stage(k.clone(), n, false), reply);
        let ino = st.ino_of(&k);
        let path = st.staged[&k].path.clone();
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path);
        let file = tri!(file.map_err(|_| Errno::EIO), reply);
        st.next_fh += 1;
        let fh = st.next_fh;
        st.handles.insert(fh, Handle { ino, file: Some(file), write: true });
        st.open_writers += 1;
        let o = tri!(io(st.obj(&k)), reply).unwrap();
        reply.created(&TTL, &st.attr(ino, &o), Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let mut st = self.st.lock().unwrap();
        let k = tri!(st.key(ino.0).ok_or(Errno::ENOENT), reply);
        let write = (flags.0 & libc::O_ACCMODE) != libc::O_RDONLY;
        let file = match st.staged.get(&k) {
            Some(s) => {
                let f = OpenOptions::new().read(true).write(write).open(&s.path);
                Some(tri!(f.map_err(|_| Errno::EIO), reply))
            }
            None => {
                if write {
                    return reply.error(Errno::EOPNOTSUPP); // committed files are read-only (spike)
                }
                // Bulk read of a directory (a copy, not browsing): read the rest of its files
                // in physical order, in a few large reads.
                if let Some((_, parent)) = k.split_last() {
                    let parent = parent.to_vec();
                    let n = st.dir_reads.entry(parent.clone()).or_insert(0);
                    *n += 1;
                    if *n == RA_TRIGGER
                        && let Ok(Some(d)) = st.dir_idx(&parent)
                    {
                        let kids: Vec<Child> = st.vol.dirs[d].children.values().cloned().collect();
                        st.readahead(&kids);
                    }
                }
                None
            }
        };
        if write {
            if flags.0 & libc::O_TRUNC != 0 {
                let st = &mut *st;
                let s = st.staged.get_mut(&k).unwrap();
                let _ = file.as_ref().unwrap().set_len(0);
                st.staged_bytes -= s.size;
                s.size = 0;
            }
            st.open_writers += 1;
        }
        st.next_fh += 1;
        let fh = st.next_fh;
        st.handles.insert(fh, Handle { ino: ino.0, file, write });
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut st = self.st.lock().unwrap();
        let k = tri!(st.key(ino.0).ok_or(Errno::ENOENT), reply);
        if let Some(s) = st.staged.get(&k) {
            let mut buf = vec![0u8; size.min(s.size.saturating_sub(offset) as u32) as usize];
            let r = match st.handles.get(&fh.0).and_then(|h| h.file.as_ref()) {
                Some(f) => f.read_exact_at(&mut buf, offset),
                None => File::open(&s.path).and_then(|f| f.read_exact_at(&mut buf, offset)),
            };
            tri!(r.map_err(|_| Errno::EIO), reply);
            return reply.data(&buf);
        }
        let len = match tri!(io(st.committed(&k)), reply) {
            Some(c) => c.len,
            None => return reply.error(Errno::ENOENT),
        };
        if offset >= len {
            return reply.data(&[]);
        }
        let size = size.min((len - offset) as u32);
        let t0 = Instant::now();
        let data = tri!(io(st.read_committed(ino.0, &k, offset, size)), reply);
        st.prof.1 += t0.elapsed().as_secs_f64();
        st.prof.2 += 1;
        reply.data(&data);
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _wf: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut st = self.st.lock().unwrap();
        let k = tri!(st.key(ino.0).ok_or(Errno::ENOENT), reply);
        if st.frozen.contains(&ino.0) || !st.staged.contains_key(&k) {
            return reply.error(Errno::EOPNOTSUPP);
        }
        let end = offset + data.len() as u64;
        let cur = st.staged[&k].size;
        if end > cur && end - cur > st.free_bytes() {
            return reply.error(Errno::ENOSPC);
        }
        // With writeback caching the kernel may write through any handle of the inode.
        let r = match st.handles.get(&fh.0).and_then(|h| h.file.as_ref()) {
            Some(f) => f.write_all_at(data, offset),
            None => OpenOptions::new().write(true).open(&st.staged[&k].path).and_then(|f| f.write_all_at(data, offset)),
        };
        tri!(r.map_err(|_| Errno::EIO), reply);
        let st = &mut *st;
        let s = st.staged.get_mut(&k).unwrap();
        if end > s.size {
            st.staged_bytes += end - s.size;
            s.size = end;
        }
        s.mtime = SystemTime::now();
        st.last_write = Instant::now();
        st.dirty = true;
        reply.written(data.len() as u32);
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        // ENOSYS makes the kernel stop sending FLUSH (one round trip per close saved); dirty
        // pages are still written back on close. Nothing is committed on close anyway.
        reply.error(Errno::ENOSYS);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut st = self.st.lock().unwrap();
        if let Some(h) = st.handles.remove(&fh.0)
            && h.write
        {
            st.open_writers -= 1;
            st.frozen.remove(&h.ino);
            st.last_write = Instant::now();
        }
        reply.ok();
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let mut st = self.st.lock().unwrap();
        let k = tri!(st.key(ino.0).ok_or(Errno::ENOENT), reply);
        if !st.staged.contains_key(&k) {
            return reply.ok(); // committed data is already durable
        }
        // Commits everything staged, including this (still open) file; later writes on its
        // open handles are refused (spike limitation, see DESIGN.md).
        tri!(io(st.commit("fsync")), reply);
        st.frozen.insert(ino.0);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let mut st = self.st.lock().unwrap();
        let k = tri!(st.key(ino.0).ok_or(Errno::ENOENT), reply);
        let mut ents: Vec<(u64, FileType, String)> =
            vec![(ino.0, FileType::Directory, ".".into()), (ino.0, FileType::Directory, "..".into())];
        let mut seen = HashSet::new();
        if let Some(d) = tri!(io(st.dir_idx(&k)), reply) {
            let kids: Vec<Child> = st.vol.dirs[d].children.values().cloned().collect();
            for c in kids {
                let mut ck = k.clone();
                let up = upcase(&c.name, &st.vol.upc);
                ck.push(up.clone());
                seen.insert(up);
                let i = st.ino_of(&ck);
                let t = if c.is_dir { FileType::Directory } else { FileType::RegularFile };
                ents.push((i, t, String::from_utf16_lossy(&c.name)));
            }
        }
        let over: Vec<(Vec<u16>, Vec<u16>)> =
            st.overlay.get(&k).map(|m| m.iter().map(|(a, b)| (a.clone(), b.clone())).collect()).unwrap_or_default();
        for (up, name) in over {
            if seen.contains(&up) {
                continue;
            }
            let mut ck = k.clone();
            ck.push(up);
            let Some(s) = st.staged.get(&ck) else { continue };
            let t = if s.is_dir { FileType::Directory } else { FileType::RegularFile };
            let i = st.ino_of(&ck);
            ents.push((i, t, String::from_utf16_lossy(&name)));
        }
        st.next_fh += 1;
        let fh = st.next_fh;
        st.dir_snaps.insert(fh, ents);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn readdir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let st = self.st.lock().unwrap();
        let Some(ents) = st.dir_snaps.get(&fh.0) else { return reply.error(Errno::EBADF) };
        for (i, (ino, t, name)) in ents.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*ino), (i + 1) as u64, *t, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.st.lock().unwrap().dir_snaps.remove(&fh.0);
        reply.ok();
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut st = self.st.lock().unwrap();
        let (k, _) = tri!(st.child_key(parent.0, name), reply);
        match st.staged.get(&k) {
            Some(s) if s.is_dir => reply.error(Errno::EISDIR),
            Some(_) => {
                let s = st.staged.remove(&k).unwrap();
                let _ = fs::remove_file(&s.path);
                st.staged_bytes -= s.size;
                if let Some(m) = st.overlay.get_mut(&k[..k.len() - 1]) {
                    m.remove(k.last().unwrap());
                }
                let i = st.inos.get(&k).copied().unwrap_or(0);
                st.clusters.remove(&i);
                reply.ok()
            }
            None => match tri!(io(st.committed(&k)), reply) {
                Some(_) => reply.error(Errno::EOPNOTSUPP),
                None => reply.error(Errno::ENOENT),
            },
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut st = self.st.lock().unwrap();
        let (k, _) = tri!(st.child_key(parent.0, name), reply);
        match st.staged.get(&k) {
            Some(s) if !s.is_dir => reply.error(Errno::ENOTDIR),
            Some(_) => {
                if st.overlay.get(&k).is_some_and(|m| !m.is_empty()) {
                    return reply.error(Errno::ENOTEMPTY);
                }
                st.staged.remove(&k);
                st.overlay.remove(&k);
                if let Some(m) = st.overlay.get_mut(&k[..k.len() - 1]) {
                    m.remove(k.last().unwrap());
                }
                reply.ok()
            }
            None => match tri!(io(st.committed(&k)), reply) {
                Some(_) => reply.error(Errno::EOPNOTSUPP),
                None => reply.error(Errno::ENOENT),
            },
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.bits() != 0 {
            return reply.error(Errno::EINVAL);
        }
        let mut st = self.st.lock().unwrap();
        let (k, _) = tri!(st.child_key(parent.0, name), reply);
        let (nk, nn) = tri!(st.child_key(newparent.0, newname), reply);
        let Some(s) = st.staged.get(&k) else {
            return match tri!(io(st.committed(&k)), reply) {
                Some(_) => reply.error(Errno::EOPNOTSUPP),
                None => reply.error(Errno::ENOENT),
            };
        };
        if s.is_dir && st.overlay.get(&k).is_some_and(|m| !m.is_empty()) {
            return reply.error(Errno::EOPNOTSUPP); // non-empty staged directories (spike)
        }
        let is_dir = s.is_dir;
        if k == nk {
            // case-only rename of a staged object
            let s = st.staged.get_mut(&k).unwrap();
            s.name = nn.clone();
            st.overlay.get_mut(&k[..k.len() - 1]).unwrap().insert(k.last().unwrap().clone(), nn);
            return reply.ok();
        }
        match tri!(io(st.obj(&nk)), reply) {
            None => {}
            Some(Obj::Staged { is_dir: false, size, .. }) if !is_dir => {
                let old = st.staged.remove(&nk).unwrap();
                let _ = fs::remove_file(&old.path);
                st.staged_bytes -= size;
                if let Some(m) = st.overlay.get_mut(&nk[..nk.len() - 1]) {
                    m.remove(nk.last().unwrap());
                }
            }
            Some(Obj::Staged { .. }) => return reply.error(Errno::EEXIST),
            Some(_) => return reply.error(Errno::EOPNOTSUPP),
        }
        let s = st.staged.remove(&k).unwrap();
        if let Some(m) = st.overlay.get_mut(&k[..k.len() - 1]) {
            m.remove(k.last().unwrap());
        }
        st.overlay.remove(&k);
        // Re-stage under the new key, keeping the same staging file and inode number.
        let size = s.size;
        let path = s.path.clone();
        tri!(st.stage(nk.clone(), nn, is_dir), reply);
        {
            let ns = st.staged.get_mut(&nk).unwrap();
            if !is_dir {
                let _ = fs::remove_file(&ns.path);
                ns.path = path;
            }
            ns.size = size;
            ns.mtime = s.mtime;
        }
        if let Some(ino) = st.inos.remove(&k) {
            if let Some(old) = st.inos.insert(nk.clone(), ino) {
                st.keys[old as usize - 1] = Vec::new(); // the replaced inode is gone
            }
            st.keys[ino as usize - 1] = nk;
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let st = self.st.lock().unwrap();
        let cs = st.vol.cs;
        let free = st.free_bytes() / cs;
        reply.statfs(st.vol.cc, free, free, 0, 0, cs as u32, 255, cs as u32);
    }
}

/// Mounts `target` (block device or image file holding exFAT) at `mountpoint` and serves it
/// until unmounted (`fusermount3 -u`). Commits staged writes on idle, fsync and unmount.
pub fn mount(target: &Path, mountpoint: &Path, o: &MountOpts) -> Result<()> {
    let vol = open_vol(target)?;
    let pid = std::process::id();
    let ram_base = PathBuf::from("/dev/shm");
    let ram_dir = ram_base.join(format!("flashcp-stage-{pid}"));
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let disk_dir = cache.join("flashcp").join(format!("stage-{pid}"));
    fs::create_dir_all(&ram_dir)?;
    fs::create_dir_all(&disk_dir)?;
    let st = State {
        target: target.to_path_buf(),
        vol_free: vol.free_clusters(),
        vol,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        keys: vec![Key::new()],
        inos: HashMap::from([(Key::new(), 1)]),
        staged: HashMap::new(),
        overlay: HashMap::new(),
        staged_bytes: 0,
        clusters: HashMap::new(),
        handles: HashMap::new(),
        dir_snaps: HashMap::new(),
        next_fh: 0,
        next_stage: 0,
        open_writers: 0,
        last_write: Instant::now(),
        dirty: false,
        commit_failed: false,
        frozen: HashSet::new(),
        ram_dir: ram_dir.clone(),
        disk_dir: disk_dir.clone(),
        o: o.clone(),
        commits: 0,
        ra: Default::default(),
        ra_bytes: 0,
        prof: (0.0, 0.0, 0, 0),
        dir_reads: HashMap::new(),
    };
    let st = Arc::new(Mutex::new(st));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (st, stop, idle) = (st.clone(), stop.clone(), o.idle);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
                let mut s = st.lock().unwrap();
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if s.dirty
                    && !s.commit_failed
                    && s.open_writers == 0
                    && s.last_write.elapsed() >= idle
                    && let Err(e) = s.commit("idle")
                {
                    eprintln!("flashcp mount: commit failed (staged data kept, retried on next write): {e:#}");
                }
            }
        });
    }
    let mut cfg = Config::default();
    cfg.n_threads = Some(o.threads.max(1));
    cfg.clone_fd = o.threads > 1;
    cfg.mount_options = vec![
        MountOption::FSName(target.display().to_string()),
        MountOption::Subtype("flashcp".into()),
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    let fs_ = DriveFs { st, stop, writeback: o.writeback };
    eprintln!("flashcp mount: serving {} at {}", target.display(), mountpoint.display());
    let r = fuser::mount(fs_, mountpoint, &cfg);
    let _ = fs::remove_dir_all(&ram_dir);
    let _ = fs::remove_dir_all(&disk_dir);
    r.context("FUSE session")
}
