//! Directory-fd primitives for writing a tree under a destination root without ever following
//! a symlink or leaving the root, whatever happens to the paths concurrently: every operation
//! is relative to a directory fd, intermediate directories are resolved with `openat2`
//! (RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS), or component by component with O_NOFOLLOW where
//! `openat2` is unavailable, and every final component is created or opened with O_NOFOLLOW.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
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

pub(crate) fn is_regular(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

/// The names in directory `dir` (never `.` or `..`). Read through a fresh descriptor opened
/// from `dir` with `openat(".")`, so `dir` may be an O_PATH fd, and no symlink is followed.
pub(crate) fn list_at(dir: BorrowedFd<'_>) -> io::Result<Vec<CString>> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = cvt(unsafe { libc::openat(dir.as_raw_fd(), c".".as_ptr(), flags) })?;
    let dp = unsafe { libc::fdopendir(fd) };
    if dp.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut names = Vec::new();
    let res = loop {
        // SAFETY: errno is thread-local; clearing it tells end-of-directory from an error.
        unsafe { *libc::__errno_location() = 0 };
        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            let e = unsafe { *libc::__errno_location() };
            break if e == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(e)) };
        }
        // SAFETY: readdir returned a valid entry, whose d_name is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    };
    unsafe { libc::closedir(dp) };
    res.map(|()| names)
}

/// Opens directory `name` in `dir` without following a symlink.
fn open_dir_nofollow(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = cvt(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) })?;
    // SAFETY: openat returned a fresh descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// An entry's identity and type: (st_dev, st_ino, S_IFMT bits of st_mode).
pub(crate) type Key = (libc::dev_t, libc::ino_t, libc::mode_t);

fn key_of(st: &libc::stat) -> Key {
    (st.st_dev, st.st_ino, st.st_mode & libc::S_IFMT)
}

/// [`key_of`] for an open descriptor.
fn key_of_fd(fd: BorrowedFd<'_>) -> io::Result<Key> {
    // SAFETY: stat is plain data, filled in by the kernel.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok(key_of(&st))
}

/// [`key_of`] of the entry `name` in `dir` now (None: there is no such entry).
fn key_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Option<Key>> {
    Ok(stat_at(dir, name)?.as_ref().map(key_of))
}

/// An entry as a deletion plan saw it: its name, its [`Key`], and for a directory the entries it
/// held when planned (recursively). Symlinks are entries like any other and are never followed.
#[derive(Debug)]
pub(crate) struct Planned {
    pub name: CString,
    pub key: Key,
    pub children: Vec<Planned>,
}

impl Planned {
    pub(crate) fn is_dir(&self) -> bool {
        self.key.2 == libc::S_IFDIR
    }

    /// The entries this node stands for: itself and everything planned below it.
    pub(crate) fn count(&self) -> u64 {
        1 + self.children.iter().map(Planned::count).sum::<u64>()
    }
}

/// Plans the removal of `name` in `dir`: the entry and, for a directory, everything inside it as
/// it is now. None if there is no such entry. Nothing is removed.
pub(crate) fn plan_tree_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Option<Planned>> {
    let Some(st) = stat_at(dir, name)? else { return Ok(None) };
    let mut children = Vec::new();
    if is_dir(&st) {
        let sub = open_dir_nofollow(dir, name)?;
        for child in list_at(sub.as_fd())? {
            if let Some(c) = plan_tree_at(sub.as_fd(), &child)? {
                children.push(c);
            }
        }
    }
    Ok(Some(Planned { name: name.to_owned(), key: key_of(&st), children }))
}

/// Outcome of checking a planned entry against what is at its name now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    /// Nothing is there: it was removed since planning (not an error).
    Gone,
    /// The planned entry itself.
    Same,
    /// Something else is there (another inode or another type): not ours to remove.
    Changed,
}

/// Compares the planned identity and type of an entry with `now` (what is at its name, if
/// anything). Pure: the syscalls are in the callers.
pub(crate) fn presence(planned: Key, now: Option<Key>) -> Presence {
    match now {
        None => Presence::Gone,
        Some(k) if k == planned => Presence::Same,
        Some(_) => Presence::Changed,
    }
}

/// The names in `present` that the plan does not have, in their order. Pure.
pub(crate) fn unplanned<'a>(planned: &[CString], present: &'a [CString]) -> Vec<&'a CString> {
    let have: HashSet<&[u8]> = planned.iter().map(|c| c.as_bytes()).collect();
    present.iter().filter(|n| !have.contains(n.as_bytes())).collect()
}

/// What [`remove_planned_at`] did. Paths are relative to the root of the removal.
#[derive(Debug, Default)]
pub(crate) struct Removal {
    /// Entries removed.
    pub removed: u64,
    /// Planned entries left in place, and why: changed since planning, or a directory that still
    /// holds entries.
    pub kept: Vec<(PathBuf, &'static str)>,
    /// Entries that the plan did not have (they appeared after planning), left in place. Each
    /// directory holding one is kept too.
    pub unplanned: Vec<PathBuf>,
}

const CHANGED: &str = "changed since planning";
const NOT_EMPTY: &str = "still holds entries";

/// Records what is left in the planned directory `sub`: the entries the plan did not have, and
/// the directory itself as kept. Returns false when nothing is left.
fn note_left(sub: BorrowedFd<'_>, planned: &[CString], rel: &Path, out: &mut Removal) -> io::Result<bool> {
    let left = list_at(sub)?;
    if left.is_empty() {
        return Ok(false);
    }
    for n in unplanned(planned, &left) {
        out.unplanned.push(rel.join(std::ffi::OsStr::from_bytes(n.as_bytes())));
    }
    out.kept.push((rel.to_path_buf(), NOT_EMPTY));
    Ok(true)
}

/// Removes the planned entry `p` from `dir`, and only what was planned.
///
/// Each entry is checked against its planned identity and type right before it is removed: a
/// file is unlinked; a directory is opened and checked, emptied through its own fd, checked
/// again, then removed. A planned entry that is gone is not an error. One that is now something
/// else is kept, with everything in it. A directory that holds an entry the plan did not have is
/// kept with its planned parents, the unplanned entry is reported, and the other planned entries
/// are still removed. Any other failure is an error.
pub(crate) fn remove_planned_at(dir: BorrowedFd<'_>, p: &Planned, rel: &Path, out: &mut Removal) -> io::Result<()> {
    let name = p.name.as_c_str();
    let keep = |out: &mut Removal, why: &'static str| -> io::Result<()> {
        out.kept.push((rel.to_path_buf(), why));
        Ok(())
    };
    match presence(p.key, key_at(dir, name)?) {
        Presence::Gone => return Ok(()),
        Presence::Changed => return keep(out, CHANGED),
        Presence::Same => {}
    }
    if !p.is_dir() {
        match unlink_at(dir, name) {
            Ok(()) => out.removed += 1,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
            Err(e) => return Err(e),
        }
        return Ok(());
    }
    let sub = match open_dir_nofollow(dir, name) {
        Ok(fd) => fd,
        Err(e) => {
            return match e.raw_os_error() {
                Some(libc::ENOENT) => Ok(()),
                // Replaced by a symlink or a file since the check above.
                Some(libc::ELOOP | libc::ENOTDIR) => keep(out, CHANGED),
                _ => Err(e),
            };
        }
    };
    // The directory opened must be the planned one, or its planned names mean nothing.
    if presence(p.key, Some(key_of_fd(sub.as_fd())?)) != Presence::Same {
        return keep(out, CHANGED);
    }
    for child in &p.children {
        remove_planned_at(sub.as_fd(), child, &rel.join(std::ffi::OsStr::from_bytes(child.name.as_bytes())), out)?;
    }
    let planned: Vec<CString> = p.children.iter().map(|c| c.name.clone()).collect();
    if note_left(sub.as_fd(), &planned, rel, out)? {
        return Ok(());
    }
    // Empty: remove it, if it is still the planned directory.
    match presence(p.key, key_at(dir, name)?) {
        Presence::Gone => return Ok(()),
        Presence::Changed => return keep(out, CHANGED),
        Presence::Same => {}
    }
    match cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }) {
        Ok(_) => out.removed += 1,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
        // Something arrived after the listing above.
        Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => {
            if !note_left(sub.as_fd(), &planned, rel, out)? {
                keep(out, CHANGED)?;
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// The identity of a directory: (st_dev, st_ino). Two paths name the same directory iff equal.
pub(crate) type Ident = (libc::dev_t, libc::ino_t);

/// The identity of an open directory.
pub(crate) fn ident_of(fd: BorrowedFd<'_>) -> io::Result<Ident> {
    key_of_fd(fd).map(|(dev, ino, _)| (dev, ino))
}

/// Opens `path` (following symlinks) as a directory descriptor for identity checks only (O_PATH:
/// no read permission needed). None if it does not exist.
pub(crate) fn open_path_dir(path: &Path) -> io::Result<Option<OwnedFd>> {
    let c = cstr(path)?;
    match cvt(unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) }) {
        // SAFETY: open returned a fresh descriptor that nothing else owns.
        Ok(fd) => Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) })),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The identities of directory `dir` and of its ancestors, walking `..` (which crosses mount
/// points the way the kernel does): `dir` first, the filesystem root last. The walk stops where
/// `..` is the directory itself, which is how the root is recognized.
pub(crate) fn ancestor_idents(dir: BorrowedFd<'_>) -> io::Result<Vec<Ident>> {
    // Far deeper than any real path; a loop that never reaches the root fails here.
    const MAX_DEPTH: usize = 4096;
    let mut out = vec![ident_of(dir)?];
    let mut cur: Option<OwnedFd> = None;
    for _ in 0..MAX_DEPTH {
        let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let from = cur.as_ref().map_or(dir, |c| c.as_fd());
        let fd = cvt(unsafe { libc::openat(from.as_raw_fd(), c"..".as_ptr(), flags) })?;
        // SAFETY: openat returned a fresh descriptor that nothing else owns.
        let parent = unsafe { OwnedFd::from_raw_fd(fd) };
        let id = ident_of(parent.as_fd())?;
        if Some(&id) == out.last() {
            return Ok(out);
        }
        out.push(id);
        cur = Some(parent);
    }
    Err(io::Error::from_raw_os_error(libc::ELOOP))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("flashcp-dirfd-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn planned_removal_never_follows_symlinks() {
        let base = scratch("unit");
        let outside = base.join("outside");
        fs::create_dir_all(outside.join("keep")).unwrap();
        fs::write(outside.join("keep/f.txt"), b"keep").unwrap();
        let root = base.join("root");
        fs::create_dir_all(root.join("tree/inner")).unwrap();
        fs::write(root.join("tree/inner/a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("tree/link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out-link")).unwrap();
        let rfd = open_root(&root).unwrap();
        assert_eq!(list_at(rfd.as_fd()).unwrap().len(), 2);
        // tree, inner, inner/a.txt and the link itself: the symlink is unlinked, never entered.
        let tree = plan_tree_at(rfd.as_fd(), c"tree").unwrap().expect("tree is planned");
        assert_eq!(tree.count(), 4);
        let mut out = Removal::default();
        remove_planned_at(rfd.as_fd(), &tree, Path::new("tree"), &mut out).unwrap();
        assert_eq!(out.removed, 4);
        assert!(out.kept.is_empty() && out.unplanned.is_empty());
        let link = plan_tree_at(rfd.as_fd(), c"out-link").unwrap().expect("link is planned");
        assert_eq!(link.count(), 1);
        remove_planned_at(rfd.as_fd(), &link, Path::new("out-link"), &mut out).unwrap();
        assert_eq!(out.removed, 5);
        assert!(plan_tree_at(rfd.as_fd(), c"missing").unwrap().is_none());
        assert!(fs::symlink_metadata(root.join("tree")).is_err());
        assert!(fs::symlink_metadata(root.join("out-link")).is_err());
        assert_eq!(fs::read(outside.join("keep/f.txt")).unwrap(), b"keep", "outside target was touched");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn planned_removal_keeps_what_changed_and_reports_what_is_unplanned() {
        let base = scratch("changed");
        let root = base.join("root");
        fs::create_dir_all(root.join("tree/sub")).unwrap();
        fs::write(root.join("tree/keep.txt"), b"k").unwrap();
        fs::write(root.join("tree/swap.txt"), b"old").unwrap();
        fs::write(root.join("tree/sub/a.txt"), b"a").unwrap();
        let rfd = open_root(&root).unwrap();
        let tree = plan_tree_at(rfd.as_fd(), c"tree").unwrap().unwrap();
        assert_eq!(tree.count(), 5);

        // A new file in the planned directory, and a replacement of a planned file: the new
        // file is made while the old one still exists, so the replacement has a new inode.
        fs::write(root.join("tree/new.txt"), b"new").unwrap();
        fs::write(root.join("tree/swap.tmp"), b"replacement").unwrap();
        fs::rename(root.join("tree/swap.tmp"), root.join("tree/swap.txt")).unwrap();

        let mut out = Removal::default();
        remove_planned_at(rfd.as_fd(), &tree, Path::new("tree"), &mut out).unwrap();
        assert_eq!(out.removed, 3, "keep.txt, sub/a.txt and sub are removed");
        let mut kept: Vec<_> = out.kept.iter().map(|(p, why)| (p.display().to_string(), *why)).collect();
        kept.sort();
        assert_eq!(kept, vec![("tree".to_string(), NOT_EMPTY), ("tree/swap.txt".to_string(), CHANGED)], "kept entries");
        assert_eq!(out.unplanned, vec![PathBuf::from("tree/new.txt")]);
        assert_eq!(fs::read(root.join("tree/swap.txt")).unwrap(), b"replacement");
        assert_eq!(fs::read(root.join("tree/new.txt")).unwrap(), b"new");
        assert!(fs::symlink_metadata(root.join("tree/keep.txt")).is_err());
        assert!(fs::symlink_metadata(root.join("tree/sub")).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn planned_entry_that_vanished_is_not_an_error() {
        let base = scratch("gone");
        fs::write(base.join("f.txt"), b"x").unwrap();
        let rfd = open_root(&base).unwrap();
        let f = plan_tree_at(rfd.as_fd(), c"f.txt").unwrap().unwrap();
        fs::remove_file(base.join("f.txt")).unwrap();
        let mut out = Removal::default();
        remove_planned_at(rfd.as_fd(), &f, Path::new("f.txt"), &mut out).unwrap();
        assert_eq!(out.removed, 0);
        assert!(out.kept.is_empty() && out.unplanned.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn presence_compares_identity_and_type_only() {
        let planned: Key = (7, 42, libc::S_IFREG);
        assert_eq!(presence(planned, None), Presence::Gone);
        assert_eq!(presence(planned, Some((7, 42, libc::S_IFREG))), Presence::Same);
        // Another inode (a replacement), the same inode on another device, or another type.
        assert_eq!(presence(planned, Some((7, 43, libc::S_IFREG))), Presence::Changed);
        assert_eq!(presence(planned, Some((8, 42, libc::S_IFREG))), Presence::Changed);
        assert_eq!(presence(planned, Some((7, 42, libc::S_IFLNK))), Presence::Changed);
        let dir: Key = (7, 42, libc::S_IFDIR);
        assert_eq!(presence(dir, Some((7, 42, libc::S_IFREG))), Presence::Changed);
    }

    #[test]
    fn unplanned_is_present_minus_planned_in_order() {
        let c = |s: &str| CString::new(s).unwrap();
        let planned = vec![c("a"), c("b")];
        let present = vec![c("b"), c("new"), c("a"), c("zz")];
        let got: Vec<&[u8]> = unplanned(&planned, &present).into_iter().map(|n| n.as_bytes()).collect();
        assert_eq!(got, vec![&b"new"[..], &b"zz"[..]]);
        assert!(unplanned(&planned, &planned).is_empty());
        assert!(unplanned(&[], &[]).is_empty());
    }

    #[test]
    fn ancestor_idents_end_at_the_filesystem_root() {
        let base = scratch("ancestors");
        let dir = base.join("a/b");
        fs::create_dir_all(&dir).unwrap();
        let fd = open_root(&dir).unwrap();
        let chain = ancestor_idents(fd.as_fd()).unwrap();
        assert_eq!(chain[0], ident_of(fd.as_fd()).unwrap());
        let root = open_path_dir(Path::new("/")).unwrap().expect("/ exists");
        assert_eq!(*chain.last().unwrap(), ident_of(root.as_fd()).unwrap(), "the walk must end at /");
        let parent = open_path_dir(&base.join("a")).unwrap().expect("parent exists");
        assert_eq!(chain[1], ident_of(parent.as_fd()).unwrap(), "the second entry is the parent");
        assert!(open_path_dir(&base.join("missing")).unwrap().is_none());
        let _ = fs::remove_dir_all(&base);
    }
}
