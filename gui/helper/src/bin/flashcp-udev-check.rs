//! `flashcp-udev-check DEVNODE`: run from the drive-mode udev rule (IMPORT{program}) as root.
//! Prints udev properties on stdout: `FLASHCP_DRIVE_MODE=1` + `UDISKS_IGNORE=1` when the active
//! seat user's drive daemon is on and `flashcp mount --check` accepts the volume, or (plus
//! `FLASHCP_RECOVER=1`) whenever the volume has an interrupted flashcp commit; `FLASHCP_KEEP=1`
//! when the check says busy (keep the previous decision); nothing otherwise, which leaves the
//! device to udisks and the kernel driver. Any error, oddity or timeout also prints nothing.
//!
//! The volume is parsed only by `flashcp mount --check` in a transient systemd service (see
//! SANDBOX): a dynamic user, no capabilities, no network, an empty /etc and /run, a read-only
//! system, a seccomp filter, tight limits, and only the read-only device fd. The parent never
//! waits more than 1.5 s for `systemd-run` and never waits for one it killed. See SECURITY-NOTES.md.
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::time::{Duration, Instant};

const FLASHCP: &str = "/usr/bin/flashcp";
const SYSTEMD_RUN: &std::ffi::CStr = c"/usr/bin/systemd-run";
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
    let wants = daemon_wants(uid, uuid.as_deref(), dev.rsplit('/').next()?);
    if wants == Some(false) {
        // The daemon gave this drive to the kernel driver: visible to udisks, it mounts it itself.
        return Some("UDISKS_AUTO=0\n".into());
    }
    let (code, out) = check(dev)?;
    // 121..=123: the helper could not run systemd-run; stderr goes to udev's log.
    if std::env::var_os("FLASHCP_UDEV_CHECK_DEBUG").is_some() {
        eprintln!("flashcp-udev-check: {dev}: check exit {code}: {}", out.trim());
    }
    let takeover = format!("FLASHCP_DRIVE_MODE=1\nFLASHCP_DRIVE_UID={uid}\nUDISKS_IGNORE=1\n");
    match code {
        4 => Some("FLASHCP_KEEP=1\n".into()),
        // busy_unknown: the check could not see the holders/mounts, so it is no takeover.
        0 if out.contains("busy_unknown") => None,
        // An interrupted flashcp commit: flashcp's recovery first, even with drive mode off (the
        // daemon mounts it once, then hands it over); never the kernel driver before that.
        // Only while the seat user's daemon runs (its socket exists): with nobody to mount it, a
        // hidden stick would just be invisible; the kernel may mount it (the volume stays
        // consistent, and the journal's pre-state checks protect a later replay).
        0 if out.contains("\"pending_journal\":true") && daemon_running(uid) => {
            Some(format!("{takeover}FLASHCP_RECOVER=1\n"))
        }
        0 if wants == Some(true) => Some(takeover),
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

/// The seat user's `flashcp drived` runs: its control socket exists (lstat, owned by `uid`).
fn daemon_running(uid: u32) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let base = format!("/run/user/{uid}/flashcp");
    let owned = |p: &str| fs::symlink_metadata(p).is_ok_and(|m| m.uid() == uid);
    owned(&base)
        && fs::symlink_metadata(format!("{base}/drived.sock"))
            .is_ok_and(|m| m.uid() == uid && m.file_type().is_socket())
}

/// lstat only (never follows), everything must be owned by `uid`. Some(false): a per-drive
/// kernel-fallback flag (by UUID or device name) exists; Some(true): the daemon is on (its
/// "on" marker exists); None: neither.
fn daemon_wants(uid: u32, uuid: Option<&str>, devname: &str) -> Option<bool> {
    let base = format!("/run/user/{uid}/flashcp");
    let owned = |p: &str, dir: bool| {
        fs::symlink_metadata(p).is_ok_and(|m| m.uid() == uid && if dir { m.is_dir() } else { m.is_file() })
    };
    if !owned(&base, true) {
        return None;
    }
    let flag = |k: &str| owned(&format!("{base}/kernel/{k}"), false);
    if uuid.is_some_and(flag) || (valid_key(devname) && flag(devname)) {
        return Some(false);
    }
    owned(&format!("{base}/drive-mode-on"), false).then_some(true)
}

/// The sandbox of the check, as properties of a transient service that PID 1 starts: udevd's own
/// seccomp filter (inherited by everything it runs) denies Landlock, mount and seccomp, so the
/// isolation cannot be built here. Only the device (read-only, as stdin) is passed in.
const SANDBOX: &[&str] = &[
    "DynamicUser=yes",
    "NoNewPrivileges=yes",
    "CapabilityBoundingSet=",
    "PrivateNetwork=yes",
    "PrivateIPC=yes",
    "PrivateDevices=yes",
    "PrivateTmp=yes",
    "ProtectSystem=strict",
    "ProtectHome=yes",
    "TemporaryFileSystem=/etc /var /run /srv /opt /mnt /media /root",
    "BindReadOnlyPaths=/etc/ld.so.cache",
    "ProtectProc=invisible",
    "ProtectKernelTunables=yes",
    "ProtectKernelModules=yes",
    "ProtectKernelLogs=yes",
    "ProtectControlGroups=yes",
    "ProtectClock=yes",
    "ProtectHostname=yes",
    // "none" is not accepted for transient units (D-Bus: Invalid argument); AF_UNIX only, and the
    // private network, /run and IPC leave nothing to connect to.
    "RestrictAddressFamilies=AF_UNIX",
    "IPAddressDeny=any",
    "RestrictNamespaces=yes",
    "RestrictRealtime=yes",
    "RestrictSUIDSGID=yes",
    "LockPersonality=yes",
    "MemoryDenyWriteExecute=yes",
    "SystemCallArchitectures=native",
    "SystemCallFilter=@system-service",
    "SystemCallFilter=~@privileged @resources @mount",
    "SystemCallErrorNumber=EPERM",
    "UMask=0777",
    "MemoryMax=256M",
    "RuntimeMaxSec=2",
    "LimitCPU=2",
    "LimitFSIZE=0",
    "LimitNOFILE=16",
    "LimitCORE=0",
];

/// Runs the check through `systemd-run --wait --pipe` (the device as its stdin, which systemd
/// passes to the service as fd 0); returns its exit code and (up to 4 KB of) its stdout, or None
/// on timeout/oddity. Any failure to set the sandbox up fails the unit: no takeover.
fn check(dev: &str) -> Option<(i32, String)> {
    let devc = CString::new(dev).ok()?;
    let fd = unsafe { libc::open(devc.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK) };
    if fd < 0 || !fs::metadata(format!("/proc/self/fd/{fd}")).is_ok_and(|m| m.file_type().is_block_device()) {
        return None;
    }
    let mut argv: Vec<CString> = ["systemd-run", "--quiet", "--wait", "--pipe", "--collect", "--service-type=exec"]
        .iter()
        .map(|s| CString::new(*s).unwrap())
        .collect();
    for p in SANDBOX {
        argv.push(CString::new(format!("--property={p}")).ok()?);
    }
    for a in ["--", FLASHCP, "mount", "--check", "--json", "--device-fd", "0", dev] {
        argv.push(CString::new(a).ok()?);
    }
    let mut argp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argp.push(std::ptr::null());
    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    // Debug runs (by hand) see systemd-run's own errors; under udev its stderr is /dev/null.
    let err = if std::env::var_os("FLASHCP_UDEV_CHECK_DEBUG").is_some() { 2 } else { null };
    let mut pipe = [-1; 2];
    if null < 0 || unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        unsafe {
            // 0 = the device, 1 = the pipe, 2 = /dev/null; nothing else survives the exec.
            if libc::dup2(fd, 0) < 0 || libc::dup2(pipe[1], 1) < 0 || libc::dup2(err, 2) < 0 {
                libc::_exit(121);
            }
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) != 0 {
                libc::_exit(122);
            }
            libc::execve(SYSTEMD_RUN.as_ptr(), argp.as_ptr(), envp.as_ptr());
            libc::_exit(123);
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
            // Kill systemd-run and do not wait for it; the unit itself ends at RuntimeMaxSec.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            return None;
        }
    }
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
