//! Lend the device to the invoking user, run flashcp as that user, take the device back.
//!
//! Root is used for exactly one thing: changing the owner of one validated device node for the
//! duration of the run. flashcp itself never runs privileged.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::device::NodeStat;

/// The flashcp binary. Overridable only when the helper is compiled, never at run time.
pub const FLASHCP: &str = match option_env!("FLASHCP_HELPER_FLASHCP_PATH") {
    Some(p) => p,
    None => "/usr/bin/flashcp",
};

/// Signals held back in the helper (so it always gets to restore ownership) and forwarded to the
/// child instead.
const HELD_SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// The invoking user, resolved from the passwd database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Creds {
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub groups: Vec<libc::gid_t>,
    pub home: String,
}

/// What to run and how. `drop: None` skips the privilege drop (tests only; production always
/// passes the invoking user's credentials).
#[derive(Debug, Clone)]
pub struct ExecPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub drop: Option<Creds>,
    pub env: Vec<(String, String)>,
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

/// Block the held signals in the (single-threaded) helper. Called before the device is granted,
/// so a Ctrl-C or hangup can never interrupt us between granting and restoring.
pub fn block_signals() -> io::Result<()> {
    let set = held_set();
    // SAFETY: valid set pointer; old mask not needed.
    check(unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) })
}

/// Runs in the forked child before exec: become the user for good, then unblock signals.
/// Only async-signal-safe syscalls, and no allocation (errors are raw errno values).
fn child_setup(drop: Option<&Creds>, set: &libc::sigset_t) -> io::Result<()> {
    // SAFETY: plain syscalls on memory allocated before fork.
    unsafe {
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

/// Spawn the plan (no shell, absolute program path, scrubbed env, cwd /, stdin null) and wait
/// for it, forwarding held signals. Spawn rather than exec: the helper must outlive flashcp to
/// give the device back.
pub fn run(plan: &ExecPlan, stdout: Stdio) -> io::Result<ExitStatus> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
        .env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::inherit());
    let drop = plan.drop.clone();
    let set = held_set();
    // SAFETY: child_setup is async-signal-safe (see its doc comment).
    unsafe {
        cmd.pre_exec(move || child_setup(drop.as_ref(), &set));
    }
    let mut child = cmd.spawn()?;
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

/// flashcp's exit code, or 128+signal like a shell.
pub fn exit_code(st: ExitStatus) -> i32 {
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
}

/// A device node opened with O_PATH|O_NOFOLLOW. Every later operation goes through this fd, so
/// a rename/replace of the path after validation cannot redirect us to another node.
pub struct PinnedNode {
    fd: OwnedFd,
    st: libc::stat,
}

impl PinnedNode {
    pub fn open(path: &Path) -> io::Result<Self> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: valid C string; returned fd is owned below.
        let raw = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and out-pointer.
        check(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
        Ok(PinnedNode { fd, st })
    }

    pub fn stat(&self) -> NodeStat {
        NodeStat::from_libc(&self.st)
    }

    /// Make `uid` the owner (group and mode untouched, normally root:disk 0660 → uid:disk 0660)
    /// until the returned guard is dropped.
    pub fn grant(self, uid: libc::uid_t) -> io::Result<DeviceGrant> {
        let (uid0, gid0) = (self.st.st_uid, self.st.st_gid);
        chown_fd(&self.fd, uid, libc::gid_t::MAX)?;
        Ok(DeviceGrant { fd: self.fd, uid: uid0, gid: gid0 })
    }
}

fn chown_fd(fd: &OwnedFd, uid: libc::uid_t, gid: libc::gid_t) -> io::Result<()> {
    // SAFETY: AT_EMPTY_PATH with "" operates on the fd itself.
    check(unsafe { libc::fchownat(fd.as_raw_fd(), c"".as_ptr(), uid, gid, libc::AT_EMPTY_PATH) })
}

/// Restores the node's original owner when dropped (normal return, error return or unwind).
pub struct DeviceGrant {
    fd: OwnedFd,
    uid: libc::uid_t,
    gid: libc::gid_t,
}

impl Drop for DeviceGrant {
    fn drop(&mut self) {
        if let Err(e) = chown_fd(&self.fd, self.uid, self.gid) {
            eprintln!("flashcp-helper: could not restore device ownership: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_flashcp() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fake-flashcp")
    }

    /// Run `sh <script> args...` with output captured to a file.
    fn run_sh(script: &str, args: &[&str]) -> (ExitStatus, String) {
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("s.sh");
        fs::write(&s, script).unwrap();
        run_prog("/bin/sh", &[&[s.to_str().unwrap()], args].concat(), &t)
    }

    fn run_prog(prog: &str, args: &[&str], t: &tempfile::TempDir) -> (ExitStatus, String) {
        let out = t.path().join("out");
        let plan = ExecPlan {
            program: prog.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            drop: None,
            env: user_env("/home/someone"),
        };
        let st = run(&plan, Stdio::from(fs::File::create(&out).unwrap())).unwrap();
        (st, fs::read_to_string(&out).unwrap())
    }

    #[test]
    fn fake_flashcp_output_passes_through() {
        let t = tempfile::tempdir().unwrap();
        let fake = fake_flashcp();
        let (st, out) = run_prog("/bin/sh", &[fake.to_str().unwrap(), "info", "/dev/sdz1", "--json"], &t);
        assert!(st.success());
        assert!(out.starts_with("{\"label\":\"FAKEUSB\""), "{out}");
        let (st, out) = run_prog("/bin/sh", &[fake.to_str().unwrap(), "add", "/x", "/dev/sdz1", "--json"], &t);
        assert!(st.success());
        assert!(out.lines().last().unwrap().starts_with("{\"event\":\"done\""), "{out}");
    }

    #[test]
    fn env_is_sanitized_and_cwd_is_root() {
        // cargo always sets CARGO_MANIFEST_DIR for the test process; it must not leak.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let (st, out) = run_sh("env; echo cwd=$(pwd); read x && echo stdin-had-data; true", &[]);
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
    fn exit_codes_and_signals() {
        let (st, _) = run_sh("exit 7", &[]);
        assert_eq!(exit_code(st), 7);
        let (st, _) = run_sh("kill -TERM $$", &[]);
        assert_eq!(exit_code(st), 128 + libc::SIGTERM);
        let t = tempfile::tempdir().unwrap();
        let (st, out) = run_prog("/bin/sh", &[fake_flashcp().to_str().unwrap(), "bogus"], &t);
        assert_eq!(exit_code(st), 2);
        assert!(out.contains("\"event\":\"error\""));
    }

    #[test]
    fn missing_program_is_an_error() {
        let plan = ExecPlan { program: "/nonexistent/flashcp".into(), args: vec![], drop: None, env: vec![] };
        assert!(run(&plan, Stdio::null()).is_err());
    }

    #[test]
    fn failed_privilege_drop_aborts_spawn() {
        // Unprivileged, setgroups() fails with EPERM even for our own groups. That failure must
        // abort the spawn (fail closed) instead of running the program with whatever ids it had.
        let uid = unsafe { libc::getuid() };
        let me = lookup_user(uid).unwrap();
        assert_eq!(me.uid, uid);
        assert!(me.groups.contains(&me.gid));
        let t = tempfile::tempdir().unwrap();
        let out = t.path().join("out");
        let plan = ExecPlan { program: "/usr/bin/id".into(), args: vec!["-u".into()], drop: Some(me), env: vec![] };
        let r = run(&plan, Stdio::from(fs::File::create(&out).unwrap()));
        if uid != 0 {
            assert!(r.is_err());
        }
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
    fn pinned_node_and_grant_restore() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("node");
        fs::write(&f, "").unwrap();
        // Symlinks are opened as themselves (O_NOFOLLOW|O_PATH), so they show up as links.
        std::os::unix::fs::symlink("/dev/null", t.path().join("link")).unwrap();
        assert!(PinnedNode::open(&t.path().join("link")).unwrap().stat().is_symlink);
        let n = PinnedNode::open(&f).unwrap();
        assert!(!n.stat().is_block);
        // chown to ourselves is allowed unprivileged: exercises grant + restore through the fd.
        let uid = unsafe { libc::getuid() };
        let g = n.grant(uid).unwrap();
        drop(g);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&f).unwrap().uid(), uid);
    }

    #[test]
    fn flashcp_path_is_absolute() {
        assert!(FLASHCP.starts_with('/'));
    }
}
