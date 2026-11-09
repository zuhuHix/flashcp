//! Run flashcp as the invoking user, holding exactly one extra fd: the device, as fd 3.
//!
//! The helper opened and validated that fd itself; the child gets it via `dup2` and nothing else
//! (every other fd >= 4 is closed at exec). No device ownership or permission is changed, so
//! there is nothing to restore if the helper dies.

use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

use crate::args::TARGET_FD;

/// The flashcp binary. Overridable only when the helper is compiled, never at run time.
pub const FLASHCP: &str = match option_env!("FLASHCP_HELPER_FLASHCP_PATH") {
    Some(p) => p,
    None => "/usr/bin/flashcp",
};

/// Signals held back in the helper and forwarded to the running child instead, so a Ctrl-C or
/// hangup stops flashcp cleanly and no further run is started.
const HELD_SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// The invoking user, resolved from the passwd database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Creds {
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub groups: Vec<libc::gid_t>,
    pub home: String,
}

/// Most bytes of captured stdout kept (the rest is drained and dropped).
const MAX_CAPTURE: u64 = 64 * 1024;

/// `close_range(2)` flag: mark the range close-on-exec instead of closing it now.
const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

/// What to run and how. `drop: None` skips the privilege drop (tests only; production always
/// passes the invoking user's credentials).
#[derive(Debug, Clone)]
pub struct ExecPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub drop: Option<Creds>,
    pub env: Vec<(String, String)>,
    /// Handed to the child as fd 3 (rewound to offset 0 first). `None`: the child gets no fd >= 3.
    pub target_fd: Option<RawFd>,
}

/// The complete environment flashcp gets. Nothing from the caller's environment survives, so
/// LD_PRELOAD, LANGUAGE, locale paths and the like cannot influence it.
pub fn user_env(home: &str) -> Vec<(String, String)> {
    [("PATH", "/usr/bin:/bin"), ("LANG", "C.UTF-8"), ("LC_ALL", "C.UTF-8"), ("HOME", home)]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Which user asked. pkexec sets PKEXEC_UID in the (otherwise scrubbed) environment; SUDO_UID is
/// accepted for `sudo` invocations. Root is refused: flashcp must never run as uid 0 here.
pub fn invoking_uid(pkexec: Option<&str>, sudo: Option<&str>) -> Result<libc::uid_t, String> {
    let raw = pkexec.or(sudo).ok_or("cannot tell which user invoked the helper (PKEXEC_UID unset)")?;
    match raw.parse::<libc::uid_t>() {
        Ok(0) => Err("refusing to run flashcp as root".into()),
        Ok(u) if raw.bytes().all(|b| b.is_ascii_digit()) && u != libc::uid_t::MAX => Ok(u),
        _ => Err("invalid invoking uid".into()),
    }
}

/// passwd entry + supplementary groups for `uid`.
pub fn lookup_user(uid: libc::uid_t) -> Result<Creds, String> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    loop {
        // SAFETY: all pointers are valid for the given lengths for the duration of the call.
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
    // SAFETY: on success pw_name/pw_dir point into buf as NUL-terminated strings.
    let (name, home) = unsafe {
        (
            std::ffi::CStr::from_ptr(pw.pw_name).to_owned(),
            std::ffi::CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned(),
        )
    };
    let gid = pw.pw_gid;
    let mut n: libc::c_int = 32;
    loop {
        let mut groups = vec![0 as libc::gid_t; n as usize];
        let mut got = n;
        // SAFETY: groups has room for `got` entries; getgrouplist updates `got`.
        let rc = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut got) };
        if rc >= 0 {
            groups.truncate(got as usize);
            return Ok(Creds { uid, gid, groups, home });
        }
        if got <= n {
            if n >= 65536 {
                return Err("getgrouplist failed".into());
            }
            n *= 2;
        } else {
            n = got;
        }
    }
}

fn held_set() -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset initialise and modify a local sigset_t.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in HELD_SIGNALS {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Block the held signals in the (single-threaded) helper; they are forwarded to the child while
/// one runs, and stay pending otherwise (see [`take_pending_signal`]).
pub fn block_signals() -> io::Result<()> {
    let set = held_set();
    // SAFETY: valid set pointer; old mask not needed.
    check(unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) })
}

/// A held signal that arrived while no child was running (consumed), so the caller can stop
/// before starting the next run.
pub fn take_pending_signal() -> Option<libc::c_int> {
    let set = held_set();
    let ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointers; zero timeout polls.
    let sig = unsafe { libc::sigtimedwait(&set, std::ptr::null_mut(), &ts) };
    (sig > 0).then_some(sig)
}

/// Mark every fd >= `first` close-on-exec, so the exec drops them all. Marking instead of
/// closing keeps std's internal exec-error pipe working until the exec itself (closing it early
/// would turn a failed privilege drop into a silent abort). Async-signal-safe, no allocation.
fn cloexec_from(first: libc::c_uint) -> io::Result<()> {
    // SAFETY: plain syscall with integer arguments.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, CLOSE_RANGE_CLOEXEC) };
    if rc == 0 {
        return Ok(());
    }
    match io::Error::last_os_error().raw_os_error() {
        // No close_range (< 5.9) or no CLOSE_RANGE_CLOEXEC (< 5.11): walk /proc/self/fd.
        Some(libc::ENOSYS) | Some(libc::EINVAL) => cloexec_from_procfs(first),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Fallback for [`cloexec_from`]: getdents64 over /proc/self/fd into a stack buffer.
pub(crate) fn cloexec_from_procfs(first: libc::c_uint) -> io::Result<()> {
    // SAFETY: raw syscalls on a stack buffer we own; dirfd is closed before returning.
    unsafe {
        let dir = libc::open(c"/proc/self/fd".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC);
        if dir < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = [0u8; 2048];
        let res = loop {
            let n = libc::syscall(libc::SYS_getdents64, dir, buf.as_mut_ptr(), buf.len());
            if n < 0 {
                break Err(io::Error::last_os_error());
            }
            if n == 0 {
                break Ok(());
            }
            let mut off = 0usize;
            while off + 19 < n as usize {
                // struct linux_dirent64 { u64 ino; i64 off; u16 reclen; u8 type; char name[]; }
                let reclen = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]) as usize;
                let mut fd: u64 = 0;
                let mut digits = 0;
                for &b in &buf[off + 19..off + reclen] {
                    if !b.is_ascii_digit() {
                        break;
                    }
                    fd = fd.saturating_mul(10).saturating_add(u64::from(b - b'0'));
                    digits += 1;
                }
                if digits > 0 && fd >= u64::from(first) && fd != dir as u64 && fd <= i32::MAX as u64 {
                    libc::fcntl(fd as libc::c_int, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                if reclen == 0 {
                    break;
                }
                off += reclen;
            }
        };
        libc::close(dir);
        res
    }
}

/// Runs in the forked child before exec: install the target as fd 3, drop every other fd >= 4,
/// become the user for good, then unblock signals. Only async-signal-safe syscalls, and no
/// allocation (errors are raw errno values).
fn child_setup(target: Option<RawFd>, drop: Option<&Creds>, set: &libc::sigset_t) -> io::Result<()> {
    // SAFETY: plain syscalls on memory allocated before fork.
    unsafe {
        let first_closed = match target {
            Some(fd) if fd == TARGET_FD => {
                check(libc::fcntl(TARGET_FD, libc::F_SETFD, 0))?;
                TARGET_FD + 1
            }
            Some(fd) => {
                // dup2 leaves the new fd without FD_CLOEXEC.
                if libc::dup2(fd, TARGET_FD) != TARGET_FD {
                    return Err(io::Error::last_os_error());
                }
                TARGET_FD + 1
            }
            None => TARGET_FD,
        };
        cloexec_from(first_closed as libc::c_uint)?;
        if let Some(c) = drop {
            check(libc::setgroups(c.groups.len(), c.groups.as_ptr()))?;
            check(libc::setresgid(c.gid, c.gid, c.gid))?;
            check(libc::setresuid(c.uid, c.uid, c.uid))?;
            // If any way back to root remained, stop here rather than run flashcp privileged.
            if libc::setuid(0) == 0 || libc::geteuid() != c.uid || libc::getegid() != c.gid {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
        }
        // No setuid/file-capability binary started by flashcp can gain privileges.
        check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0))?;
        check(libc::sigprocmask(libc::SIG_UNBLOCK, set, std::ptr::null_mut()))?;
    }
    Ok(())
}

/// Make sure fd 3 is occupied in the parent while spawning, so std's internal exec-error pipe can
/// never be allocated as fd 3 (the child's dup2 onto 3 would clobber it). Returns the fd the
/// child should dup from, and a guard to keep alive until the spawn returned.
fn occupy_fd3(fd: RawFd) -> io::Result<(RawFd, Option<OwnedFd>)> {
    if fd < TARGET_FD {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // SAFETY: F_GETFD/F_DUPFD_CLOEXEC on integer fds; the duplicate is owned below.
    unsafe {
        if fd == TARGET_FD || libc::fcntl(TARGET_FD, libc::F_GETFD) >= 0 {
            return Ok((fd, None));
        }
        let d = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, TARGET_FD);
        if d < 0 {
            return Err(io::Error::last_os_error());
        }
        let guard = OwnedFd::from_raw_fd(d);
        Ok((if d == TARGET_FD { d } else { fd }, Some(guard)))
    }
}

fn spawn(plan: &ExecPlan, stdout: Stdio) -> io::Result<std::process::Child> {
    let (target, guard) = match plan.target_fd {
        Some(fd) => {
            // SAFETY: lseek on an integer fd.
            if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let (t, g) = occupy_fd3(fd)?;
            (Some(t), g)
        }
        None => (None, None),
    };
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
        .env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::inherit());
    let creds = plan.drop.clone();
    let set = held_set();
    // SAFETY: child_setup is async-signal-safe (see its doc comment).
    unsafe {
        cmd.pre_exec(move || child_setup(target, creds.as_ref(), &set));
    }
    let child = cmd.spawn();
    drop(guard);
    child
}

/// Wait for `child`, forwarding held signals to it.
fn wait(child: &mut std::process::Child) -> io::Result<ExitStatus> {
    let set = held_set();
    let pid = child.id() as libc::pid_t;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(st);
        }
        let ts = libc::timespec { tv_sec: 0, tv_nsec: 100_000_000 };
        // SAFETY: valid pointers; returns a pending held signal or -1 on timeout.
        let sig = unsafe { libc::sigtimedwait(&set, std::ptr::null_mut(), &ts) };
        if sig > 0 {
            // SAFETY: pid is our own unreaped child, so it cannot have been recycled.
            unsafe { libc::kill(pid, sig) };
        }
    }
}

/// Spawn the plan (no shell, absolute program path, scrubbed env, cwd /, stdin null, stdout as
/// given, stderr inherited) and wait for it, forwarding held signals.
pub fn run(plan: &ExecPlan, stdout: Stdio) -> io::Result<ExitStatus> {
    let mut child = spawn(plan, stdout)?;
    wait(&mut child)
}

/// Like [`run`], but capture stdout (at most 64 KiB is kept) instead of passing it through.
pub fn run_capture(plan: &ExecPlan) -> io::Result<(ExitStatus, Vec<u8>)> {
    let mut child = spawn(plan, Stdio::piped())?;
    let mut out = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let read =
            (&mut pipe).take(MAX_CAPTURE).read_to_end(&mut out).and_then(|_| io::copy(&mut pipe, &mut io::sink()));
        if let Err(e) = read {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    Ok((wait(&mut child)?, out))
}

/// flashcp's exit code, or 128+signal like a shell.
pub fn exit_code(st: ExitStatus) -> i32 {
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    fn plan(prog: &str, args: &[&str], target_fd: Option<RawFd>) -> ExecPlan {
        ExecPlan {
            program: prog.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            drop: None,
            env: user_env("/home/someone"),
            target_fd,
        }
    }

    /// `/bin/sh -c <script>` (no script file, so the shell holds no extra fds), stdout captured.
    fn sh(script: &str, target_fd: Option<RawFd>) -> (ExitStatus, String) {
        let (st, out) = run_capture(&plan("/bin/sh", &["-c", script], target_fd)).unwrap();
        (st, String::from_utf8(out).unwrap())
    }

    fn fd_list(out: &str) -> Vec<String> {
        out.lines().skip_while(|l| *l != "FDS").skip(1).take_while(|l| *l != "END").map(String::from).collect()
    }

    const LIST_FDS: &str = "echo FDS; ls /proc/$$/fd; echo END";

    #[test]
    fn target_is_fd3_readable_rewound_and_nothing_else_leaks() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello-fd3").unwrap();
        // A deliberately inheritable (no CLOEXEC) fd in the parent must not reach the child.
        let leak = unsafe { libc::dup(f.as_file().as_raw_fd()) };
        assert!(leak > 3);
        let script = format!("readlink /proc/$$/fd/3; cat <&3; echo; {LIST_FDS}");
        for _ in 0..2 {
            // The write above left the offset at the end; each run starts at 0 again.
            let (st, out) = sh(&script, Some(f.as_file().as_raw_fd()));
            assert!(st.success(), "{out}");
            let mut lines = out.lines();
            assert_eq!(lines.next().unwrap(), f.path().to_str().unwrap());
            assert_eq!(lines.next().unwrap(), "hello-fd3");
            assert_eq!(fd_list(&out), ["0", "1", "2", "3"], "{out}");
        }
        unsafe { libc::close(leak) };
    }

    #[test]
    fn no_target_means_no_fd3() {
        let (st, out) = sh(LIST_FDS, None);
        assert!(st.success());
        assert_eq!(fd_list(&out), ["0", "1", "2"], "{out}");
    }

    #[test]
    fn target_already_at_fd3_keeps_working() {
        // occupy_fd3 picks whichever fd it can; either way the child must see the file at 3.
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), "x3").unwrap();
        let (fd, guard) = occupy_fd3(f.as_file().as_raw_fd()).unwrap();
        assert!(fd >= 3);
        let (st, out) = sh("cat <&3", Some(fd));
        assert!(st.success());
        assert_eq!(out, "x3");
        drop(guard);
        assert!(occupy_fd3(2).is_err());
    }

    #[test]
    fn procfs_fallback_marks_fds_cloexec() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let leak = unsafe { libc::dup(f.as_file().as_raw_fd()) };
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", LIST_FDS]).stdout(Stdio::piped());
        unsafe { cmd.pre_exec(|| cloexec_from_procfs(3)) };
        let out = String::from_utf8(cmd.output().unwrap().stdout).unwrap();
        assert_eq!(fd_list(&out), ["0", "1", "2"], "{out}");
        unsafe { libc::close(leak) };
    }

    #[test]
    fn env_is_sanitized_and_cwd_is_root() {
        // cargo always sets CARGO_MANIFEST_DIR for the test process; it must not leak.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let (st, out) = sh("env; echo cwd=$(pwd); read x && echo stdin-had-data; true", None);
        assert!(st.success());
        let mut keys: Vec<&str> = out.lines().filter_map(|l| l.split_once('=')).map(|(k, _)| k).collect();
        keys.sort();
        // dash/bash add PWD (and bash SHLVL/_); everything else must come from the plan.
        keys.retain(|k| !["PWD", "SHLVL", "_", "OLDPWD"].contains(k));
        assert_eq!(keys, ["HOME", "LANG", "LC_ALL", "PATH", "cwd"], "{out}");
        assert!(out.contains("PATH=/usr/bin:/bin\n") && out.contains("HOME=/home/someone\n"));
        assert!(out.contains("cwd=/\n"));
        assert!(!out.contains("stdin-had-data"));
    }

    #[test]
    fn capture_and_exit_codes() {
        let (st, out) = sh("echo '{\"label\":\"X\"}'; exit 7", None);
        assert_eq!(exit_code(st), 7);
        assert_eq!(out, "{\"label\":\"X\"}\n");
        let (st, _) = sh("kill -TERM $$", None);
        assert_eq!(exit_code(st), 128 + libc::SIGTERM);
        // Output beyond the cap is drained (the child is not blocked) and dropped.
        let (st, out) = sh("head -c 200000 /dev/zero", None);
        assert!(st.success());
        assert_eq!(out.len() as u64, MAX_CAPTURE);
        let st = run(&plan("/bin/sh", &["-c", "exit 0"], None), Stdio::null()).unwrap();
        assert!(st.success());
    }

    #[test]
    fn missing_program_is_an_error() {
        assert!(run(&plan("/nonexistent/flashcp", &[], None), Stdio::null()).is_err());
        let f = tempfile::tempfile().unwrap();
        assert!(run(&plan("/nonexistent/flashcp", &[], Some(f.as_raw_fd())), Stdio::null()).is_err());
    }

    #[test]
    fn failed_privilege_drop_aborts_spawn() {
        // Unprivileged, setgroups() fails with EPERM even for our own groups. That failure must
        // abort the spawn (fail closed) instead of running the program with whatever ids it had,
        // and must be reported as an error even with the fd juggling in place.
        let uid = unsafe { libc::getuid() };
        let me = lookup_user(uid).unwrap();
        assert_eq!(me.uid, uid);
        assert!(me.groups.contains(&me.gid));
        let f = tempfile::tempfile().unwrap();
        let p = ExecPlan { drop: Some(me), ..plan("/usr/bin/id", &["-u"], Some(f.as_raw_fd())) };
        let r = run(&p, Stdio::null());
        if uid != 0 {
            assert!(r.is_err());
        }
    }

    #[test]
    fn pending_signal_is_taken() {
        assert_eq!(take_pending_signal(), None);
    }

    #[test]
    fn uid_parsing() {
        assert_eq!(invoking_uid(Some("1000"), None), Ok(1000));
        assert_eq!(invoking_uid(None, Some("1001")), Ok(1001));
        assert_eq!(invoking_uid(Some("1000"), Some("0")), Ok(1000));
        for bad in [Some("0"), Some(""), Some("-1"), Some("+5"), Some("10 "), Some("abc"), Some("4294967295")] {
            assert!(invoking_uid(bad, Some("1000")).is_err(), "{bad:?}");
        }
        assert!(invoking_uid(None, None).is_err());
    }

    #[test]
    fn flashcp_path_is_absolute() {
        assert!(FLASHCP.starts_with('/'));
    }
}
