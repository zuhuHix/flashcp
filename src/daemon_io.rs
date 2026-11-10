//! The I/O side of `flashcp drived`: runs the [`daemon::Core`]'s actions and feeds results back.
//!
//! Event sources (one thread each, into one channel): `udevadm monitor` (block devices), the
//! drivers' stdout and exit, `--check` runs, control-socket replies, timers, logind's
//! PrepareForSleep/PrepareForShutdown (`gdbus monitor`), SIGTERM, and the daemon's own control
//! socket (`$XDG_RUNTIME_DIR/flashcp/drived.sock`: `status`, `enable`, `disable`, `eject DEV`).
//!
//! The udev side (`flashcp-udev-check`, run by the drive-mode rule) only hides a partition from
//! udisks while `$XDG_RUNTIME_DIR/flashcp/drive-mode-on` exists, and leaves it to udisks again
//! when `kernel/<UUID>` exists there; this file creates and removes those markers.
//!
//! Test hooks (environment): `FLASHCP_DRIVED_EVENTS` (read udev-style `KEY=VALUE` blocks from
//! this file/FIFO instead of `udevadm monitor`, and skip the enumeration), `FLASHCP_DRIVED_MEDIA`
//! (create mountpoints directly in this directory instead of through the pkexec helper),
//! `FLASHCP_DRIVED_FLASHCP` (the `flashcp` binary for `mount`), `FLASHCP_DRIVED_UDISKSCTL`,
//! `FLASHCP_DRIVED_GDBUS`, `FLASHCP_DRIVED_NOTIFY` (replacements for udisksctl, gdbus and
//! notify-send), `FLASHCP_DRIVED_NO_LOGIN1=1` (no inhibitor, no sleep monitor).

use crate::daemon::{Action, Core, DevInfo, DriverEvent, Input, Urgency};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

pub enum Msg {
    In(Input),
    /// A request on the daemon's control socket, with where to send the one-line reply.
    Req(String, Sender<String>),
}

/// Paths and replaceable programs (see the module docs for the test hooks).
#[derive(Clone)]
pub struct Env {
    pub runtime: PathBuf,
    pub config: PathBuf,
    pub media: Option<PathBuf>,
    pub flashcp: PathBuf,
    pub udisksctl: String,
    pub gdbus: String,
    pub notify: String,
    pub events: Option<PathBuf>,
    pub login1: bool,
}

impl Env {
    pub fn from_env() -> Result<Env> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let runtime = var("XDG_RUNTIME_DIR").map(PathBuf::from).context("XDG_RUNTIME_DIR is not set")?.join("flashcp");
        Ok(Env {
            runtime,
            config: config_path(),
            media: var("FLASHCP_DRIVED_MEDIA").map(PathBuf::from),
            flashcp: var("FLASHCP_DRIVED_FLASHCP").map(PathBuf::from).unwrap_or(std::env::current_exe()?),
            udisksctl: var("FLASHCP_DRIVED_UDISKSCTL").unwrap_or_else(|| "udisksctl".into()),
            gdbus: var("FLASHCP_DRIVED_GDBUS").unwrap_or_else(|| "gdbus".into()),
            notify: var("FLASHCP_DRIVED_NOTIFY").unwrap_or_else(|| "notify-send".into()),
            events: var("FLASHCP_DRIVED_EVENTS").map(PathBuf::from),
            login1: var("FLASHCP_DRIVED_NO_LOGIN1").is_none(),
        })
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime.join("drived.sock")
    }
    fn marker(&self) -> PathBuf {
        self.runtime.join("drive-mode-on")
    }
    fn driver_socket(&self, dev: &str) -> PathBuf {
        self.runtime.join(format!("{}.sock", dev.rsplit('/').next().unwrap_or("dev")))
    }
}

/// `$XDG_CONFIG_HOME/flashcp/drive-mode.conf` (`enabled=true|false`; default on).
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("flashcp").join("drive-mode.conf")
}

pub fn read_enabled(config: &Path) -> bool {
    std::fs::read_to_string(config).map(|s| !s.lines().any(|l| l.trim() == "enabled=false")).unwrap_or(true)
}

pub fn write_enabled(config: &Path, on: bool) -> Result<()> {
    if let Some(d) = config.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = config.with_extension("tmp");
    std::fs::write(&tmp, format!("# flashcp drive mode (\"Speed up USB drives\")\nenabled={on}\n"))?;
    std::fs::rename(&tmp, config)?;
    Ok(())
}

static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);
static MONITOR_DIED: AtomicBool = AtomicBool::new(false);

fn system_off() -> bool {
    Path::new(crate::daemon_cli::SYSTEM_OFF).exists()
}

extern "C" fn on_signal(_: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::write(fd, b"x".as_ptr().cast(), 1) };
    }
}

/// Run the daemon until SIGTERM (then every drive is flushed and unmounted) or a fatal error.
pub fn run(env: Env) -> Result<()> {
    std::fs::create_dir_all(&env.runtime)?;
    std::fs::set_permissions(&env.runtime, std::fs::Permissions::from_mode(0o700))?;
    std::fs::create_dir_all(env.runtime.join("kernel"))?;
    // Fallback flags are per plug-in: a fresh daemon starts without any.
    for e in std::fs::read_dir(env.runtime.join("kernel"))?.flatten() {
        let _ = std::fs::remove_file(e.path());
    }
    let (tx, rx) = unbounded::<Msg>();

    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0 {
        SIGNAL_PIPE.store(fds[1], Ordering::Relaxed);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
        }
        let t = tx.clone();
        let rfd = fds[0];
        std::thread::spawn(move || {
            let mut b = [0u8; 1];
            while unsafe { libc::read(rfd, b.as_mut_ptr().cast(), 1) } != 1 {}
            let _ = t.send(Msg::In(Input::Shutdown));
        });
    }

    let sock = env.socket();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    let t = tx.clone();
    std::thread::spawn(move || serve_control(listener, t));

    let enabled = read_enabled(&env.config) && !system_off();
    let mut io =
        Io { env: env.clone(), tx: tx.clone(), drivers: HashMap::new(), notes: HashMap::new(), inhibitor: None };
    let mut core = Core::new(enabled);
    cleanup_stale_mounts();

    // udev: start the monitor first, then enumerate, so nothing falls between the two.
    let t = tx.clone();
    match env.events.clone() {
        Some(p) => {
            std::thread::spawn(move || {
                if let Ok(f) = std::fs::File::open(&p) {
                    read_udev_blocks(BufReader::new(f), &t);
                }
            });
        }
        None => {
            let mut child = Command::new("udevadm")
                .args(["monitor", "--udev", "--property", "--subsystem-match=block"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("udevadm monitor")?;
            let out = child.stdout.take().unwrap();
            std::thread::spawn(move || {
                read_udev_blocks(BufReader::new(out), &t);
                let _ = child.wait();
                eprintln!("flashcp drived: udevadm monitor ended; stopping (systemd restarts us)");
                MONITOR_DIED.store(true, Ordering::Relaxed);
                let _ = t.send(Msg::In(Input::Shutdown)); // flush and unmount first
            });
            std::thread::sleep(Duration::from_millis(100));
            for props in enumerate() {
                if let Some(i) = to_input(&props) {
                    let _ = tx.send(Msg::In(i));
                }
            }
        }
    }
    if env.login1 {
        let t = tx.clone();
        let gdbus = env.gdbus.clone();
        std::thread::spawn(move || watch_login1(&gdbus, &t));
    }
    // The administrator's switch (`drive-mode off --system`): looked at every 2 s.
    let t = tx.clone();
    let config = env.config.clone();
    std::thread::spawn(move || {
        let mut off = system_off();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if system_off() != off {
                off = !off;
                let _ = t.send(Msg::In(Input::SetEnabled(!off && read_enabled(&config))));
            }
        }
    });
    // Last: from now on the udev rule hides drives from udisks for us (ExecStopPost removes it
    // again if we die).
    io.set_marker(enabled);
    eprintln!("flashcp drived: running (drive mode {})", if enabled { "on" } else { "off" });

    main_loop(&mut core, &mut io, &rx);
    io.set_marker(false);
    let _ = std::fs::remove_file(&sock);
    anyhow::ensure!(!MONITOR_DIED.load(Ordering::Relaxed), "udevadm monitor ended");
    Ok(())
}

fn main_loop(core: &mut Core, io: &mut Io, rx: &Receiver<Msg>) {
    while let Ok(msg) = rx.recv() {
        let input = match msg {
            Msg::In(i) => i,
            Msg::Req(line, reply) => {
                let mut words = line.split_whitespace();
                let (r, input) = match (words.next(), words.next()) {
                    (Some("status"), None) => {
                        (core.status_json().replacen("{\"ok\":true", "{\"ok\":true,\"running\":true", 1), None)
                    }
                    (Some("enable"), None) => (ok_reply(true), Some(Input::SetEnabled(true))),
                    (Some("disable"), None) => (ok_reply(false), Some(Input::SetEnabled(false))),
                    (Some("eject"), Some(dev)) if core.drives.contains_key(dev) => {
                        ("{\"ok\":true}".into(), Some(Input::Eject { dev: dev.into() }))
                    }
                    (Some("eject"), Some(_)) => ("{\"ok\":false,\"error\":\"no such drive\"}".into(), None),
                    _ => ("{\"ok\":false,\"error\":\"unknown command\"}".into(), None),
                };
                let _ = reply.send(r);
                match input {
                    Some(i) => i,
                    None => continue,
                }
            }
        };
        let input = match input {
            Input::SetEnabled(on) => {
                let on = on && !system_off();
                io.set_marker(on);
                if on {
                    io.clear_fallback_flags(None);
                }
                Input::SetEnabled(on)
            }
            Input::Removed { dev } => {
                // Fallback flags are per plug-in: the next plug-in tries flashcp again.
                if let Some(d) = core.drives.get(&dev) {
                    io.clear_fallback_flags(Some(&fallback_key(&d.info)));
                }
                Input::Removed { dev }
            }
            Input::Shutdown => {
                io.set_marker(false); // from now on new drives go to udisks
                Input::Shutdown
            }
            i => i,
        };
        let log = format!("{input:?}");
        let actions = core.handle(input);
        if !actions.is_empty() {
            let a: Vec<String> = actions.iter().map(|a| a.to_string()).collect();
            eprintln!("flashcp drived: {} -> {}", short(&log), a.join("; "));
        }
        for a in actions {
            if a == Action::Exit {
                return;
            }
            io.perform(a, core);
        }
    }
}

fn ok_reply(on: bool) -> String {
    format!("{{\"ok\":true,\"enabled\":{on}}}")
}

fn short(s: &str) -> &str {
    &s[..s.char_indices().nth(160).map(|(i, _)| i).unwrap_or(s.len())]
}

struct Io {
    env: Env,
    tx: Sender<Msg>,
    /// Running drivers by device: their pid (for SIGTERM).
    drivers: HashMap<String, u32>,
    /// Notification ids by key, to replace a notification instead of stacking a new one.
    notes: HashMap<String, u32>,
    inhibitor: Option<Child>,
}

impl Io {
    fn send_later(&self, f: impl FnOnce() -> Input + Send + 'static) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::In(f()));
        });
    }

    fn clear_fallback_flags(&self, key: Option<&str>) {
        let dir = self.env.runtime.join("kernel");
        match key {
            Some(k) if !k.is_empty() => {
                let _ = std::fs::remove_file(dir.join(k));
            }
            Some(_) => {}
            None => {
                for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    fn set_marker(&self, on: bool) {
        let m = self.env.marker();
        if on {
            let _ = std::fs::write(&m, b"");
        } else {
            let _ = std::fs::remove_file(&m);
        }
    }

    fn perform(&mut self, a: Action, core: &Core) {
        match a {
            Action::Check { dev } => {
                let bin = self.env.flashcp.clone();
                self.send_later(move || {
                    let mut c = Command::new(&bin);
                    c.args(["mount", "--check", "--json"]).arg(&dev).stdin(Stdio::null()).stderr(Stdio::null());
                    // A device that stalls must not leave the drive in "checking" forever.
                    let (code, reason) = match run_with_timeout(c, Duration::from_secs(10)) {
                        Ok((code, out)) => {
                            let line = String::from_utf8_lossy(&out).lines().last().unwrap_or("").to_string();
                            let m = crate::daemon::json::parse_flat(&line).unwrap_or_default();
                            let code = code.unwrap_or(1);
                            // Exit 0 counts only with a `check` event saying ok and no busy_unknown.
                            let reason = if m.contains_key("busy_unknown") {
                                Some("busy_unknown".to_string())
                            } else if code == 0
                                && !(m.get("event").is_some_and(|e| e == "check")
                                    && m.get("ok").is_some_and(|v| v == "true"))
                            {
                                Some("unexpected check output".to_string())
                            } else {
                                m.get("reason").cloned().filter(|r| r != "null")
                            };
                            (code, reason)
                        }
                        Err(e) => (1, Some(e.to_string())),
                    };
                    Input::CheckExit { dev, code, reason }
                });
            }
            Action::MakeMountpoint { dev, name } => {
                let media = self.env.media.clone();
                self.send_later(move || Input::MountpointReady {
                    dev,
                    result: make_mountpoint(media.as_deref(), &name),
                });
            }
            Action::StartDriver { dev, mountpoint, label } => self.start_driver(dev, mountpoint, label),
            Action::StopDriver { dev } => {
                if let Some(pid) = self.drivers.get(&dev) {
                    unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
                }
            }
            Action::Control { dev, cmd } => {
                let sock = self.env.driver_socket(&dev);
                self.send_later(move || {
                    let (ok, error) = match control(&sock, cmd) {
                        Ok(reply) => {
                            let m = crate::daemon::json::parse_flat(&reply).unwrap_or_default();
                            (m.get("ok").is_some_and(|v| v == "true"), m.get("error").cloned())
                        }
                        Err(e) => (false, Some(e.to_string())),
                    };
                    Input::ControlReply { dev, cmd: cmd.into(), ok, error }
                });
            }
            Action::KernelMount { dev, read_only } => {
                let env = self.env.clone();
                let key = core.drives.get(&dev).map(|d| fallback_key(&d.info)).unwrap_or_default();
                self.send_later(move || Input::KernelMounted {
                    result: kernel_mount(&env, &dev, &key, read_only),
                    dev,
                });
            }
            Action::KernelEject { dev, disk, checked } => {
                let u = self.env.udisksctl.clone();
                self.send_later(move || match run_ok(&u, &["unmount", "-b", &dev, "--no-user-interaction"]) {
                    // Not mounted: a power-off only right after the holder/mount re-check.
                    Err(error) if error.contains("NotMounted") && !checked => Input::KernelNotMounted { dev },
                    Err(error) if !error.contains("NotMounted") => Input::KernelUnmountFailed { dev, error },
                    _ => {
                        let ok = run_ok(&u, &["power-off", "-b", &disk, "--no-user-interaction"]).is_ok();
                        Input::PoweredOff { dev, ok }
                    }
                });
            }
            Action::PowerOff { dev, disk } => {
                let u = self.env.udisksctl.clone();
                self.send_later(move || {
                    let ok = run_ok(&u, &["power-off", "-b", &disk, "--no-user-interaction"]).is_ok();
                    Input::PoweredOff { dev, ok }
                });
            }
            Action::RemoveMountpoint { path } => {
                let media = self.env.media.clone();
                std::thread::spawn(move || remove_mountpoint(media.as_deref(), Path::new(&path)));
            }
            Action::Notify { key, title, body, urgency } => self.notify(&key, &title, &body, urgency),
            Action::Timer { dev, ms, seq } => {
                self.send_later(move || {
                    std::thread::sleep(Duration::from_millis(ms));
                    Input::Timer { dev, seq }
                });
            }
            Action::Inhibit(on) => self.inhibit(on),
            Action::Rescan { dev } => {
                let _ = rescan(&self.env.gdbus, &dev);
            }
            Action::Exit => {}
        }
    }

    fn start_driver(&mut self, dev: String, mountpoint: String, label: String) {
        // A driver that died leaves its FUSE mount behind ("transport endpoint is not connected").
        if is_mountpoint(Path::new(&mountpoint)) {
            let _ = run_ok("fusermount3", &["-u", "-z", &mountpoint]);
        }
        let sock = self.env.driver_socket(&dev);
        let spawned = Command::new(&self.env.flashcp)
            .args(["mount", &dev, &mountpoint, "--json", "--control"])
            .arg(&sock)
            .args(["--label-hint", &label])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!("flashcp drived: cannot start {}: {e}", self.env.flashcp.display());
                self.send_later(move || Input::DriverExit { dev, code: Some(crate::daemon::exit::RUNTIME) });
                return;
            }
        };
        self.drivers.insert(dev.clone(), child.id());
        let out = child.stdout.take().unwrap();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let ev = DriverEvent::parse(&line);
                if ev != DriverEvent::Other {
                    let _ = tx.send(Msg::In(Input::Driver { dev: dev.clone(), ev }));
                }
            }
            let code = child.wait().ok().and_then(|s| s.code());
            let _ = tx.send(Msg::In(Input::DriverExit { dev, code }));
        });
    }

    fn notify(&mut self, key: &str, title: &str, body: &str, urgency: Urgency) {
        let u = match urgency {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        };
        let mut c = Command::new(&self.env.notify);
        c.args(["--app-name=flashcp", "--icon=drive-removable-media", "--print-id", "-u", u]);
        if let Some(id) = self.notes.get(key) {
            c.arg(format!("--replace-id={id}"));
        }
        c.arg("--").arg(title).arg(body).stdin(Stdio::null()).stderr(Stdio::null());
        // notify-send returns at once; a desktop without a notification server must not stall us.
        if let Ok((_, o)) = run_with_timeout(c, Duration::from_secs(3))
            && let Ok(id) = String::from_utf8_lossy(&o).trim().parse::<u32>()
        {
            self.notes.insert(key.into(), id);
        }
    }

    fn inhibit(&mut self, on: bool) {
        if !self.env.login1 {
            return;
        }
        if let Some(mut c) = self.inhibitor.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if on {
            self.inhibitor = Command::new("systemd-inhibit")
                .args(["--what=sleep:shutdown", "--mode=delay", "--who=flashcp", "--why=Saving data to a USB drive"])
                .args(["sleep", "infinity"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }
    }
}

/// The key of the per-drive fallback flag: the volume serial, else the device name.
fn fallback_key(info: &DevInfo) -> String {
    let k = info.uuid.clone().unwrap_or_else(|| info.dev.rsplit('/').next().unwrap_or("").to_string());
    k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(36).collect()
}

/// Hand a drive to the kernel driver: flag it so the udev rule stops hiding it from udisks, have
/// udisks re-run the rules (Block.Rescan), wait for that, then mount it through udisks.
fn kernel_mount(env: &Env, dev: &str, key: &str, read_only: bool) -> Result<String, String> {
    if !key.is_empty() {
        let _ = std::fs::write(env.runtime.join("kernel").join(key), b"");
    }
    let _ = rescan(&env.gdbus, dev);
    if env.events.is_none() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && udev_props(dev).get("FLASHCP_DRIVE_MODE").is_some_and(|v| v == "1") {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if let Some(m) = kernel_mountpoint(dev) {
        return Ok(m);
    }
    let mut args = vec!["mount", "-b", dev, "--no-user-interaction"];
    if read_only {
        args.extend(["-o", "ro"]);
    }
    match run_ok(&env.udisksctl, &args) {
        Ok(out) => Ok(out.trim().rsplit(" at ").next().unwrap_or("").trim_end_matches('.').to_string()),
        Err(e) => kernel_mountpoint(dev).ok_or(e),
    }
}

/// udisks `Block.Rescan`: a change uevent, so udev re-runs the rules for the device.
fn rescan(gdbus: &str, dev: &str) -> Result<String, String> {
    let name = dev.rsplit('/').next().unwrap_or("");
    let path = format!("/org/freedesktop/UDisks2/block_devices/{}", name.replace('-', "_2d"));
    let dest = ["call", "--system", "--dest", "org.freedesktop.UDisks2", "--object-path", &path];
    run_ok(gdbus, &[&dest[..], &["--method", "org.freedesktop.UDisks2.Block.Rescan", "{}"]].concat())
}

fn run_ok(prog: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(prog).args(args).stdin(Stdio::null()).output().map_err(|e| format!("{prog}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

/// Run with a deadline: (exit code, stdout). Stdout is read on a thread so a chatty child can't
/// block on a full pipe.
fn run_with_timeout(mut c: Command, t: Duration) -> std::io::Result<(Option<i32>, Vec<u8>)> {
    let mut child = c.stdout(Stdio::piped()).spawn()?;
    let mut so = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + t;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok((st.code(), reader.join().unwrap_or_default()));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Send one control request to a driver, return its one-line reply.
pub fn control(sock: &Path, cmd: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_secs(300)))?;
    s.write_all(format!("{cmd}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    Ok(line)
}

fn serve_control(listener: UnixListener, tx: Sender<Msg>) {
    for conn in listener.incoming().flatten() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let Ok(r) = conn.try_clone() else { return };
            let mut w = conn;
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                let (rtx, rrx) = unbounded();
                if tx.send(Msg::Req(line, rtx)).is_err() {
                    return;
                }
                let reply = rrx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| "{\"ok\":false}".into());
                if w.write_all(format!("{reply}\n").as_bytes()).is_err() {
                    return;
                }
            }
        });
    }
}

/// Mountpoints: through the pkexec helper (`/media/$USER/NAME`, NAME1, ...), or directly in
/// `media` for tests.
fn make_mountpoint(media: Option<&Path>, name: &str) -> Result<String, String> {
    match media {
        Some(base) => {
            for i in 0..100 {
                let p = base.join(if i == 0 { name.to_string() } else { format!("{name}{i}") });
                match std::fs::create_dir(&p) {
                    Ok(()) => return Ok(p.display().to_string()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let empty = std::fs::read_dir(&p).map(|mut d| d.next().is_none()).unwrap_or(false);
                        if empty && !is_mountpoint(&p) {
                            return Ok(p.display().to_string());
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Err("no free mountpoint name".into())
        }
        None => run_ok("pkexec", &["/usr/libexec/flashcp-mountpoint", "create", name]).map(|s| s.trim().to_string()),
    }
}

fn remove_mountpoint(media: Option<&Path>, path: &Path) {
    for _ in 0..50 {
        if !is_mountpoint(path) {
            break;
        }
        let _ = run_ok("fusermount3", &["-u", "-z", &path.to_string_lossy()]);
        std::thread::sleep(Duration::from_millis(100));
    }
    match media {
        Some(_) => {
            let _ = std::fs::remove_dir(path);
        }
        None => {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let _ = run_ok("pkexec", &["/usr/libexec/flashcp-mountpoint", "remove", name]);
            }
        }
    }
}

fn is_mountpoint(p: &Path) -> bool {
    let (Ok(a), Some(Ok(b))) = (std::fs::symlink_metadata(p), p.parent().map(std::fs::metadata)) else {
        // A dead FUSE mount fails stat with ENOTCONN: still a mountpoint.
        return mountinfo().iter().any(|m| Path::new(&m.target) == p);
    };
    a.dev() != b.dev() || mountinfo().iter().any(|m| Path::new(&m.target) == p)
}

struct MountEntry {
    source: String,
    target: String,
    fstype: String,
}

fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mountinfo() -> Vec<MountEntry> {
    let s = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    s.lines()
        .filter_map(|l| {
            let (a, b) = l.split_once(" - ")?;
            let target = unescape_mountinfo(a.split(' ').nth(4)?);
            let mut bw = b.split(' ');
            let fstype = bw.next()?.to_string();
            let source = unescape_mountinfo(bw.next()?);
            Some(MountEntry { source, target, fstype })
        })
        .collect()
}

fn kernel_mountpoint(dev: &str) -> Option<String> {
    mountinfo().into_iter().find(|m| m.source == dev && !m.fstype.starts_with("fuse")).map(|m| m.target)
}

/// FUSE mounts of ours whose driver is gone (the daemon was killed): unmount them lazily.
fn cleanup_stale_mounts() {
    for m in mountinfo() {
        if m.fstype == "fuse.flashcp" && std::fs::metadata(&m.target).is_err() {
            let _ = run_ok("fusermount3", &["-u", "-z", &m.target]);
        }
    }
}

/// udev properties of one device (`udevadm info`).
fn udev_props(dev: &str) -> BTreeMap<String, String> {
    let out = Command::new("udevadm").args(["info", "--query=property", "--name", dev]).output();
    let mut m = BTreeMap::new();
    if let Ok(o) = out {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some((k, v)) = l.split_once('=') {
                m.insert(k.to_string(), v.to_string());
            }
        }
    }
    m
}

/// Every block device's udev properties, for the startup enumeration.
fn enumerate() -> Vec<BTreeMap<String, String>> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/block") {
        for e in rd.flatten() {
            let mut p = udev_props(&format!("/dev/{}", e.file_name().to_string_lossy()));
            p.insert("ACTION".into(), "add".into());
            v.push(p);
        }
    }
    v
}

/// `KEY=VALUE` lines, blocks separated by blank lines (the `udevadm monitor --property` format;
/// its header lines carry no `=` and are skipped).
pub fn read_udev_blocks(r: impl BufRead, tx: &Sender<Msg>) {
    let mut props = BTreeMap::new();
    for line in r.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            if let Some(i) = to_input(&props) {
                let _ = tx.send(Msg::In(i));
            }
            props.clear();
        } else if let Some((k, v)) = line.split_once('=')
            && !k.contains(' ')
        {
            props.insert(k.to_string(), v.to_string());
        }
    }
    if let Some(i) = to_input(&props) {
        let _ = tx.send(Msg::In(i));
    }
}

/// udev's `\xHH` escapes in `ID_FS_LABEL_ENC`.
fn decode_enc(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(v) = s.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A udev event as daemon input: add/change of a partition the rule marked, or any remove.
pub fn to_input(p: &BTreeMap<String, String>) -> Option<Input> {
    let dev = p.get("DEVNAME")?.clone();
    match p.get("ACTION").map(String::as_str) {
        Some("remove") => Some(Input::Removed { dev }),
        // Only the drives the rule took over for this user (the active seat user's daemon).
        Some("add" | "change")
            if p.get("FLASHCP_DRIVE_MODE").is_some_and(|v| v == "1")
                && p.get("FLASHCP_DRIVE_UID").is_some_and(|u| *u == unsafe { libc::getuid() }.to_string()) =>
        {
            let disk = match (p.get("DEVTYPE").map(String::as_str), p.get("DEVPATH")) {
                (Some("partition"), Some(path)) => {
                    let parent = Path::new(path).parent()?.file_name()?.to_string_lossy().into_owned();
                    format!("/dev/{parent}")
                }
                _ => dev.clone(),
            };
            let label = p.get("ID_FS_LABEL_ENC").map(|l| decode_enc(l)).or_else(|| p.get("ID_FS_LABEL").cloned());
            Some(Input::Added(DevInfo {
                mounted_at: kernel_mountpoint(&dev),
                dev,
                disk,
                label: label.filter(|l| !l.is_empty()),
                uuid: p.get("ID_FS_UUID").cloned().filter(|u| !u.is_empty()),
                recover: p.get("FLASHCP_RECOVER").is_some_and(|v| v == "1"),
            }))
        }
        _ => None,
    }
}

/// logind's PrepareForSleep/PrepareForShutdown, through `gdbus monitor` (no D-Bus crate).
fn watch_login1(gdbus: &str, tx: &Sender<Msg>) {
    let child = Command::new(gdbus)
        .args(["monitor", "--system", "--dest", "org.freedesktop.login1", "--object-path", "/org/freedesktop/login1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        if line.contains("PrepareForSleep") || line.contains("PrepareForShutdown") {
            let before = line.contains("(true");
            let _ = tx.send(Msg::In(Input::Sleep(before)));
        }
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udev_blocks_to_inputs() {
        let uid = unsafe { libc::getuid() };
        let text = format!(
            "monitor will print the received events for:\nUDEV - the event which udev sends out\n\n\
            UDEV  [12.3] add      /devices/x/block/sdb/sdb1 (block)\nACTION=add\nDEVPATH=/devices/x/block/sdb/sdb1\n\
            DEVNAME=/dev/sdb1\nDEVTYPE=partition\nID_FS_LABEL=MY_STICK\nID_FS_LABEL_ENC=MY\\x20STICK\n\
            ID_FS_UUID=1A2B-3C4D\nFLASHCP_DRIVE_MODE=1\nFLASHCP_DRIVE_UID={uid}\n\n\
            ACTION=add\nDEVNAME=/dev/sdc1\nDEVTYPE=partition\nDEVPATH=/d/sdc/sdc1\n\n\
            ACTION=remove\nDEVNAME=/dev/sdb1\n"
        );
        let (tx, rx) = unbounded();
        read_udev_blocks(BufReader::new(text.as_bytes()), &tx);
        let got: Vec<Input> = rx.try_iter().filter_map(|m| if let Msg::In(i) = m { Some(i) } else { None }).collect();
        assert_eq!(got.len(), 2, "{got:?}");
        let Input::Added(i) = &got[0] else { panic!("{got:?}") };
        assert_eq!((i.dev.as_str(), i.disk.as_str(), i.label.as_deref()), ("/dev/sdb1", "/dev/sdb", Some("MY STICK")));
        assert_eq!(got[1], Input::Removed { dev: "/dev/sdb1".into() });
        assert_eq!(unescape_mountinfo("/media/u/MSI\\040DATAMAG"), "/media/u/MSI DATAMAG");
        let d = tempfile_dir();
        let c = d.join("c/drive-mode.conf");
        assert!(read_enabled(&c));
        write_enabled(&c, false).unwrap();
        assert!(!read_enabled(&c));
        write_enabled(&c, true).unwrap();
        assert!(read_enabled(&c));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("flashcp-drived-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
