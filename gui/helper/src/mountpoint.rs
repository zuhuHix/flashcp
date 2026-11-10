//! `flashcp-mountpoint` (pkexec): create or remove an empty mountpoint directory
//! `/media/<user>/<NAME>` owned by the invoking user, so a FUSE mount of a USB drive shows up
//! where file managers expect drives. It does nothing else.
//!
//! Every filesystem step is relative to a directory fd that was opened with `O_NOFOLLOW`
//! (`/media` -> `/media/<user>` -> entry), and ownership is changed through the entry's own fd,
//! never by path. A symlink swapped in while the program runs therefore cannot redirect it.
//! See packaging/desktop/SECURITY-NOTES.md.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use crate::exec::invoking_uid;
use crate::json;

const PROG: &str = "flashcp-mountpoint";
/// Highest numeric suffix tried after the plain name (udisks: `NAME`, `NAME1`, ...).
const MAX_SUFFIX: u32 = 99;

/// Why a request failed; decides the exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Fail {
    /// Refused at runtime (exit 1).
    Refused(String),
    /// Bad invocation or caller (exit 3).
    Usage(String),
}
use Fail::{Refused, Usage};

/// Where mountpoints live and how strict to be. Production is [`Env::system`]; tests use a
/// tempdir with `strict: false`, which skips only the "base is owned by root" check and the
/// root-only `fchown` of a freshly created `/media/<user>`.
pub struct Env {
    pub media: PathBuf,
    pub strict: bool,
}

impl Env {
    pub fn system() -> Env {
        Env { media: "/media".into(), strict: true }
    }
}

/// One path component the way udisks would use it as a mountpoint name.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 255 {
        return Err("name must be 1 to 255 bytes".into());
    }
    if name == "." || name == ".." || name.starts_with('.') {
        return Err("name must not start with a dot".into());
    }
    if name.contains('/') {
        return Err("name must not contain '/'".into());
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters".into());
    }
    Ok(())
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("validated: no NUL")
}

fn errno_is(e: &io::Error, codes: &[i32]) -> bool {
    e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Open the directory `name` relative to `dir`; a symlink or non-directory fails.
fn open_dir(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: name is NUL-terminated; on success the fd is new and owned by us.
    let fd = cvt(unsafe { libc::openat(dir, name.as_ptr(), flags) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: st is a valid out pointer.
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

fn mkdirat(dir: RawFd, name: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: name is NUL-terminated.
    cvt(unsafe { libc::mkdirat(dir, name.as_ptr(), mode) }).map(drop)
}

fn fchown_fchmod(fd: RawFd, uid: libc::uid_t, gid: libc::gid_t, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: plain syscalls on a valid fd.
    cvt(unsafe { libc::fchown(fd, uid, gid) })?;
    cvt(unsafe { libc::fchmod(fd, mode) }).map(drop)
}

/// True when the directory behind `fd` has no entries besides `.` and `..`.
fn is_empty(fd: RawFd) -> io::Result<bool> {
    // SAFETY: dup'd fd is handed to fdopendir, which owns it (closedir closes it).
    let dup = cvt(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) })?;
    let d = unsafe { libc::fdopendir(dup) };
    if d.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(e);
    }
    unsafe { libc::rewinddir(d) };
    let mut empty = true;
    let mut err = None;
    loop {
        // SAFETY: errno is thread-local; d is a valid DIR*.
        unsafe { *libc::__errno_location() = 0 };
        let ent = unsafe { libc::readdir(d) };
        if ent.is_null() {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(0) {
                err = Some(e);
            }
            break;
        }
        let n = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if n != b"." && n != b".." {
            empty = false;
            break;
        }
    }
    unsafe { libc::closedir(d) };
    err.map_or(Ok(empty), Err)
}

fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// Open `/media` (no symlink), then `/media/<user>` under it. `make`: create the user dir when
/// missing (0750, root:<gid>). Returns the user dir fd and its st_dev, or None if missing.
fn open_base(env: &Env, user: &str, gid: libc::gid_t, make: bool) -> Result<Option<(OwnedFd, libc::dev_t)>, Fail> {
    let path = CString::new(env.media.as_os_str().as_encoded_bytes()).map_err(|_| Usage("bad media path".into()))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: path is NUL-terminated.
    let raw = cvt(unsafe { libc::open(path.as_ptr(), flags) })
        .map_err(|e| Refused(format!("cannot open {}: {e}", env.media.display())))?;
    let media = unsafe { OwnedFd::from_raw_fd(raw) };
    let shown = env.media.join(user);
    let name = cstr(user);
    let mut created = false;
    let base = match open_dir(media.as_raw_fd(), &name) {
        Ok(fd) => fd,
        Err(e) if errno_is(&e, &[libc::ENOENT]) => {
            if !make {
                return Ok(None);
            }
            match mkdirat(media.as_raw_fd(), &name, 0o750) {
                Ok(()) => created = true,
                Err(e) if errno_is(&e, &[libc::EEXIST]) => {}
                Err(e) => return Err(Refused(format!("cannot create {}: {e}", shown.display()))),
            }
            open_dir(media.as_raw_fd(), &name).map_err(|e| Refused(format!("cannot open {}: {e}", shown.display())))?
        }
        Err(e) if errno_is(&e, &[libc::ELOOP, libc::ENOTDIR]) => {
            return Err(Refused(format!("{} is not a plain directory", shown.display())))
        }
        Err(e) => return Err(Refused(format!("cannot open {}: {e}", shown.display()))),
    };
    if created {
        // Group ownership is our stand-in for udisks' ACL; strict (root) only.
        let owner = if env.strict { 0 } else { libc::uid_t::MAX };
        let gid = if env.strict { gid } else { libc::gid_t::MAX };
        fchown_fchmod(base.as_raw_fd(), owner, gid, 0o750)
            .map_err(|e| Refused(format!("cannot set up {}: {e}", shown.display())))?;
    }
    let st = fstat(base.as_raw_fd()).map_err(|e| Refused(format!("{}: {e}", shown.display())))?;
    if !is_dir(&st) || (env.strict && st.st_uid != 0) || st.st_mode & 0o022 != 0 {
        return Err(Refused(format!("{} is not owned by root or is writable by others", shown.display())));
    }
    Ok(Some((base, st.st_dev)))
}

/// Create (or reuse) an empty directory for `name` under `/media/<user>`; returns its path.
pub fn create(env: &Env, user: &str, uid: libc::uid_t, gid: libc::gid_t, name: &str) -> Result<PathBuf, Fail> {
    validate_name(name).map_err(Usage)?;
    validate_name(user).map_err(|e| Usage(format!("user name: {e}")))?;
    let (base, base_dev) = open_base(env, user, gid, true)?.ok_or_else(|| Refused("no base directory".into()))?;
    let dir = base.as_raw_fd();
    for n in 0..=MAX_SUFFIX {
        let cand = if n == 0 { name.to_string() } else { format!("{name}{n}") };
        if cand.len() > 255 {
            continue;
        }
        let c = cstr(&cand);
        let path = env.media.join(user).join(&cand);
        match open_dir(dir, &c) {
            Ok(fd) => {
                // Reuse only an empty, unmounted directory that is already the user's.
                let st = fstat(fd.as_raw_fd()).map_err(|e| Refused(format!("{}: {e}", path.display())))?;
                let usable = is_dir(&st) && st.st_uid == uid && st.st_dev == base_dev;
                if usable && is_empty(fd.as_raw_fd()).map_err(|e| Refused(format!("{}: {e}", path.display())))? {
                    return Ok(path);
                }
            }
            Err(e) if errno_is(&e, &[libc::ENOENT]) => {
                mkdirat(dir, &c, 0o700).map_err(|e| Refused(format!("cannot create {}: {e}", path.display())))?;
                let fd = open_dir(dir, &c).map_err(|e| Refused(format!("cannot open {}: {e}", path.display())))?;
                fchown_fchmod(fd.as_raw_fd(), uid, gid, 0o700)
                    .map_err(|e| Refused(format!("cannot set up {}: {e}", path.display())))?;
                return Ok(path);
            }
            // Symlink, file, other: not ours to use, try the next name.
            Err(e) if errno_is(&e, &[libc::ELOOP, libc::ENOTDIR]) => {}
            Err(e) => return Err(Refused(format!("cannot open {}: {e}", path.display()))),
        }
    }
    Err(Refused(format!("no free mountpoint name for {name:?} (tried {} candidates)", MAX_SUFFIX + 1)))
}

/// Remove `/media/<user>/<name>` if it is the user's empty, unmounted directory.
pub fn remove(env: &Env, user: &str, uid: libc::uid_t, gid: libc::gid_t, name: &str) -> Result<(), Fail> {
    validate_name(name).map_err(Usage)?;
    validate_name(user).map_err(|e| Usage(format!("user name: {e}")))?;
    let Some((base, base_dev)) = open_base(env, user, gid, false)? else { return Ok(()) };
    let c = cstr(name);
    let path = env.media.join(user).join(name);
    let fd = match open_dir(base.as_raw_fd(), &c) {
        Ok(fd) => fd,
        Err(e) if errno_is(&e, &[libc::ENOENT]) => return Ok(()),
        Err(e) if errno_is(&e, &[libc::ELOOP, libc::ENOTDIR]) => {
            return Err(Refused(format!("{} is not a plain directory", path.display())))
        }
        Err(e) => return Err(Refused(format!("cannot open {}: {e}", path.display()))),
    };
    let st = fstat(fd.as_raw_fd()).map_err(|e| Refused(format!("{}: {e}", path.display())))?;
    if st.st_uid != uid {
        return Err(Refused(format!("{} belongs to another user", path.display())));
    }
    if st.st_dev != base_dev {
        return Err(Refused(format!("{} is a mountpoint in use", path.display())));
    }
    if !is_empty(fd.as_raw_fd()).map_err(|e| Refused(format!("{}: {e}", path.display())))? {
        return Err(Refused(format!("{} is not empty", path.display())));
    }
    drop(fd);
    // SAFETY: c is NUL-terminated; rmdir semantics also refuse a non-empty dir or a mountpoint.
    cvt(unsafe { libc::unlinkat(base.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) })
        .map(drop)
        .map_err(|e| Refused(format!("cannot remove {}: {e}", path.display())))
}

/// (user name, primary gid) of `uid`.
fn passwd(uid: libc::uid_t) -> Result<(String, libc::gid_t), String> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    loop {
        // SAFETY: all pointers are valid for the given lengths.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut result) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(format!("getpwuid_r: {}", io::Error::from_raw_os_error(rc)));
        }
        break;
    }
    if result.is_null() {
        return Err(format!("no passwd entry for uid {uid}"));
    }
    // SAFETY: pw_name points into buf as a NUL-terminated string.
    let name = unsafe { CStr::from_ptr(pw.pw_name) }.to_str().map_err(|_| "user name is not UTF-8".to_string())?;
    Ok((name.to_string(), pw.pw_gid))
}

fn run(argv: &[std::ffi::OsString]) -> Result<Option<PathBuf>, Fail> {
    let args: Vec<&str> = argv.iter().map(|a| a.to_str().unwrap_or("")).collect();
    let (verb, name) = match args.as_slice() {
        ["create", n] => (true, *n),
        ["remove", n] => (false, *n),
        _ => return Err(Usage("usage: flashcp-mountpoint create|remove <NAME>".into())),
    };
    validate_name(name).map_err(Usage)?;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Usage("must be started via pkexec".into()));
    }
    // PKEXEC_UID only: SUDO_UID is deliberately not accepted.
    let pkexec_uid = std::env::var("PKEXEC_UID").ok();
    let uid = invoking_uid(pkexec_uid.as_deref(), None).map_err(Usage)?;
    let (user, gid) = passwd(uid).map_err(Usage)?;
    let env = Env::system();
    if verb {
        create(&env, &user, uid, gid, name).map(Some)
    } else {
        remove(&env, &user, uid, gid, name).map(|()| None)
    }
}

/// Entry point of the `flashcp-mountpoint` binary; returns 0, 1 or 3.
pub fn main() -> i32 {
    let argv: Vec<_> = std::env::args_os().skip(1).collect();
    match run(&argv) {
        Ok(path) => {
            if let Some(p) = path {
                json::print_line(&p.to_string_lossy());
            }
            0
        }
        Err(Refused(m)) => {
            json::print_line(&json::error_line(PROG, &m));
            1
        }
        Err(Usage(m)) => {
            json::print_line(&json::error_line(PROG, &m));
            3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    #[test]
    fn name_validation() {
        let long = "a".repeat(256);
        let ok255 = "a".repeat(255);
        let cases: [(&str, bool); 16] = [
            ("STICK", true),
            ("My Stick 2", true),
            ("Größe é", true),
            (&ok255, true),
            (&long, false),
            ("", false),
            (".", false),
            ("..", false),
            (".hidden", false),
            ("a/b", false),
            ("/", false),
            ("a\0b", false),
            ("a\nb", false),
            ("a\x7fb", false),
            ("a\u{85}b", false),
            ("a\tb", false),
        ];
        for (name, ok) in cases {
            assert_eq!(validate_name(name).is_ok(), ok, "{name:?}");
        }
    }

    #[test]
    fn create_and_remove() {
        let t = tempfile::tempdir().unwrap();
        let env = Env { media: t.path().to_path_buf(), strict: false };
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mk = |n: &str| create(&env, "alice", uid, gid, n);
        let rm = |n: &str| remove(&env, "alice", uid, gid, n);
        let base = t.path().join("alice");

        // Missing base: remove is a no-op; create makes it 0750.
        assert_eq!(rm("STICK"), Ok(()));
        assert!(!base.exists());
        assert_eq!(mk("STICK"), Ok(base.join("STICK")));
        assert_eq!(fs::metadata(&base).unwrap().permissions().mode() & 0o7777, 0o750);
        let m = fs::metadata(base.join("STICK")).unwrap();
        assert_eq!((m.permissions().mode() & 0o7777, m.uid()), (0o700, uid));

        // An empty one of ours is reused; a non-empty one is skipped.
        assert_eq!(mk("STICK"), Ok(base.join("STICK")));
        fs::write(base.join("STICK/file"), "x").unwrap();
        assert_eq!(mk("STICK"), Ok(base.join("STICK1")));
        fs::write(base.join("STICK1/file"), "x").unwrap();
        // A symlink is skipped and its target left alone.
        let target = t.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        symlink(&target, base.join("STICK2")).unwrap();
        assert_eq!(mk("STICK"), Ok(base.join("STICK3")));
        fs::remove_dir(base.join("STICK3")).unwrap();
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

        // Remove: empty ok, non-empty and symlink refused, missing ok.
        fs::remove_file(base.join("STICK1/file")).unwrap();
        assert_eq!(rm("STICK1"), Ok(()));
        assert!(!base.join("STICK1").exists());
        assert!(matches!(rm("STICK"), Err(Refused(m)) if m.contains("not empty")));
        assert!(base.join("STICK/file").exists());
        assert!(matches!(rm("STICK2"), Err(Refused(_))));
        assert!(base.join("STICK2").symlink_metadata().is_ok());
        assert_eq!(rm("NOPE"), Ok(()));

        // All 100 names taken: error.
        for n in 0..=MAX_SUFFIX {
            let d = base.join(if n == 0 { "FULL".to_string() } else { format!("FULL{n}") });
            fs::create_dir(&d).unwrap();
            fs::write(d.join("f"), "x").unwrap();
        }
        assert!(matches!(mk("FULL"), Err(Refused(_))));
        // Bad names and a symlinked base are refused without touching anything.
        assert!(matches!(mk("../x"), Err(Usage(_))));
        assert!(matches!(create(&env, "..", uid, gid, "x"), Err(Usage(_))));
        symlink(&target, t.path().join("bob")).unwrap();
        assert!(matches!(create(&env, "bob", uid, gid, "x"), Err(Refused(_))));
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }
}
