//! The FUSE adapter: [`Fs`] behind fuser 0.18 (pure Rust, mounted through `fusermount3`).
//!
//! Every semantic decision lives in [`Fs`]; this file only translates: inode numbers are the
//! same numbers, errors are errno values, and nothing here keeps per-file state except the
//! directory snapshots a `readdir` stream pages through.

use super::{Attr, Fs, RenameFlags as Rf, SetAttr};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
    WriteFlags,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Positive entries and attributes live this long in the kernel. Negative answers are never
/// cached (the names are case-insensitive: "foo" absent says nothing once "Foo" is created).
/// Whether open flags give write access: one predicate for create, open and release, so the
/// driver's count of write handles always balances.
fn writable(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY
}

/// FUSE requests served, by operation (the control socket's `stats`).
static OPS: std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn count(op: &'static str) {
    *OPS.lock().unwrap_or_else(|e| e.into_inner()).entry(op).or_default() += 1;
}

/// The request counts as one JSON object.
pub fn stats_json() -> String {
    let m = OPS.lock().unwrap_or_else(|e| e.into_inner());
    let total: u64 = m.values().sum();
    let parts: Vec<String> = m.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!("{{\"ok\":true,\"requests\":{total},\"ops\":{{{}}}}}", parts.join(","))
}

const TTL: Duration = Duration::from_secs(1);

type Snap = Arc<Vec<(u64, String, bool)>>;

pub struct Adapter {
    fs: Fs,
    uid: u32,
    gid: u32,
    next_dir: Mutex<(u64, HashMap<u64, Snap>)>,
}

impl Adapter {
    pub fn new(fs: Fs) -> Adapter {
        Adapter {
            fs,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            next_dir: Mutex::new((0, HashMap::new())),
        }
    }

    fn fattr(&self, a: &Attr) -> FileAttr {
        let (kind, perm, nlink) = match (a.is_dir, a.read_only) {
            (true, false) => (FileType::Directory, 0o755, 2),
            (true, true) => (FileType::Directory, 0o555, 2),
            (false, false) => (FileType::RegularFile, 0o644, 1),
            (false, true) => (FileType::RegularFile, 0o444, 1),
        };
        FileAttr {
            ino: INodeNo(a.ino),
            size: a.size,
            blocks: a.blocks,
            atime: a.atime,
            mtime: a.mtime,
            ctime: a.ctime,
            crtime: a.crtime,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: a.blksize,
            flags: 0,
        }
    }

    pub fn config(threads: usize) -> Config {
        let mut cfg = Config::default();
        cfg.n_threads = Some(threads.max(1));
        cfg.clone_fd = threads > 1;
        cfg.mount_options = vec![
            MountOption::FSName("flashcp".into()),
            MountOption::Subtype("exfat".into()),
            // no DefaultPermissions: it costs a GETATTR of the parent per created file; the
            // driver makes the same checks itself (read-only attribute, access(2)), and only the
            // mounting user can reach the mount anyway (no allow_other)
            MountOption::NoAtime,
            MountOption::NoDev,
            MountOption::NoSuid,
        ];
        cfg
    }
}

fn err(e: i32) -> Errno {
    Errno::from_i32(e)
}

macro_rules! tri {
    ($r:expr, $reply:expr) => {
        match $r {
            Ok(v) => v,
            Err(e) => {
                $reply.error(err(e));
                return;
            }
        }
    };
}

fn when(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

impl Filesystem for Adapter {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.set_max_write(1 << 20);
        let _ = config.set_max_readahead(1 << 20);
        let _ = config.set_max_background(64);
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        count("lookup");
        let a = tri!(self.fs.lookup(parent.0, name), reply);
        reply.entry(&TTL, &self.fattr(&a), Generation(0));
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        count("forget");
        self.fs.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        count("getattr");
        let a = tri!(self.fs.getattr(ino.0), reply);
        reply.attr(&TTL, &self.fattr(&a));
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        count("setattr");
        if uid.is_some_and(|u| u != self.uid) || gid.is_some_and(|g| g != self.gid) {
            return reply.error(err(libc::EPERM));
        }
        let sa =
            SetAttr { size, mtime: mtime.map(when), atime: atime.map(when), crtime, mode: mode.map(|m| m & 0o222) };
        let a = if sa.size.is_none()
            && sa.mtime.is_none()
            && sa.atime.is_none()
            && sa.crtime.is_none()
            && sa.mode.is_none()
        {
            tri!(self.fs.getattr(ino.0), reply)
        } else {
            tri!(self.fs.setattr(ino.0, &sa), reply)
        };
        reply.attr(&TTL, &self.fattr(&a));
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        count("mkdir");
        let a = tri!(self.fs.mkdir(parent.0, name), reply);
        reply.entry(&TTL, &self.fattr(&a), Generation(0));
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        count("create");
        let excl = flags & libc::O_EXCL != 0;
        let trunc = flags & libc::O_TRUNC != 0;
        let a = tri!(self.fs.create_handle(parent.0, name, excl, trunc, writable(flags)), reply);
        reply.created(&TTL, &self.fattr(&a), Generation(0), FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        count("open");
        let write = writable(flags.0);
        let trunc = flags.0 & libc::O_TRUNC != 0;
        tri!(self.fs.open(ino.0, write, trunc), reply);
        // flashcp is the only writer of the volume: cached pages stay valid across opens.
        reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        count("read");
        let d = tri!(self.fs.read(ino.0, offset, size), reply);
        reply.data(&d);
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _wf: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        count("write");
        let n = tri!(self.fs.write(ino.0, offset, data), reply);
        reply.written(n);
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        count("flush");
        // ENOSYS makes the kernel stop sending FLUSH (a round trip per close saved). Nothing is
        // committed on close anyway: durability is fsync, idle, or the control socket.
        reply.error(err(libc::ENOSYS));
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        count("release");
        self.fs.release_handle(ino.0, writable(flags.0));
        reply.ok();
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        count("fsync");
        tri!(self.fs.fsync(ino.0), reply);
        reply.ok();
    }

    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        count("fsyncdir");
        tri!(self.fs.flush().map_err(|_| libc::EIO), reply);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        count("opendir");
        let v = tri!(self.fs.readdir(ino.0), reply);
        let mut g = self.next_dir.lock().unwrap();
        g.0 += 1;
        let fh = g.0;
        g.1.insert(fh, Arc::new(v));
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        count("readdir");
        let Some(snap) = self.next_dir.lock().unwrap().1.get(&fh.0).cloned() else {
            return reply.error(err(libc::EBADF));
        };
        // offsets: 1 = ".", 2 = "..", then 3 + index
        let mut i = offset;
        while i < 2 + snap.len() as u64 {
            let (ino, kind, name) = match i {
                0 => (ino.0, FileType::Directory, "."),
                1 => (ino.0, FileType::Directory, ".."),
                _ => {
                    let (c, n, d) = &snap[(i - 2) as usize];
                    (*c, if *d { FileType::Directory } else { FileType::RegularFile }, n.as_str())
                }
            };
            if reply.add(INodeNo(ino), i + 1, kind, name) {
                break;
            }
            i += 1;
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        count("readdirplus");
        let Some(snap) = self.next_dir.lock().unwrap().1.get(&fh.0).cloned() else {
            return reply.error(err(libc::EBADF));
        };
        let mut i = offset;
        while i < 2 + snap.len() as u64 {
            if i < 2 {
                let a = tri!(self.fs.getattr(ino.0), reply);
                let fa = self.fattr(&a);
                // "." and ".." are not looked up by the kernel
                if reply.add(INodeNo(ino.0), i + 1, if i == 0 { "." } else { ".." }, &TTL, &fa, Generation(0)) {
                    break;
                }
            } else {
                let (_, name, _) = &snap[(i - 2) as usize];
                // The entry may be gone since the snapshot. Each one added counts as a lookup
                // for the kernel (which sends the matching forget).
                if let Ok(a) = self.fs.lookup(ino.0, OsStr::new(name)) {
                    let fa = self.fattr(&a);
                    if reply.add(fa.ino, i + 1, name, &TTL, &fa, Generation(0)) {
                        self.fs.forget(a.ino, 1);
                        break;
                    }
                }
            }
            i += 1;
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        count("releasedir");
        self.next_dir.lock().unwrap().1.remove(&fh.0);
        reply.ok();
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        count("unlink");
        tri!(self.fs.unlink(parent.0, name), reply);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        count("rmdir");
        tri!(self.fs.rmdir(parent.0, name), reply);
        reply.ok();
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
        count("rename");
        if flags.contains(RenameFlags::RENAME_WHITEOUT) {
            return reply.error(err(libc::EINVAL));
        }
        let fl = Rf {
            noreplace: flags.contains(RenameFlags::RENAME_NOREPLACE),
            exchange: flags.contains(RenameFlags::RENAME_EXCHANGE),
        };
        tri!(self.fs.rename(parent.0, name, newparent.0, newname, fl), reply);
        reply.ok();
    }

    fn access(&self, _req: &Request, ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        count("access");
        // what default_permissions checked against the modes the driver reports (0644/0755,
        // 0444/0555 with the read-only attribute): write needs the attribute clear, execute
        // a directory (regular files are never executable)
        let a = tri!(self.fs.getattr(ino.0), reply);
        if mask.contains(fuser::AccessFlags::W_OK) && a.read_only
            || mask.contains(fuser::AccessFlags::X_OK) && !a.is_dir
        {
            return reply.error(err(libc::EACCES));
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        count("statfs");
        let s = self.fs.statfs();
        reply.statfs(s.blocks, s.bfree, s.bfree, s.files, s.files, s.bsize, s.namelen, s.bsize);
    }

    fn symlink(&self, _req: &Request, _p: INodeNo, _n: &OsStr, _t: &std::path::Path, reply: ReplyEntry) {
        reply.error(err(libc::EPERM));
    }

    fn link(&self, _req: &Request, _ino: INodeNo, _p: INodeNo, _n: &OsStr, reply: ReplyEntry) {
        reply.error(err(libc::EPERM));
    }

    fn mknod(&self, _req: &Request, _p: INodeNo, _n: &OsStr, _mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        reply.error(err(libc::EPERM));
    }

    // xattrs: ENOSYS (not ENOTSUP) makes the kernel stop asking. It would otherwise send a
    // getxattr("security.capability") before every write; callers still get EOPNOTSUPP.
    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        count("setxattr");
        reply.error(err(libc::ENOSYS));
    }

    fn getxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _size: u32, reply: ReplyXattr) {
        count("getxattr");
        reply.error(err(libc::ENOSYS));
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        count("listxattr");
        reply.error(err(libc::ENOSYS));
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        count("removexattr");
        reply.error(err(libc::ENOSYS));
    }
}
