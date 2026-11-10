//! Integration tests: the real `flashcp drived` against image files, with udev events fed
//! through a FIFO and fake udisksctl / gdbus / notify-send that log their calls.
//!
//! The driver is `tests/daemon_fixtures/fake-mount` (a stand-in that speaks the
//! docs/drive-mode-interface.md contract without FUSE) unless FLASHCP_DRIVE_TESTS_REAL=1, which
//! uses this build's own `flashcp mount` over real FUSE (needs /dev/fuse, fusermount3 and
//! mkfs.exfat) and also checks the data that went through the mount.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn real() -> bool {
    std::env::var("FLASHCP_DRIVE_TESTS_REAL").is_ok_and(|v| v == "1")
}

struct Rig {
    dir: PathBuf,
    daemon: Child,
    events: File,
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Rig {
    fn new(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("flashcp-drived-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for d in ["run", "cfg", "media", "bin"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        let log = |n: &str| dir.join(format!("{n}.log")).display().to_string();
        script(
            &dir.join("bin/udisksctl"),
            &format!(
                "echo \"$@\" >> {}\ncase \"$1\" in mount) mkdir -p {m}/kernel; echo \"Mounted $3 at {m}/kernel.\";; esac",
                log("udisksctl"),
                m = dir.join("media").display()
            ),
        );
        script(&dir.join("bin/gdbus"), &format!("echo \"$@\" >> {}", log("gdbus")));
        script(&dir.join("bin/notify-send"), &format!("printf '%s|' \"$@\" >> {}; echo >> {0}; echo 7", log("notify")));
        let fifo = dir.join("events");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let flashcp = if real() {
            PathBuf::from(env!("CARGO_BIN_EXE_flashcp"))
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/daemon_fixtures/fake-mount")
        };
        let daemon = Command::new(env!("CARGO_BIN_EXE_flashcp"))
            .arg("drived")
            .envs(rig_env(&dir))
            .env("FLASHCP_DRIVED_FLASHCP", flashcp)
            .env("FLASHCP_DRIVED_EVENTS", &fifo)
            .env("FLASHCP_DRIVED_MEDIA", dir.join("media"))
            .env("FLASHCP_DRIVED_UDISKSCTL", dir.join("bin/udisksctl"))
            .env("FLASHCP_DRIVED_GDBUS", dir.join("bin/gdbus"))
            .env("FLASHCP_DRIVED_NOTIFY", dir.join("bin/notify-send"))
            .env("FLASHCP_DRIVED_NO_LOGIN1", "1")
            .stderr(File::create(dir.join("daemon.log")).unwrap())
            .spawn()
            .unwrap();
        let events = OpenOptions::new().write(true).open(&fifo).unwrap();
        let r = Rig { dir, daemon, events };
        r.wait("the daemon's socket", || r.dir.join("run/flashcp/drived.sock").exists());
        r
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join(format!("{name}.log"))).unwrap_or_default()
    }

    fn wait(&self, what: &str, mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "timed out waiting for {what}\n--- daemon.log\n{}\n--- status\n{}",
                self.log("daemon"),
                self.status()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A 64 MB image as a USB drive: exFAT (mkfs.exfat for the real driver) or zeros.
    fn plug(&mut self, name: &str, exfat: bool, mode: Option<&str>) -> String {
        let img = self.dir.join(format!("{name}.img"));
        let f = File::create(&img).unwrap();
        f.set_len(64 << 20).unwrap();
        if exfat && real() {
            assert!(Command::new("mkfs.exfat").args(["-L", "STICK"]).arg(&img).output().unwrap().status.success());
        } else if exfat {
            use std::os::unix::fs::FileExt;
            f.write_at(b"\xeb\x76\x90EXFAT   ", 0).unwrap();
        }
        if let Some(m) = mode {
            fs::write(img.with_extension("img.mode"), m).unwrap();
        }
        let dev = img.display().to_string();
        let block = format!(
            "ACTION=add\nDEVNAME={dev}\nDEVTYPE=disk\nID_FS_LABEL=STICK\nID_FS_UUID=1A2B-3C4D\nFLASHCP_DRIVE_MODE=1\nFLASHCP_DRIVE_UID={}\n\n",
            unsafe { libc_getuid() }
        );
        self.events.write_all(block.as_bytes()).unwrap();
        dev
    }

    fn unplug(&mut self, dev: &str) {
        self.events.write_all(format!("ACTION=remove\nDEVNAME={dev}\n\n").as_bytes()).unwrap();
    }

    fn cli(&self, args: &[&str]) -> String {
        let o = Command::new(env!("CARGO_BIN_EXE_flashcp")).args(args).envs(rig_env(&self.dir)).output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn status(&self) -> String {
        self.cli(&["drive-mode", "status", "--json"])
    }

    fn state_is(&self, state: &str) -> impl FnMut() -> bool + '_ {
        let want = format!("\"state\":\"{state}\"");
        move || self.status().contains(&want)
    }

    /// The driver's control socket (the test-only `fake-dirty` and `status` requests).
    fn control(&self, dev: &str, cmd: &str) -> String {
        let name = Path::new(dev).file_name().unwrap().to_string_lossy();
        let mut s = UnixStream::connect(self.dir.join(format!("run/flashcp/{name}.sock"))).unwrap();
        s.write_all(format!("{cmd}\n").as_bytes()).unwrap();
        let mut l = String::new();
        BufReader::new(s).read_line(&mut l).unwrap();
        l
    }

    /// SIGKILL the driver serving `mp` (found by its command line).
    fn kill_driver(&self, mp: &str) {
        for e in fs::read_dir("/proc").unwrap().flatten() {
            let cmd = fs::read(e.path().join("cmdline")).unwrap_or_default();
            let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
            if args.contains(&&b"mount"[..]) && args.contains(&mp.as_bytes()) {
                let pid: i32 = e.file_name().to_string_lossy().parse().unwrap();
                unsafe { libc_kill(pid, 9) };
            }
        }
    }

    /// SIGTERM the daemon and expect a clean exit.
    fn stop(mut self) {
        unsafe { libc_kill(self.daemon.id() as i32, 15) };
        let t = Instant::now();
        loop {
            if let Some(s) = self.daemon.try_wait().unwrap() {
                assert!(s.success(), "daemon exit {s:?}\n{}", self.log("daemon"));
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(20), "daemon did not exit\n{}", self.log("daemon"));
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!self.dir.join("run/flashcp/drive-mode-on").exists(), "the udev marker outlives the daemon");
        let _ = fs::remove_dir_all(&self.dir);
    }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn rig_env(dir: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![("XDG_RUNTIME_DIR", dir.join("run")), ("XDG_CONFIG_HOME", dir.join("cfg"))]
}

const MP: &str = "media/STICK";

/// Plug in, served by flashcp; data pending; eject from the CLI: prepare-eject → unmount →
/// exit 0 → power off; the mountpoint is gone and the data is on the image.
#[test]
fn plug_write_eject_power_off() {
    let mut r = Rig::new("eject");
    let dev = r.plug("a", true, None);
    r.wait("ready", r.state_is("ready"));
    assert!(r.dir.join("run/flashcp/drive-mode-on").exists());
    let mp = r.dir.join(MP);
    if real() {
        fs::write(mp.join("hello.txt"), b"hello drive mode").unwrap();
    } else {
        r.control(&dev, "fake-dirty 5000000");
        r.wait("saving", r.state_is("saving"));
    }
    let out = r.cli(&["drive-mode", "eject", &dev, "--json"]);
    assert!(out.contains("\"ok\":true"), "{out}");
    r.wait("safe to remove", r.state_is("safe_to_remove"));
    r.wait("power-off", || r.log("udisksctl").contains(&format!("power-off -b {dev}")));
    r.wait("the mountpoint removed", || !mp.exists());
    r.wait("the 'can be removed' notification", || r.log("notify").contains("can be removed"));
    if real() {
        let o = Command::new(env!("CARGO_BIN_EXE_flashcp")).args(["info", &dev, "--json"]).output().unwrap();
        assert!(String::from_utf8_lossy(&o.stdout).contains("\"files\":1"), "{}", String::from_utf8_lossy(&o.stdout));
        let fsck = Command::new("fsck.exfat").args(["-n", &dev]).output().unwrap();
        assert!(fsck.status.success(), "{}", String::from_utf8_lossy(&fsck.stdout));
    } else {
        assert!(r.log("notify").contains("Saving 5.0 MB to STICK"), "{}", r.log("notify"));
    }
    r.stop();
}

/// A volume `--check` refuses goes to the kernel driver: fallback flag, udisks Rescan, mount.
#[test]
fn refused_volume_gets_the_kernel_driver() {
    let mut r = Rig::new("refused");
    let dev = r.plug("z", false, None);
    r.wait("kernel mode", r.state_is("kernel"));
    assert!(r.dir.join("run/flashcp/kernel/1A2B-3C4D").exists(), "fallback flag for the udev rule");
    assert!(r.log("gdbus").contains("Block.Rescan"), "{}", r.log("gdbus"));
    assert!(r.log("udisksctl").contains(&format!("mount -b {dev}")), "{}", r.log("udisksctl"));
    r.stop();
}

/// The driver crashes: restarted once (recovery), the second crash falls back to the kernel
/// driver with a notification.
#[test]
fn crash_restart_once_then_kernel() {
    let mut r = Rig::new("crash");
    let dev = r.plug("c", true, None);
    r.wait("ready", r.state_is("ready"));
    let mp = r.dir.join(MP).display().to_string();
    r.kill_driver(&mp);
    r.wait("restarted", || {
        r.log("daemon").matches("-> start ").count() >= 2 && r.status().contains("\"state\":\"ready\"")
    });
    r.kill_driver(&mp);
    r.wait("kernel mode", r.state_is("kernel"));
    assert!(r.log("notify").contains("fast mode stopped"), "{}", r.log("notify"));
    assert!(r.log("udisksctl").contains(&format!("mount -b {dev}")));
    r.stop();
}

/// The off switch: the driver is flushed and stopped, the drive remounted by the kernel driver,
/// the setting persists, and the udev marker is removed.
#[test]
fn off_switch_remounts_with_the_kernel_driver() {
    let mut r = Rig::new("off");
    let dev = r.plug("o", true, None);
    r.wait("ready", r.state_is("ready"));
    let out = r.cli(&["drive-mode", "off", "--json"]);
    assert!(out.contains("\"enabled\":false") && out.contains("\"running\":true"), "{out}");
    r.wait("kernel mode", r.state_is("kernel"));
    assert!(r.log("udisksctl").contains(&format!("mount -b {dev}")));
    assert!(!r.dir.join("run/flashcp/drive-mode-on").exists());
    assert!(fs::read_to_string(r.dir.join("cfg/flashcp/drive-mode.conf")).unwrap().contains("enabled=false"));
    assert!(r.status().contains("\"enabled\":false"));
    r.cli(&["drive-mode", "on"]);
    r.wait("marker back", || r.dir.join("run/flashcp/drive-mode-on").exists());
    r.stop();
}

/// Yanked while data is pending: a clear warning; busy devices are retried, never kernel-mounted.
#[test]
fn yank_while_dirty_warns_and_busy_never_falls_back() {
    if real() {
        return; // needs the stand-in's fake-dirty and busy modes
    }
    let mut r = Rig::new("yank");
    let dev = r.plug("y", true, None);
    r.wait("ready", r.state_is("ready"));
    r.control(&dev, "fake-dirty 12000000");
    r.wait("saving", r.state_is("saving"));
    r.unplug(&dev);
    r.wait("the warning", || r.log("notify").contains("removed too early"));
    assert!(r.log("notify").contains("12 MB"), "{}", r.log("notify"));
    r.wait("forgotten", || !r.status().contains(&dev));
    let busy = r.plug("b", true, Some("busy"));
    r.wait("busy", r.state_is("busy"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!r.log("udisksctl").contains(&busy), "a busy device was mounted: {}", r.log("udisksctl"));
    r.stop();
}
