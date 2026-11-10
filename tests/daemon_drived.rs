//! Integration tests: the real `flashcp drived` against image files, with udev events fed
//! through a FIFO and fake udisksctl / gdbus / notify-send that log their calls.
//!
//! `gdbus` is a fake too: it answers `GetCapabilities` and `Notify` of
//! org.freedesktop.Notifications and, as `gdbus monitor --session`, reports the button the test
//! writes to `answer`. `flashcp recluster` is always this build's real one, on image files.
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
        // udisksctl mount/unmount also keep a fake mount table (FLASHCP_DRIVED_MOUNTINFO).
        let mi = dir.join("mountinfo").display().to_string();
        fs::write(&mi, "").unwrap();
        script(
            &dir.join("bin/udisksctl"),
            &format!(
                "echo \"$@\" >> {}\ncase \"$1\" in\n\
                 mount) mkdir -p {m}/kernel; echo \"36 25 8:17 / {m}/kernel rw - exfat $3 rw\" >> {mi}; echo \"Mounted $3 at {m}/kernel.\";;\n\
                 unmount) grep -v \" $3 rw$\" {mi} > {mi}.tmp; mv {mi}.tmp {mi};;\nesac",
                log("udisksctl"),
                m = dir.join("media").display()
            ),
        );
        let d = dir.display();
        script(
            &dir.join("bin/gdbus"),
            &format!(
                "echo \"$@\" >> {log}\ncase \"$1 $2\" in\n\
                 'monitor --session') echo 'Monitoring signals on object /org/freedesktop/Notifications owned by :1.9'\n\
                   while [ ! -f {d}/answer ]; do sleep 0.05; done; a=$(cat {d}/answer); rm -f {d}/answer\n\
                   case $a in closed) echo \"/org/freedesktop/Notifications: org.freedesktop.Notifications.NotificationClosed (uint32 77, uint32 2)\";;\n\
                   *) echo \"/org/freedesktop/Notifications: org.freedesktop.Notifications.ActionInvoked (uint32 77, '$a')\";; esac\n\
                   exec sleep 30;;\n\
                 'call --session') case \"$*\" in\n\
                   *GetCapabilities*) if [ -f {d}/no-actions ]; then echo \"(['body'],)\"; else echo \"(['actions', 'body'],)\"; fi;;\n\
                   *.Notify*) echo '(uint32 77,)';; esac;;\nesac",
                log = log("gdbus")
            ),
        );
        script(&dir.join("bin/notify-send"), &format!("printf '%s|' \"$@\" >> {}; echo >> {0}; echo 7", log("notify")));
        let fifo = dir.join("events");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let (daemon, events) = spawn_daemon(&dir);
        let r = Rig { dir, daemon, events };
        r.wait("the daemon's socket", || r.dir.join("run/flashcp/drived.sock").exists());
        r
    }

    /// Stop the daemon (SIGTERM, a clean exit) and start a new one on the same directories.
    fn restart(&mut self) {
        unsafe { libc_kill(self.daemon.id() as i32, 15) };
        let st = self.daemon.wait().unwrap();
        assert!(st.success(), "daemon exit {st:?}\n{}", self.log("daemon"));
        (self.daemon, self.events) = spawn_daemon(&self.dir);
        self.wait("the new daemon's socket", || UnixStream::connect(self.dir.join("run/flashcp/drived.sock")).is_ok());
    }
}

fn spawn_daemon(dir: &Path) -> (Child, File) {
    let fifo = dir.join("events");
    let flashcp = if real() {
        PathBuf::from(env!("CARGO_BIN_EXE_flashcp"))
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/daemon_fixtures/fake-mount")
    };
    let log = OpenOptions::new().create(true).append(true).open(dir.join("daemon.log")).unwrap();
    let daemon = Command::new(env!("CARGO_BIN_EXE_flashcp"))
        .arg("drived")
        .envs(rig_env(dir))
        .env("FLASHCP_DRIVED_FLASHCP", flashcp)
        .env("FLASHCP_DRIVED_EVENTS", &fifo)
        .env("FLASHCP_DRIVED_MEDIA", dir.join("media"))
        .env("FLASHCP_DRIVED_UDISKSCTL", dir.join("bin/udisksctl"))
        .env("FLASHCP_DRIVED_GDBUS", dir.join("bin/gdbus"))
        .env("FLASHCP_DRIVED_NOTIFY", dir.join("bin/notify-send"))
        .env("FLASHCP_DRIVED_NO_LOGIN1", "1")
        .stderr(log)
        .spawn()
        .unwrap();
    let events = OpenOptions::new().write(true).open(&fifo).unwrap();
    (daemon, events)
}

impl Rig {
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
        self.announce(&img)
    }

    /// A real exFAT image with 32 KB clusters (built by `flashcp build`), not plugged in yet.
    fn big_cluster_image(&self, name: &str) -> PathBuf {
        let src = self.dir.join(format!("{name}-src"));
        fs::create_dir_all(src.join("docs")).unwrap();
        for i in 0..20 {
            fs::write(src.join(format!("docs/note{i}.txt")), format!("note {i}\n").repeat(50)).unwrap();
        }
        let img = self.dir.join(format!("{name}.img"));
        File::create(&img).unwrap().set_len(64 << 20).unwrap();
        let o = Command::new(env!("CARGO_BIN_EXE_flashcp"))
            .arg("build")
            .arg(&src)
            .arg(&img)
            .args(["--yes-erase", "--cluster", "32768"])
            .output()
            .unwrap();
        assert!(o.status.success(), "flashcp build: {}", String::from_utf8_lossy(&o.stderr));
        img
    }

    /// The udev add event for an image, with its volume serial as `ID_FS_UUID` (as blkid
    /// prints it); 1A2B-3C4D for the stand-in images, which have none.
    fn announce(&mut self, img: &Path) -> String {
        let mut b = [0u8; 104];
        use std::io::Read;
        let _ = File::open(img).unwrap().read_exact(&mut b);
        let serial = u32::from_le_bytes([b[100], b[101], b[102], b[103]]);
        let uuid =
            if serial == 0 { "1A2B-3C4D".to_string() } else { format!("{:04X}-{:04X}", serial >> 16, serial & 0xffff) };
        self.announce_as(img, &uuid)
    }

    fn announce_as(&mut self, img: &Path, uuid: &str) -> String {
        let dev = img.display().to_string();
        let block = format!(
            "ACTION=add\nDEVNAME={dev}\nDEVTYPE=disk\nID_FS_LABEL=STICK\nID_FS_UUID={uuid}\nFLASHCP_DRIVE_MODE=1\nFLASHCP_DRIVE_UID={}\n\n",
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

impl Drop for Rig {
    /// A failed test must not leave its daemon behind (it holds the test harness's pipes).
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn rig_env(dir: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("XDG_RUNTIME_DIR", dir.join("run")),
        ("XDG_CONFIG_HOME", dir.join("cfg")),
        ("XDG_STATE_HOME", dir.join("state")),
        ("FLASHCP_DRIVED_MOUNTINFO", dir.join("mountinfo")),
    ]
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
/// driver READ-ONLY (it did not end cleanly) with notifications.
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
    r.wait("kernel mode, read-only", r.state_is("read_only"));
    assert!(r.log("notify").contains("fast mode stopped"), "{}", r.log("notify"));
    assert!(r.log("notify").contains("couldn't finish saving"), "{}", r.log("notify"));
    assert!(r.log("udisksctl").contains(&format!("mount -b {dev} --no-user-interaction -o ro")));
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

/// `flashcp recluster --check --json IMG`, the one line.
fn recluster_check(img: &Path) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_flashcp")).args(["recluster", "--check", "--json"]).arg(img).output();
    String::from_utf8_lossy(&o.unwrap().stdout).trim().to_string()
}

/// The offer: one notification with Accept / Not now; Accept pauses and stops the driver
/// cleanly, runs the real `flashcp recluster` on the image, mounts it again and says so. The
/// answer is remembered; the stick is not offered again.
#[test]
fn offer_accept_converts_the_image_and_remounts() {
    let mut r = Rig::new("offer");
    let img = r.big_cluster_image("k");
    assert!(recluster_check(&img).contains("\"cluster\":32768"), "{}", recluster_check(&img));
    let dev = r.announce(&img);
    r.wait("ready", r.state_is("ready"));
    r.wait("the offer", || r.log("gdbus").contains(".Notify "));
    let g = r.log("gdbus");
    assert!(g.contains("GetCapabilities"), "{g}");
    assert!(g.contains("'Make STICK faster for small files?'"), "{g}");
    assert!(g.contains("Nothing is erased") && g.contains("must not unplug"), "{g}");
    assert!(g.contains("['accept', 'Accept', 'notnow', 'Not now']"), "{g}");
    let st = r.status();
    assert!(st.contains("\"can_convert\":true") && st.contains("\"cluster\":32768"), "{st}");
    let text = r.cli(&["drive-mode", "status"]);
    assert!(text.contains("can be made faster") && text.contains("offer to make drives faster: on"), "{text}");

    fs::write(r.dir.join("answer"), "accept").unwrap();
    r.wait("the result", || r.log("notify").contains("STICK is ready"));
    r.wait("ready again", r.state_is("ready"));
    let n = r.log("notify");
    assert!(n.contains("Making STICK faster…|Don't unplug it."), "{n}");
    assert!(n.contains("Small files now copy to it much faster."), "{n}");
    assert!(!n.contains("repaired"), "a clean stop is no recovery: {n}");
    let st = r.status();
    assert!(st.contains("\"convert_result\":\"done\"") && st.contains("\"can_convert\":false"), "{st}");
    assert!(st.contains(&format!("\"mountpoint\":\"{}\"", r.dir.join(MP).display())), "same mountpoint: {st}");
    // The daemon's log, not the stand-in's `.starts` file: this also runs with the real driver.
    assert_eq!(r.log("daemon").matches("-> start ").count(), 2, "flashcp mounted it again: {}", r.log("daemon"));
    let c = recluster_check(&img);
    assert!(c.contains("\"cluster\":4096") && c.contains("\"finish\":false"), "{c}");
    // Written when the answer came, before the conversion started (so before "ready again").
    let saved = fs::read_to_string(r.dir.join("state/flashcp/recluster-offers")).unwrap();
    assert!(saved.ends_with("::STICK\taccepted\n") && saved.lines().count() == 1, "{saved}");
    assert!(r.log("gdbus").contains("CloseNotification 77"), "{}", r.log("gdbus"));

    // Plugged in again: no second offer (and the check says no anyway).
    r.unplug(&dev);
    r.wait("forgotten", || !r.status().contains(&dev));
    r.announce(&img);
    r.wait("ready", r.state_is("ready"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(r.log("gdbus").matches(".Notify ").count(), 1, "{}", r.log("gdbus"));
    r.stop();
}

/// The same path from the CLI, with the offer turned off and a notification server without
/// buttons: no notification at all, `flashcp drive-mode recluster DEV --yes` does it and prints
/// the result; a second one is refused in plain words.
#[test]
fn cli_recluster_and_the_offer_switch() {
    let mut r = Rig::new("cli");
    fs::write(r.dir.join("no-actions"), "").unwrap();
    let out = r.cli(&["drive-mode", "offer", "off", "--json"]);
    assert!(out.contains("\"offers\":false"), "{out}");
    assert!(fs::read_to_string(r.dir.join("cfg/flashcp/drive-mode.conf")).unwrap().contains("offer_faster=false"));
    assert!(r.cli(&["drive-mode", "status"]).contains("offer to make drives faster: off"));
    let img = r.big_cluster_image("c");
    let dev = r.announce(&img);
    r.wait("ready", r.state_is("ready"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!r.log("gdbus").contains("Notifications"), "offer off: {}", r.log("gdbus"));

    // Not a terminal and no --yes: nothing happens.
    let o = Command::new(env!("CARGO_BIN_EXE_flashcp"))
        .args(["drive-mode", "recluster", &dev])
        .envs(rig_env(&r.dir))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("--yes"));
    assert!(recluster_check(&img).contains("\"cluster\":32768"));

    let o = Command::new(env!("CARGO_BIN_EXE_flashcp"))
        .args(["drive-mode", "recluster", &dev, "--yes"])
        .envs(rig_env(&r.dir))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{text}\n{}\n{}", String::from_utf8_lossy(&o.stderr), r.log("daemon"));
    assert!(text.contains("Make STICK faster for small files?") && text.contains("Windows"), "{text}");
    assert!(text.contains("STICK is ready. Small files now copy to it much faster."), "{text}");
    assert!(recluster_check(&img).contains("\"cluster\":4096"));
    r.wait("ready", r.state_is("ready"));

    let o = Command::new(env!("CARGO_BIN_EXE_flashcp"))
        .args(["drive-mode", "recluster", &dev, "--yes", "--json"])
        .envs(rig_env(&r.dir))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(3), "{text}");
    assert!(text.contains("\"ok\":false") && text.contains("would not get faster"), "{text}");
    r.stop();
}

/// A conversion that lost power between its last two writes (the state Windows does not open):
/// on plug-in the daemon completes it before mounting, says so once, and mounts it.
#[test]
fn interrupted_conversion_is_completed_on_plug_in() {
    let mut r = Rig::new("finish");
    let img = r.big_cluster_image("f");
    // The crash window: the new main boot sector, the backup region still the old one.
    use std::os::unix::fs::FileExt;
    let f = OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut old = [0u8; 512];
    f.read_exact_at(&mut old, 0).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_flashcp")).arg("recluster").arg(&img).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    f.write_all_at(&old, 12 * 512).unwrap();
    drop(f);
    assert!(recluster_check(&img).contains("\"finish\":true"), "{}", recluster_check(&img));

    r.announce(&img);
    r.wait("ready", r.state_is("ready"));
    let n = r.log("notify");
    assert_eq!(n.matches("was repaired").count(), 1, "{n}");
    assert!(n.contains("STICK was repaired|A change to make it faster had been interrupted"), "{n}");
    let c = recluster_check(&img);
    assert!(c.contains("\"finish\":false") && c.contains("\"cluster\":4096"), "{c}");
    assert!(!r.log("gdbus").contains(".Notify "), "no offer for a converted stick");
    r.stop();
}

/// Two small bugs: `drive-mode status | head -1` must not panic on the closed pipe, and a
/// kernel mount unmounted by hand must not be listed as mounted.
#[test]
fn status_survives_a_closed_pipe_and_sees_a_manual_unmount() {
    let mut r = Rig::new("status");
    let dev = r.plug("u", false, None);
    r.wait("kernel mode", r.state_is("kernel"));
    let at = format!(" at {}", r.dir.join("media/kernel").display());
    assert!(r.cli(&["drive-mode", "status"]).contains(&at), "{}", r.cli(&["drive-mode", "status"]));
    // `udisksctl unmount -b DEV` by hand (the fake drops it from the mount table).
    let o = Command::new(r.dir.join("bin/udisksctl")).args(["unmount", "-b", &dev]).output().unwrap();
    assert!(o.status.success());
    r.wait("unmounted", r.state_is("unmounted"));
    let text = r.cli(&["drive-mode", "status"]);
    assert!(text.contains("none (unmounted)") && !text.contains(&at), "{text}");

    for args in [&["drive-mode", "status"][..], &["drive-mode", "status", "--json"]] {
        let (rd, wr) = std::io::pipe().unwrap();
        drop(rd);
        let o =
            Command::new(env!("CARGO_BIN_EXE_flashcp")).args(args).envs(rig_env(&r.dir)).stdout(wr).output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(o.status.success() && !err.contains("panicked") && err.is_empty(), "{args:?}: {:?} {err}", o.status);
    }
    r.stop();
}

/// The daemon's control socket, one request.
fn daemon_request(r: &Rig, line: &str) -> String {
    let mut s = UnixStream::connect(r.dir.join("run/flashcp/drived.sock")).unwrap();
    s.write_all(format!("{line}\n").as_bytes()).unwrap();
    let mut l = String::new();
    BufReader::new(s).read_line(&mut l).unwrap();
    l
}

/// A string field of the (one drive's) status.
fn field_of(status: &str, key: &str) -> String {
    let pat = format!("\"{key}\":\"");
    let rest = &status[status.find(&pat).unwrap_or_else(|| panic!("no {key} in {status}")) + pat.len()..];
    rest[..rest.find('"').unwrap()].to_string()
}

fn plug_of(status: &str) -> String {
    field_of(status, "plug")
}

/// Stick A is unplugged and stick B arrives at the same path: a confirmation made for A (the
/// app's `--plug`, a raw `recluster DEV PLUG`) is refused for B and nothing is written to B.
#[test]
fn a_confirmation_for_a_replaced_stick_is_refused() {
    let mut r = Rig::new("replaced");
    r.cli(&["drive-mode", "offer", "off"]);
    let img = r.big_cluster_image("a");
    let dev = r.announce(&img);
    r.wait("A ready", r.state_is("ready"));
    let a_plug = plug_of(&r.status());
    let a_id = field_of(&r.status(), "identity");
    r.unplug(&dev);
    r.wait("A forgotten", || !r.status().contains(&dev));
    // B: another stick (a new volume serial), the same device path.
    let b = r.big_cluster_image("b");
    fs::rename(&b, &img).unwrap();
    r.announce(&img);
    r.wait("B ready", r.state_is("ready"));
    assert_ne!(plug_of(&r.status()), a_plug);
    let before = fs::read(&img).unwrap();

    let reply = daemon_request(&r, &format!("recluster {dev} {a_plug} {a_id}"));
    assert!(
        reply.contains("\"ok\":false") && reply.contains("That drive was replaced. Nothing was changed."),
        "{reply}"
    );
    let o = Command::new(env!("CARGO_BIN_EXE_flashcp"))
        .args(["drive-mode", "recluster", &dev, "--yes", "--json", "--plug", &a_plug, "--identity", &a_id])
        .envs(rig_env(&r.dir))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(3), "{text}");
    assert!(text.contains("That drive was replaced. Nothing was changed."), "{text}");
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.status().contains("\"state\":\"ready\""), "{}", r.status());
    assert_eq!(fs::read(&img).unwrap(), before, "B was written to");
    assert!(!r.dir.join("state/flashcp/recluster-offers").exists());
    r.stop();
}

/// The last line of defence: recluster itself compares the volume serial it reads through the
/// claimed fd with the one the user was asked about, before its first write.
#[test]
fn recluster_refuses_a_volume_with_another_serial() {
    let mut r = Rig::new("serial");
    r.cli(&["drive-mode", "offer", "off"]);
    let img = r.big_cluster_image("s");
    // udev says one serial, the medium has another (it was swapped under the same name).
    let dev = r.announce_as(&img, "0BAD-F00D");
    r.wait("ready", r.state_is("ready"));
    let before = fs::read(&img).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_flashcp"))
        .args(["drive-mode", "recluster", &dev, "--yes", "--json"])
        .envs(rig_env(&r.dir))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(3), "{text}\n{}", r.log("daemon"));
    assert!(text.contains("\"result\":\"refused\"") && text.contains("not the volume that was checked"), "{text}");
    assert_eq!(fs::read(&img).unwrap(), before, "nothing was written");
    r.wait("mounted again", r.state_is("ready"));
    r.stop();
}

/// A dialog left open (or a CLI confirmation made) across a restart of the drive service, for
/// the very same stick at the same path: refused by the new daemon, nothing written or saved.
#[test]
fn a_confirmation_from_before_a_daemon_restart_is_refused() {
    let mut r = Rig::new("restart");
    r.cli(&["drive-mode", "offer", "off"]);
    let img = r.big_cluster_image("r");
    let dev = r.announce(&img);
    r.wait("ready", r.state_is("ready"));
    let (old_plug, id) = (plug_of(&r.status()), field_of(&r.status(), "identity"));
    assert_eq!(old_plug.len(), 32, "{old_plug}");
    r.restart();
    r.announce(&img);
    r.wait("ready again", r.state_is("ready"));
    assert_eq!(field_of(&r.status(), "identity"), id, "the same stick");
    assert_ne!(plug_of(&r.status()), old_plug);
    let before = fs::read(&img).unwrap();
    let reply = daemon_request(&r, &format!("recluster {dev} {old_plug} {id}"));
    assert!(reply.contains("That drive was replaced. Nothing was changed."), "{reply}");
    // What the app's "Make faster" runs, with the token its dialog still holds.
    let o = Command::new(env!("CARGO_BIN_EXE_flashcp"))
        .args(["drive-mode", "recluster", &dev, "--yes", "--json", "--plug", &old_plug, "--identity", &id])
        .envs(rig_env(&r.dir))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(3), "{text}");
    assert!(text.contains("That drive was replaced. Nothing was changed."), "{text}");
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.status().contains("\"state\":\"ready\""));
    assert_eq!(fs::read(&img).unwrap(), before, "written to");
    assert!(!r.dir.join("state/flashcp/recluster-offers").exists());
    r.stop();
}
