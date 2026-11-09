//! The privileged pieces of the flashcp desktop app, both started as root by pkexec:
//!
//! - `flashcp-helper` opens one USB drive once per job (`O_EXCL` for writes), validates the
//!   OPENED fd (USB topology, udev, idle, not the system disk), checks the drive's stable
//!   identity against the administrator's trust list, and runs `/usr/bin/flashcp` as the
//!   invoking user with that fd as fd 3. No device ownership or permission is ever changed.
//! - `flashcp-trust` adds a drive's identity to `/etc/flashcp/trusted-drives`.
//!
//! See packaging/desktop/SECURITY-NOTES.md.

pub mod args;
pub mod device;
pub mod exec;
pub mod identity;
pub mod json;
pub mod mounts;
pub mod trust;
pub mod weak;

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::process::Stdio;

use args::{DevName, Request};
use device::{fstat, open_device, validate_device, NodeStat, Roots, ValidDevice};
use exec::ExecPlan;
use identity::{read_identity, Identity};
use trust::TrustStore;

/// Exit code for a refused request.
pub const EXIT_VALIDATION: i32 = 3;
/// Exit code for an unexpected failure inside the helper.
pub const EXIT_INTERNAL: i32 = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum Fail {
    Validation(String),
    Internal(String),
}
use Fail::{Internal, Validation};

fn report(prog: &str, r: Result<i32, Fail>) -> i32 {
    match r {
        Ok(code) => code,
        Err(Validation(m)) => {
            json::print_line(&json::error_line(prog, &m));
            EXIT_VALIDATION
        }
        Err(Internal(m)) => {
            json::print_line(&json::error_line(prog, &m));
            EXIT_INTERNAL
        }
    }
}

/// `flashcp-helper` entry point; returns the exit code (flashcp's own, or 3/4).
pub fn helper_main() -> i32 {
    report("flashcp-helper", helper(&std::env::args_os().skip(1).collect::<Vec<_>>()))
}

/// `flashcp-trust` entry point; returns 0, 3 or 4.
pub fn trust_main() -> i32 {
    report("flashcp-trust", trust(&std::env::args_os().skip(1).collect::<Vec<_>>()))
}

fn require_root() -> Result<(), Fail> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Validation("must be started via pkexec".into()));
    }
    Ok(())
}

fn open_err(dev: &DevName, e: io::Error) -> Fail {
    let shown = dev.path();
    Validation(match e.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ENXIO) | Some(libc::ENODEV) => format!("{shown}: no such device"),
        Some(libc::EBUSY) => format!("{shown} is in use (mounted, or opened exclusively by another program)"),
        Some(libc::ENOMEDIUM) => format!("{shown} has no medium"),
        Some(libc::ELOOP) => format!("{shown} is not a device node"),
        _ => format!("{shown}: {e}"),
    })
}

/// Validate the opened device (`st` = fstat of its fd), read its disk's identity and require it
/// to be the drive the caller expects.
pub fn check_drive(
    roots: &Roots,
    st: NodeStat,
    dev: &DevName,
    require_idle: bool,
    expect: &str,
) -> Result<(ValidDevice, Identity), String> {
    let valid = validate_device(roots, st, dev, require_idle)?;
    let id = read_identity(roots, &valid).map_err(|e| format!("{}: {e}", dev.path()))?;
    if id.key != expect {
        return Err(format!("{} is not the expected drive (serial {:?}, expected {expect:?})", dev.path(), id.key));
    }
    Ok((valid, id))
}

/// The drive must match a trusted entry on all four fields.
/// Trust-time check: the drive the helper opened must be exactly the one the user was shown,
/// field by field, so a swap or re-enumeration between the dialog and the open can't enroll a
/// different drive that merely shares a serial.
pub fn same_drive(shown: &Identity, actual: &Identity) -> Result<(), String> {
    let diff = [
        ("serial", shown.key == actual.key),
        ("WWN", shown.wwn == actual.wwn),
        ("model", shown.model == actual.model),
        ("size", shown.size == actual.size),
    ];
    match diff.iter().find(|(_, same)| !same) {
        None => Ok(()),
        Some((what, _)) => Err(format!("the drive changed, please re-select it ({what} differs)")),
    }
}

pub fn require_trusted(entries: &[Identity], id: &Identity) -> Result<(), String> {
    if trust::matches(entries, id) {
        Ok(())
    } else {
        Err(format!(
            "drive not trusted: {} (serial {:?}, {} bytes) has not been trusted by an administrator",
            id.model, id.key, id.size
        ))
    }
}

/// What `--confirm` must equal for a build: the volume label from `flashcp info` if it
/// succeeded and reported a non-empty one, else the drive's identity key (serial).
pub fn expected_confirm(info_ok: bool, info_stdout: &[u8], key: &str) -> String {
    let label = || {
        let text = std::str::from_utf8(info_stdout).ok()?;
        let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s.trim()).ok();
        let v = parse(text).or_else(|| text.lines().find(|l| !l.trim().is_empty()).and_then(parse))?;
        v.get("label")?.as_str().filter(|l| !l.is_empty()).map(String::from)
    };
    info_ok.then(label).flatten().unwrap_or_else(|| key.to_string())
}

/// Run the flashcp side of a validated, trusted request. `base` carries program, env, creds and
/// the target fd; its args are replaced per run. `home` is the invoking user's passwd home
/// (only `optimize` uses it, as a string; see [`args::optimize_temp`]). Returns the first
/// non-zero flashcp exit code, or 0.
pub fn run_job(req: &Request, key: &str, home: &str, base: &ExecPlan) -> Result<i32, Fail> {
    let runs = req.flashcp_runs(home).map_err(Validation)?;
    let with = |args: Vec<String>| ExecPlan { args, ..base.clone() };
    let cannot = |e: io::Error| Internal(format!("cannot run {}: {e}", base.program.display()));
    if let Request::Build { confirm, .. } = req {
        let (st, out) = exec::run_capture(&with(args::info_args())).map_err(cannot)?;
        if *confirm != expected_confirm(st.success(), &out, key) {
            return Err(Validation("confirmation text does not match the drive's label (or serial number)".into()));
        }
    }
    for args in runs {
        if let Some(sig) = exec::take_pending_signal() {
            return Ok(128 + sig);
        }
        let code = exec::exit_code(exec::run(&with(args), Stdio::inherit()).map_err(cannot)?);
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

fn helper(argv: &[OsString]) -> Result<i32, Fail> {
    let req = args::parse(argv).map_err(Validation)?;
    require_root()?;
    let pkexec_uid = std::env::var("PKEXEC_UID").ok();
    let sudo_uid = std::env::var("SUDO_UID").ok();
    let uid = exec::invoking_uid(pkexec_uid.as_deref(), sudo_uid.as_deref()).map_err(Validation)?;
    let creds = exec::lookup_user(uid).map_err(Validation)?;
    // Refuse an unusable home (optimize only) before the device is even opened.
    req.flashcp_runs(&creds.home).map_err(Validation)?;

    // Open once; everything below is about this fd, and flashcp gets this fd.
    let roots = Roots::system();
    let fd = open_device(&roots, req.dev(), req.access()).map_err(|e| open_err(req.dev(), e))?;
    let st = fstat(fd.as_fd()).map_err(|e| Internal(format!("fstat: {e}")))?;
    let (_, id) = check_drive(&roots, st, req.dev(), true, req.expect()).map_err(Validation)?;
    let entries = TrustStore::system().load().map_err(Validation)?;
    require_trusted(&entries, &id).map_err(Validation)?;

    exec::block_signals().map_err(|e| Internal(format!("sigprocmask: {e}")))?;
    let home = creds.home.clone();
    let base = ExecPlan {
        program: exec::FLASHCP.into(),
        args: Vec::new(),
        env: exec::user_env(&creds.home),
        drop: Some(creds),
        target_fd: Some(fd.as_raw_fd()),
    };
    run_job(&req, &id.key, &home, &base)
}

fn trust(argv: &[OsString]) -> Result<i32, Fail> {
    let req = args::parse_trust(argv).map_err(Validation)?;
    require_root()?;
    let roots = Roots::system();
    let fd = open_device(&roots, &req.dev, args::Access::Read).map_err(|e| open_err(&req.dev, e))?;
    let st = fstat(fd.as_fd()).map_err(|e| Internal(format!("fstat: {e}")))?;
    // Not idle-checked: a mounted stick may be trusted. A disk carrying the system is refused.
    let valid = validate_device(&roots, st, &req.dev, false).map_err(Validation)?;
    let id = read_identity(&roots, &valid).map_err(|e| Validation(format!("{}: {e}", req.dev.path())))?;
    same_drive(&req.shown, &id).map_err(Validation)?;
    TrustStore::system().add(&id).map_err(Internal)?;
    json::print_line(&json::trusted_line(&id));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    #[test]
    fn confirm_rule() {
        let info = br#"{"label":"MY STICK","cluster":32768,"size":1,"free":1,"files":0,"dirs":0,"dirty":false}"#;
        assert_eq!(expected_confirm(true, info, "SER1"), "MY STICK");
        assert_eq!(expected_confirm(true, b"\n{\"label\":\"A\"}\n", "SER1"), "A");
        // Failure, empty label, no label, garbage, non-string label: the serial.
        assert_eq!(expected_confirm(false, info, "SER1"), "SER1");
        assert_eq!(expected_confirm(true, br#"{"label":""}"#, "SER1"), "SER1");
        assert_eq!(expected_confirm(true, br#"{"cluster":1}"#, "SER1"), "SER1");
        assert_eq!(expected_confirm(true, b"not json", "SER1"), "SER1");
        assert_eq!(expected_confirm(true, b"", "SER1"), "SER1");
        assert_eq!(expected_confirm(true, br#"{"label":7}"#, "SER1"), "SER1");
        assert_eq!(expected_confirm(true, b"\xff\xfe", "SER1"), "SER1");
    }

    #[test]
    fn untrusted_message_prefix() {
        let id = Identity { key: "S1".into(), wwn: "-".into(), model: "M".into(), size: 512 };
        let e = require_trusted(&[], &id).unwrap_err();
        assert!(json::error_line("flashcp-helper", &e).contains(r#""message":"flashcp-helper: drive not trusted"#));
        assert!(require_trusted(&[Identity { size: 1024, ..id.clone() }], &id).is_err());
        assert!(require_trusted(std::slice::from_ref(&id), &id).is_ok());
    }

    /// A fake tree with one USB disk sdb (8:16) + sdb1 (8:17), serial `SER1`.
    fn fake_roots(t: &Path) -> Roots {
        let roots = Roots {
            dev: t.join("dev"),
            sys: t.join("sys"),
            udev_data: t.join("udev"),
            proc_mountinfo: t.join("mountinfo"),
            proc_swaps: t.join("swaps"),
        };
        let disk =
            roots.sys.join("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb");
        for (dir, majmin) in [(disk.clone(), "8:16"), (disk.join("sdb1"), "8:17")] {
            fs::create_dir_all(dir.join("holders")).unwrap();
            fs::write(dir.join("dev"), majmin).unwrap();
            fs::write(dir.join("size"), "2048").unwrap();
            fs::create_dir_all(roots.sys.join("dev/block")).unwrap();
            std::os::unix::fs::symlink(&dir, roots.sys.join("dev/block").join(majmin)).unwrap();
        }
        fs::write(disk.join("sdb1/partition"), "1").unwrap();
        fs::create_dir_all(&roots.udev_data).unwrap();
        fs::write(roots.udev_data.join("b8:16"), "E:ID_BUS=usb\nE:ID_SERIAL_SHORT=SER1\nE:ID_MODEL=Stick\n").unwrap();
        fs::write(roots.udev_data.join("b8:17"), "E:ID_BUS=usb\n").unwrap();
        fs::write(&roots.proc_mountinfo, "").unwrap();
        fs::write(&roots.proc_swaps, "Filename\n").unwrap();
        roots
    }

    #[test]
    fn same_drive_requires_every_field() {
        let shown = Identity { key: "SER1".into(), wwn: "0x5001".into(), model: "Stick".into(), size: 1024 };
        assert!(same_drive(&shown, &shown.clone()).is_ok());
        let cases = [
            ("serial", Identity { key: "SER2".into(), ..shown.clone() }),
            ("WWN", Identity { wwn: "0x5002".into(), ..shown.clone() }),
            ("WWN", Identity { wwn: "-".into(), ..shown.clone() }),
            ("model", Identity { model: "Stick2".into(), ..shown.clone() }),
            ("size", Identity { size: 2048, ..shown.clone() }),
        ];
        for (what, actual) in cases {
            let e = same_drive(&shown, &actual).unwrap_err();
            assert!(e.starts_with("the drive changed, please re-select it"), "{e}");
            assert!(e.contains(what), "{e}");
        }
    }

    #[test]
    fn check_drive_reads_identity_and_requires_expect() {
        let t = tempfile::tempdir().unwrap();
        let roots = fake_roots(t.path());
        let st = NodeStat { is_block: true, is_symlink: false, major: 8, minor: 17 };
        let dev = args::parse_dev("/dev/sdb1").unwrap();
        let (_, id) = check_drive(&roots, st, &dev, true, "SER1").unwrap();
        assert_eq!(id, Identity { key: "SER1".into(), wwn: "-".into(), model: "Stick".into(), size: 2048 * 512 });
        assert!(check_drive(&roots, st, &dev, true, "SER2").unwrap_err().contains("not the expected drive"));
        // Untrustable (no serial, no WWN) is refused even with a matching-looking expect.
        fs::write(roots.udev_data.join("b8:16"), "E:ID_BUS=usb\n").unwrap();
        assert!(check_drive(&roots, st, &dev, true, "-").unwrap_err().contains("remember it safely"));
        // Trust store round trip: trusting what check_drive saw makes require_trusted pass.
        fs::write(roots.udev_data.join("b8:16"), "E:ID_BUS=usb\nE:ID_SERIAL_SHORT=SER1\nE:ID_MODEL=Stick\n").unwrap();
        let store = TrustStore { dir: t.path().join("etc-flashcp") };
        assert!(require_trusted(&store.load().unwrap(), &id).is_err());
        store.add(&id).unwrap();
        assert!(require_trusted(&store.load().unwrap(), &id).is_ok());
    }

    /// A fake flashcp: logs its argv to `log`, `info` prints `info_out` and exits `info_rc`,
    /// `add` of a source containing "fail" exits 9, everything else prints a done event.
    fn fake_flashcp(dir: &Path, info_out: &str, info_rc: i32) -> (PathBuf, PathBuf) {
        let log = dir.join("log");
        let prog = dir.join("flashcp");
        fs::write(
            &prog,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{log}'\n\
                 case \"$1\" in\n\
                 info) printf '%s\\n' '{info_out}'; exit {info_rc} ;;\n\
                 add) case \"$2\" in *fail*) exit 9 ;; esac ;;\n\
                 esac\necho '{{\"event\":\"done\"}}'\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&prog, fs::Permissions::from_mode(0o755)).unwrap();
        (prog, log)
    }

    fn job(dir: &Path, prog: &Path, a: &[&str]) -> Result<i32, Fail> {
        job_home(dir, prog, a, &dir.join("home").display().to_string())
    }

    fn job_home(dir: &Path, prog: &Path, a: &[&str], home: &str) -> Result<i32, Fail> {
        let req = args::parse(&a.iter().map(OsString::from).collect::<Vec<_>>()).unwrap();
        let target = fs::File::create(dir.join("target")).unwrap();
        let base = ExecPlan {
            program: prog.into(),
            args: vec![],
            drop: None,
            env: exec::user_env("/nonexistent"),
            target_fd: Some(target.as_raw_fd()),
        };
        run_job(&req, "SER1", home, &base)
    }

    fn log_lines(log: &Path) -> Vec<String> {
        fs::read_to_string(log).unwrap_or_default().lines().map(String::from).collect()
    }

    #[test]
    fn build_confirm_checked_against_label_then_serial() {
        let t = tempfile::tempdir().unwrap();
        let (prog, log) = fake_flashcp(t.path(), r#"{"label":"MY STICK"}"#, 0);
        let build = |c: &str| job(t.path(), &prog, &["build", "/dev/sdb", "--expect", "SER1", "--confirm", c, "/src"]);
        assert!(matches!(build("SER1"), Err(Validation(m)) if m.contains("confirmation")));
        assert_eq!(log_lines(&log), ["info --target-fd 3 --json"], "build must not run after a mismatch");
        assert_eq!(build("MY STICK"), Ok(0));
        assert_eq!(log_lines(&log)[2], "build /src --target-fd 3 --yes-erase --json");

        // info fails (not exFAT): the serial is expected.
        let t = tempfile::tempdir().unwrap();
        let (prog, log) = fake_flashcp(t.path(), r#"{"label":"MY STICK"}"#, 1);
        let build = |c: &str| job(t.path(), &prog, &["build", "/dev/sdb", "--expect", "SER1", "--confirm", c, "/src"]);
        assert!(build("MY STICK").is_err());
        assert_eq!(build("SER1"), Ok(0));
        assert_eq!(log_lines(&log).len(), 3);
    }

    #[test]
    fn add_runs_each_item_and_stops_at_first_failure() {
        let t = tempfile::tempdir().unwrap();
        let (prog, log) = fake_flashcp(t.path(), "{}", 0);
        let a = ["add", "/dev/sdb1", "--expect", "SER1", "--skip-existing"];
        let items = ["--item", "/a", "/A", "--item", "/fail", "/B", "--item", "/c", "/C"];
        assert_eq!(job(t.path(), &prog, &[&a[..], &items].concat()), Ok(9));
        assert_eq!(
            log_lines(&log),
            [
                "add /a --target-fd 3 --to /A --skip-existing --json",
                "add /fail --target-fd 3 --to /B --skip-existing --json"
            ]
        );
        let _ = fs::remove_file(&log);
        assert_eq!(
            job(t.path(), &prog, &["add", "/dev/sdb1", "--expect", "SER1", "--item", "/a", "/", "--item", "/c", "/"]),
            Ok(0)
        );
        assert_eq!(log_lines(&log).len(), 2);
        assert_eq!(job(t.path(), &prog, &["info", "/dev/sdb1", "--expect", "SER1"]), Ok(0));
        assert_eq!(log_lines(&log)[2], "info --target-fd 3 --json");
    }

    #[test]
    fn pull_runs_flashcp_once_with_the_fixed_argv() {
        let t = tempfile::tempdir().unwrap();
        let (prog, log) = fake_flashcp(t.path(), "{}", 0);
        let a = ["pull", "/dev/sdb1", "--expect", "SER1", "--from", "DCIM", "--overwrite", "/home/u/Downloads/S"];
        assert_eq!(job(t.path(), &prog, &a), Ok(0));
        assert_eq!(log_lines(&log), ["pull --target-fd 3 /home/u/Downloads/S --from DCIM --overwrite --json"]);
        // The helper itself never creates or inspects the destination.
        assert!(!Path::new("/home/u/Downloads/S").exists());
    }

    #[test]
    fn optimize_runs_once_without_confirm_and_never_touches_the_temp_dir() {
        let t = tempfile::tempdir().unwrap();
        let (prog, log) = fake_flashcp(t.path(), r#"{"label":"MY STICK"}"#, 0);
        let home = t.path().join("home");
        let temp = home.join(".cache/flashcp/optimize");
        assert_eq!(job(t.path(), &prog, &["optimize", "/dev/sdb1", "--expect", "SER1", "--dry-run"]), Ok(0));
        assert_eq!(job(t.path(), &prog, &["optimize", "/dev/sdb1", "--expect", "SER1"]), Ok(0));
        // Again, as a resume after an interruption: the very same argv.
        assert_eq!(job(t.path(), &prog, &["optimize", "/dev/sdb1", "--expect", "SER1"]), Ok(0));
        let t3 = temp.display();
        assert_eq!(
            log_lines(&log),
            [
                format!("optimize --target-fd 3 --temp {t3} --yes --dry-run --json"),
                format!("optimize --target-fd 3 --temp {t3} --yes --json"),
                format!("optimize --target-fd 3 --temp {t3} --yes --json"),
            ],
            "no info/confirm run, one flashcp run per request"
        );
        // The helper (here: the test process standing in for root) created nothing on the user's
        // side: neither the temp dir nor any of its parents. flashcp does that as the user.
        assert!(!home.exists(), "the helper must not create the user's cache directory");
    }

    #[test]
    fn optimize_with_an_unusable_home_runs_nothing() {
        let t = tempfile::tempdir().unwrap();
        let (prog, log) = fake_flashcp(t.path(), "{}", 0);
        for home in ["/", "", "relative/home", "/home/../root"] {
            let r = job_home(t.path(), &prog, &["optimize", "/dev/sdb1", "--expect", "SER1"], home);
            assert!(matches!(&r, Err(Validation(m)) if m.contains("home directory")), "{home:?}: {r:?}");
        }
        assert!(log_lines(&log).is_empty());
        // Other ops do not depend on the home at all.
        assert_eq!(job_home(t.path(), &prog, &["info", "/dev/sdb1", "--expect", "SER1"], "/"), Ok(0));
    }

    #[test]
    fn missing_flashcp_is_internal() {
        let t = tempfile::tempdir().unwrap();
        let r = job(t.path(), Path::new("/nonexistent/flashcp"), &["info", "/dev/sdb1", "--expect", "SER1"]);
        assert!(matches!(r, Err(Internal(_))));
    }

    #[test]
    fn refuses_without_root() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let a: Vec<OsString> = ["info", "/dev/sdb", "--expect", "S"].iter().map(OsString::from).collect();
        assert!(matches!(helper(&a), Err(Validation(m)) if m.contains("pkexec")));
        let a: Vec<OsString> = ["/dev/sdb", "--expect", "S", "--wwn", "-", "--model", "M", "--size", "512"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(matches!(trust(&a), Err(Validation(m)) if m.contains("pkexec")));
        // Bad argv is reported before the root check.
        assert!(matches!(trust(&[]), Err(Validation(m)) if m.contains("usage")));
    }
}
