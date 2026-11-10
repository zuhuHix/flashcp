//! `flashcp-udev-check DEVNODE`: run from the drive-mode udev rule (IMPORT{program}) as root.
//! Prints udev properties on stdout: `FLASHCP_DRIVE_MODE=1` + `UDISKS_IGNORE=1` when the active
//! seat user's drive daemon is on and `flashcp mount --check` accepts the volume; `FLASHCP_KEEP=1`
//! when the check says busy (keep the previous decision); nothing otherwise, which leaves the
//! device to udisks and the kernel driver. Any error, oddity or timeout also prints nothing.
//!
//! The volume is parsed only by the child, as nobody, with no new privileges, tight rlimits, an
//! empty environment, only the read-only device fd, and (best effort) Landlock. The parent never
//! waits more than 1.5 s and never waits for a child it killed. See SECURITY-NOTES.md.
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::time::{Duration, Instant};

const FLASHCP: &str = "/usr/bin/flashcp";
const NOBODY: libc::uid_t = 65534;
const TIMEOUT: Duration = Duration::from_millis(1500);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(props) = decide(&args) {
        print!("{props}");
    }
}

fn decide(args: &[String]) -> Option<String> {
    let [_, dev] = args else { return None };
    if !dev.starts_with("/dev/") || dev.contains("/../") || dev.len() > 64 {
        return None;
    }
    let uid = active_uid(&fs::read_to_string("/run/systemd/seats/seat0").ok()?)?;
    let uuid = std::env::var("ID_FS_UUID").ok().filter(|u| valid_key(u));
    if !daemon_wants(uid, uuid.as_deref(), dev.rsplit('/').next()?)? {
        // The daemon gave this drive to the kernel driver: visible to udisks, it mounts it itself.
        return Some("UDISKS_AUTO=0\n".into());
    }
    let (code, out) = check(dev)?;
    match code {
        // busy_unknown: the check could not see the holders/mounts, so it is no takeover.
        0 if !out.contains("busy_unknown") => {
            Some(format!("FLASHCP_DRIVE_MODE=1\nFLASHCP_DRIVE_UID={uid}\nUDISKS_IGNORE=1\n"))
        }
        4 => Some("FLASHCP_KEEP=1\n".into()),
        _ => None,
    }
}

/// `ACTIVE_UID=` of seat0's state file (written by logind; no D-Bus from udev).
fn active_uid(seat: &str) -> Option<u32> {
    let uid = seat.lines().find_map(|l| l.strip_prefix("ACTIVE_UID="))?.trim().parse().ok()?;
    (uid != 0).then_some(uid)
}

fn valid_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= 36 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// lstat only (never follows), everything must be owned by `uid`. None: the daemon is off (no
/// "on" marker); Some(false): a per-drive kernel-fallback flag (by UUID or device name) exists.
fn daemon_wants(uid: u32, uuid: Option<&str>, devname: &str) -> Option<bool> {
    let base = format!("/run/user/{uid}/flashcp");
    let owned = |p: &str, dir: bool| {
        fs::symlink_metadata(p).is_ok_and(|m| m.uid() == uid && if dir { m.is_dir() } else { m.is_file() })
    };
    if !owned(&base, true) || !owned(&format!("{base}/drive-mode-on"), false) {
        return None;
    }
    let flag = |k: &str| owned(&format!("{base}/kernel/{k}"), false);
    Some(!(uuid.is_some_and(flag) || (valid_key(devname) && flag(devname))))
}

/// Runs the check in a sandboxed child; returns its exit code and (up to 4 KB of) its stdout, or
/// None on timeout/oddity.
fn check(dev: &str) -> Option<(i32, String)> {
    let devc = CString::new(dev).ok()?;
    let fd = unsafe { libc::open(devc.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK) };
    if fd < 0 || !fs::metadata(format!("/proc/self/fd/{fd}")).is_ok_and(|m| m.file_type().is_block_device()) {
        return None;
    }
    let bin = std::env::var("FLASHCP_UDEV_CHECK_BIN").ok().filter(|_| unsafe { libc::getuid() } != 0);
    let bin = CString::new(bin.as_deref().unwrap_or(FLASHCP)).ok()?;
    let argv: Vec<CString> = ["flashcp", "mount", "--check", "--json", "--device-fd", "3", dev]
        .iter()
        .map(|s| CString::new(*s).unwrap())
        .collect();
    let mut argp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argp.push(std::ptr::null());
    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    let mut pipe = [-1; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        unsafe {
            // Only 0, 2 (/dev/null), 1 (the pipe) and 3 (the device) survive the exec.
            // (Allocating after fork is fine: this process is single-threaded.)
            let ll = landlock_ruleset();
            if null < 0 || libc::dup2(null, 0) < 0 || libc::dup2(pipe[1], 1) < 0 || libc::dup2(null, 2) < 0 {
                libc::_exit(127);
            }
            // udev starts us with only 0-2 open, so the device usually IS fd 3: dup2 would then be a
            // no-op that keeps O_CLOEXEC, and exec would close it.
            let moved = if fd == 3 { libc::fcntl(3, libc::F_SETFD, 0) } else { libc::dup2(fd, 3) };
            if moved < 0 {
                libc::_exit(127);
            }
            for (res, v) in [
                (libc::RLIMIT_CPU, 2),
                (libc::RLIMIT_AS, 256 << 20),
                (libc::RLIMIT_NOFILE, 16),
                (libc::RLIMIT_FSIZE, 0),
                (libc::RLIMIT_CORE, 0),
            ] {
                let r = libc::rlimit { rlim_cur: v, rlim_max: v };
                if libc::setrlimit(res, &r) != 0 {
                    libc::_exit(127);
                }
            }
            if libc::getuid() == 0
                && (libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(NOBODY) != 0 || libc::setuid(NOBODY) != 0)
            {
                libc::_exit(127);
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                libc::_exit(127);
            }
            if let Some(rs) = ll {
                libc::syscall(libc::SYS_landlock_restrict_self, rs, 0u32);
            }
            if libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) != 0 {
                libc::_exit(127);
            }
            libc::execve(bin.as_ptr(), argp.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    unsafe { libc::close(pipe[1]) };
    let deadline = Instant::now() + TIMEOUT;
    let (mut out, mut eof) = (Vec::new(), false);
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().min(10) as i32;
        let mut p = libc::pollfd { fd: pipe[0], events: libc::POLLIN, revents: 0 };
        if !eof && unsafe { libc::poll(&mut p, 1, left) } > 0 {
            let mut b = [0u8; 4096];
            let n = unsafe { libc::read(pipe[0], b.as_mut_ptr().cast(), b.len()) };
            eof = n <= 0;
            out.extend_from_slice(&b[..n.max(0) as usize]);
            out.truncate(4096);
        } else if eof {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            let code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status))?;
            return Some((code, String::from_utf8_lossy(&out).into_owned()));
        }
        if r < 0 || Instant::now() >= deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) }; // and do not wait: it may be stuck in D state
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A Landlock ruleset (built in the child) that allows only reading and executing below /usr and
/// /lib* and reading /etc/ld.so.cache (exec of a dynamically linked /usr/bin/flashcp), reading
/// /sys and the child's own mountinfo (the check's busy test). None if the kernel has no
/// Landlock: the other layers still apply.
fn landlock_ruleset() -> Option<libc::c_long> {
    #[repr(C)]
    struct Attr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }
    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }
    let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<Attr>(), 0usize, 1u32) };
    if abi < 1 {
        return None;
    }
    // EXECUTE..MAKE_SYM (ABI 1), REFER (2), TRUNCATE (3).
    let handled: u64 = ((1 << 13) - 1) | if abi >= 2 { 1 << 13 } else { 0 } | if abi >= 3 { 1 << 14 } else { 0 };
    // ABI 4: also deny TCP bind/connect (no rules allow any port). Older ABIs take only the first field.
    let attr = Attr { handled_access_fs: handled, handled_access_net: if abi >= 4 { 3 } else { 0 } };
    let size = if abi >= 4 { size_of::<Attr>() } else { size_of::<u64>() };
    let rs = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, size, 0u32) };
    if rs < 0 {
        return None;
    }
    let (exec, read_file, read_dir) = (1u64, 1u64 << 2, 1u64 << 3);
    for (path, access) in [
        (c"/usr", exec | read_file | read_dir),
        (c"/lib", exec | read_file | read_dir),
        (c"/lib64", exec | read_file | read_dir),
        (c"/etc/ld.so.cache", read_file),
        (c"/sys", read_file | read_dir),
        (c"/proc/self/mountinfo", read_file),
    ] {
        let pfd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if pfd >= 0 {
            let is_dir = fs::metadata(format!("/proc/self/fd/{pfd}")).is_ok_and(|m| m.is_dir());
            let rule = PathBeneath { allowed_access: if is_dir { access } else { access & read_file }, parent_fd: pfd };
            unsafe { libc::syscall(libc::SYS_landlock_add_rule, rs, 1u32, &rule, 0u32) };
            unsafe { libc::close(pfd) };
        }
    }
    Some(rs)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsing() {
        assert_eq!(active_uid("# x\nSTATE=active\nACTIVE=c1\nACTIVE_UID=1000\n"), Some(1000));
        for bad in ["ACTIVE_UID=0\n", "ACTIVE_UID=x\n", "STATE=online\n", ""] {
            assert_eq!(active_uid(bad), None, "{bad:?}");
        }
        for (k, ok) in [("1A2B-3C4D", true), ("sdb1", true), ("", false), ("../x", false), ("a b", false)] {
            assert_eq!(valid_key(k), ok, "{k:?}");
        }
        assert_eq!(decide(&["x".into(), "/etc/passwd".into()]), None);
        assert_eq!(decide(&["x".into()]), None);
    }
}
