//! Directory-fd primitives for writing a tree under a destination root without ever following
//! a symlink or leaving the root, whatever happens to the paths concurrently: every operation
//! is relative to a directory fd, intermediate directories are resolved with `openat2`
//! (RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS), or component by component with O_NOFOLLOW where
//! `openat2` is unavailable, and every final component is created or opened with O_NOFOLLOW.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once openat2 turned out to be missing (ENOSYS) or filtered (EPERM, seccomp).
static NO_OPENAT2: AtomicBool = AtomicBool::new(false);

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// Opens the destination root itself (must be a real directory, not a symlink).
pub(crate) fn open_root(path: &Path) -> io::Result<OwnedFd> {
    let c = cstr(path)?;
    let fd = cvt(unsafe {
        libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// An O_PATH fd of directory `rel` (relative, no `..`, no symlinks anywhere) beneath `root`.
pub(crate) fn open_dir_beneath(root: BorrowedFd<'_>, rel: &Path) -> io::Result<OwnedFd> {
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if rel.as_os_str().is_empty() {
        return root.try_clone_to_owned();
    }
    if !NO_OPENAT2.load(Ordering::Relaxed) {
        let c = cstr(rel)?;
        // SAFETY: open_how is plain data; zero is a valid value for every field.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = flags as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
        let r = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                c.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if r >= 0 {
            return Ok(unsafe { OwnedFd::from_raw_fd(r as libc::c_int) });
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOSYS) | Some(libc::EPERM) => NO_OPENAT2.store(true, Ordering::Relaxed),
            _ => return Err(e),
        }
    }
    // Fallback: one component at a time, never following a symlink.
    let mut cur = root.try_clone_to_owned()?;
    for comp in rel.components() {
        let Component::Normal(name) = comp else { return Err(io::Error::from_raw_os_error(libc::EINVAL)) };
        let c = cstr(Path::new(name))?;
        let fd = cvt(unsafe { libc::openat(cur.as_raw_fd(), c.as_ptr(), flags) })?;
        cur = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(cur)
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`; None if it does not exist.
pub(crate) fn stat_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Option<libc::stat>> {
    // SAFETY: stat is plain data, filled in by the kernel.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    match cvt(unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) }) {
        Ok(_) => Ok(Some(st)),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// Creates directory `name` in `dir`; an existing real directory is fine, anything else
/// (a file, a symlink) is an error.
pub(crate) fn mkdir_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    match cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) }) {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => match stat_at(dir, name)? {
            Some(st) if is_dir(&st) => Ok(()),
            _ => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
        },
        Err(e) => Err(e),
    }
}

/// Creates a NEW file `name` in `dir` (O_EXCL | O_NOFOLLOW: never an existing file or link).
pub(crate) fn create_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<File> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = cvt(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o666 as libc::c_uint) })?;
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Atomically replaces `to` with `from`, both in `dir`.
pub(crate) fn rename_at(dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> io::Result<()> {
    cvt(unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) }).map(drop)
}

pub(crate) fn unlink_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) }).map(drop)
}

/// Sets the mtime of `name` in `dir` without following a symlink (atime untouched).
pub(crate) fn set_mtime_at(dir: BorrowedFd<'_>, name: &CStr, secs: i64, nsec: u32) -> io::Result<()> {
    let ts = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        libc::timespec { tv_sec: secs as _, tv_nsec: nsec as _ },
    ];
    cvt(unsafe { libc::utimensat(dir.as_raw_fd(), name.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }).map(drop)
}

/// Free bytes for unprivileged users on the filesystem holding `dir`.
pub(crate) fn free_bytes(dir: BorrowedFd<'_>) -> io::Result<u64> {
    // SAFETY: statvfs is plain data, filled in by the kernel.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstatvfs(dir.as_raw_fd(), &mut s) })?;
    Ok((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

pub(crate) fn syncfs(dir: BorrowedFd<'_>) -> io::Result<()> {
    cvt(unsafe { libc::syncfs(dir.as_raw_fd()) }).map(drop)
}

/// Preallocates `len` bytes; filesystems without fallocate (EOPNOTSUPP) are fine, every other
/// failure (ENOSPC, EFBIG, EIO, …) is an error.
pub(crate) fn preallocate(f: &File, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let len = libc::off_t::try_from(len).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
    loop {
        match cvt(unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, len) }) {
            Ok(_) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
            Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}
