//! `flashcp drived`: the per-user drive-mode daemon, as pure logic.
//!
//! [`Core`] is a state machine over USB exFAT partitions. It takes [`Input`]s (udev events, exit
//! codes of `flashcp mount --check` and `flashcp mount`, the driver's JSON events, control
//! replies, user requests, sleep signals) and returns the [`Action`]s to perform. It does no I/O
//! itself: `daemon_io` runs the actions and feeds the results back, and the unit test below
//! drives it with a fake event source and a fake mount process.
//!
//! The policy (see docs/drive-mode-interface.md for the exit codes):
//!
//! - a new partition tagged `FLASHCP_DRIVE_MODE=1` by the udev rule: `--check`, then a mountpoint
//!   at `/media/$USER/<LABEL>`, then `flashcp mount`. Drive mode off: a kernel (udisks) mount.
//! - check/mount exit 3 (refused) or 5 (no access) or an unknown code: kernel mount, read-write.
//!   Exit 6 (recovery refused): kernel mount READ-ONLY and tell the user to run fsck.exfat.
//!   Exit 4 (busy): retry after 1, 3 and 10 s, then only notify (never mount over a holder).
//! - the driver dies while serving: restart it once (its journal recovery runs), and after a
//!   second failure fall back to the kernel driver, with a notification.
//! - eject (tray, CLI): `prepare-eject` → "safe" → `unmount` → exit 0 → power the drive off.
//!   An unmount from outside (a file manager's eject/unmount button): the driver commits after
//!   the session ends; the daemon shows "Saving N MB to LABEL…" until it exits, then powers off.
//! - the drive vanishes while data is pending: a notification says what may be lost (the next
//!   plug-in replays the journal; `mounted.recovered` tells the user it was repaired).
//! - drive mode turned off: every flashcp mount is stopped (SIGTERM = flush, unmount) and the
//!   drive is remounted by the kernel driver; new drives get the kernel driver.
//! - sleep/shutdown: a delay inhibitor is held while anything is pending; on PrepareForSleep
//!   every dirty drive is flushed before the inhibitor is released.

use std::collections::BTreeMap;
use std::fmt;

/// Exit codes of `flashcp mount` / `flashcp mount --check` (docs/drive-mode-interface.md).
pub mod exit {
    pub const OK: i32 = 0;
    pub const RUNTIME: i32 = 1;
    pub const FALLBACK: i32 = 3;
    pub const BUSY: i32 = 4;
    pub const NO_ACCESS: i32 = 5;
    pub const RECOVERY_REFUSED: i32 = 6;
}

/// Delays before re-checking a busy device, in ms. After the last one the daemon only notifies.
pub const BUSY_RETRY_MS: [u64; 3] = [1000, 3000, 10_000];

/// What udev told us about a partition (or superfloppy disk) that the rule marked for drive mode.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DevInfo {
    /// Device node, e.g. `/dev/sdb1` (an image path in tests).
    pub dev: String,
    /// The whole disk to power off after an eject, e.g. `/dev/sdb`.
    pub disk: String,
    pub label: Option<String>,
    /// The volume serial as udev's `ID_FS_UUID` (`1A2B-3C4D`).
    pub uuid: Option<String>,
    /// Where it is already mounted (by the kernel driver), if it is.
    pub mounted_at: Option<String>,
}

impl DevInfo {
    /// The name users see: the label, else the serial, else the device name.
    pub fn name(&self) -> String {
        self.label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .or_else(|| self.uuid.clone())
            .unwrap_or_else(|| self.dev.rsplit('/').next().unwrap_or(&self.dev).to_string())
    }
}

/// A JSON event from `flashcp mount --json`, reduced to what the daemon uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverEvent {
    Mounted { recovered: Option<String> },
    DirtyBytes { bytes: u64, files: u64 },
    Committing { bytes: u64, reason: String },
    Committed,
    Error { message: String, fatal: bool },
    Unmounted { clean: bool },
    Refused { reason: String },
    Other,
}

impl DriverEvent {
    /// Parse one stdout line. Unknown events (and non-JSON lines) are `Other`.
    pub fn parse(line: &str) -> DriverEvent {
        let Some(o) = json::parse_flat(line) else { return DriverEvent::Other };
        let num = |k: &str| o.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let text = |k: &str| o.get(k).cloned().unwrap_or_default();
        match o.get("event").map(String::as_str) {
            Some("mounted") => DriverEvent::Mounted { recovered: o.get("recovered").cloned().filter(|r| r != "null") },
            Some("dirty_bytes") => DriverEvent::DirtyBytes { bytes: num("bytes"), files: num("files") },
            Some("committing") => DriverEvent::Committing { bytes: num("bytes"), reason: text("reason") },
            Some("committed") => DriverEvent::Committed,
            Some("error") => DriverEvent::Error { message: text("message"), fatal: text("fatal") == "true" },
            Some("unmounted") => DriverEvent::Unmounted { clean: text("clean") == "true" },
            Some("refused") => DriverEvent::Refused { reason: text("reason") },
            _ => DriverEvent::Other,
        }
    }
}

/// Everything the daemon reacts to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// udev add (or the startup enumeration) of a partition tagged for drive mode.
    Added(DevInfo),
    Removed {
        dev: String,
    },
    /// `flashcp mount --check` exited; `reason` is the `refused`/`check` reason, if any.
    CheckExit {
        dev: String,
        code: i32,
        reason: Option<String>,
    },
    MountpointReady {
        dev: String,
        result: Result<String, String>,
    },
    Driver {
        dev: String,
        ev: DriverEvent,
    },
    /// `flashcp mount` exited (None: killed by a signal).
    DriverExit {
        dev: String,
        code: Option<i32>,
    },
    KernelMounted {
        dev: String,
        result: Result<String, String>,
    },
    /// Eject from the tray or `flashcp drive-mode eject`.
    Eject {
        dev: String,
    },
    /// The reply to a control request (`prepare-eject`, `unmount`, `flush`).
    ControlReply {
        dev: String,
        cmd: String,
        ok: bool,
        error: Option<String>,
    },
    PoweredOff {
        dev: String,
        ok: bool,
    },
    /// udisks says an unchecked [`Action::KernelEject`]'s drive is not mounted.
    KernelNotMounted {
        dev: String,
    },
    /// udisks could not unmount a kernel-mounted drive for an eject (it is in use).
    KernelUnmountFailed {
        dev: String,
        error: String,
    },
    SetEnabled(bool),
    Timer {
        dev: String,
        seq: u64,
    },
    /// logind's PrepareForSleep / PrepareForShutdown: true before, false after resume.
    Sleep(bool),
    /// SIGTERM to the daemon (logout, systemctl stop).
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// What the daemon must do. `daemon_io` performs these and reports back through [`Input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Check {
        dev: String,
    },
    /// Create (or reuse) an empty `/media/$USER/<name>` directory owned by the user.
    MakeMountpoint {
        dev: String,
        name: String,
    },
    StartDriver {
        dev: String,
        mountpoint: String,
        label: String,
    },
    /// SIGTERM the driver: flush, then unmount.
    StopDriver {
        dev: String,
    },
    Control {
        dev: String,
        cmd: &'static str,
    },
    KernelMount {
        dev: String,
        read_only: bool,
    },
    /// Unmount a kernel mount through udisks and power the drive off. `checked`: holders and
    /// mounts were just re-checked, so "not mounted" may go on to the power-off; otherwise a "not
    /// mounted" answer comes back as [`Input::KernelNotMounted`] for that re-check.
    KernelEject {
        dev: String,
        disk: String,
        checked: bool,
    },
    PowerOff {
        dev: String,
        disk: String,
    },
    /// Lazily unmount a leftover FUSE mount there (if any) and remove the empty directory.
    RemoveMountpoint {
        path: String,
    },
    /// Show (or replace, same `key`) a desktop notification.
    Notify {
        key: String,
        title: String,
        body: String,
        urgency: Urgency,
    },
    /// Feed `Input::Timer { dev, seq }` back after `ms`.
    Timer {
        dev: String,
        ms: u64,
        seq: u64,
    },
    /// Take (true) or release (false) the logind delay inhibitor for sleep and shutdown.
    Inhibit(bool),
    /// Have udisks re-run the udev rules for the device (`Block.Rescan`), so the drive-mode rule
    /// hands it back to udisks once the daemon is gone.
    Rescan {
        dev: String,
    },
    /// Every driver has stopped after `Shutdown`: exit the daemon.
    Exit,
}

impl fmt::Display for Action {
    /// A compact form for logs and the transition table.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Check { dev } => write!(f, "check {dev}"),
            Action::MakeMountpoint { dev, name } => write!(f, "mkmp {dev} {name}"),
            Action::StartDriver { dev, mountpoint, .. } => write!(f, "start {dev} {mountpoint}"),
            Action::StopDriver { dev } => write!(f, "stop {dev}"),
            Action::Control { dev, cmd } => write!(f, "ctl {dev} {cmd}"),
            Action::KernelMount { dev, read_only } => {
                write!(f, "kernel {dev} {}", if *read_only { "ro" } else { "rw" })
            }
            Action::KernelEject { dev, disk, checked } => {
                write!(f, "kernel-eject {dev} {disk}{}", if *checked { " checked" } else { "" })
            }
            Action::PowerOff { disk, .. } => write!(f, "poweroff {disk}"),
            Action::RemoveMountpoint { path } => write!(f, "rmmp {path}"),
            Action::Notify { key, .. } => write!(f, "notify {key}"),
            Action::Timer { ms, .. } => write!(f, "timer {ms}"),
            Action::Inhibit(on) => write!(f, "inhibit {}", if *on { "on" } else { "off" }),
            Action::Rescan { dev } => write!(f, "rescan {dev}"),
            Action::Exit => write!(f, "exit"),
        }
    }
}

/// What to do once a stopping driver has exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Drive mode was turned off: hand the drive to the kernel driver.
    Kernel,
    /// The daemon is exiting.
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Checking,
    /// Busy: waiting for retry `tries` (1-based) of [`BUSY_RETRY_MS`].
    BusyWait {
        tries: usize,
    },
    MakingMountpoint,
    /// The driver runs and has not reported `mounted` yet.
    Starting,
    Mounted,
    /// Our eject: `prepare-eject`, then `unmount`, then power off.
    Ejecting,
    /// Someone unmounted the FUSE mount from outside: the driver is finishing its commit.
    Unmounting,
    Stopping(Then),
    KernelMounting {
        read_only: bool,
    },
    Kernel {
        read_only: bool,
    },
    /// Powered off or failed; the entry stays until the device is removed.
    Done {
        safe: bool,
    },
    /// Eject of a drive nobody of ours serves: `--check` looks for holders and mounts first.
    EjectChecking,
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug)]
pub struct Drive {
    pub info: DevInfo,
    pub state: State,
    pub mountpoint: Option<String>,
    /// Driver failures so far (one restart is allowed).
    pub failures: u32,
    pub dirty_bytes: u64,
    pub dirty_files: u64,
    pub committing: bool,
    /// The device was removed (the driver may still be dying).
    pub gone: bool,
    timer_gen: u64,
    busy_tries: usize,
    kernel_ro: bool,
    /// The driver reported `unmounted` clean with nothing pending (reset when it mounts).
    clean_unmount: bool,
}

impl Drive {
    fn dirty(&self) -> bool {
        self.dirty_bytes > 0 || self.dirty_files > 0 || self.committing
    }
    fn driver_running(&self) -> bool {
        matches!(
            self.state,
            State::Starting | State::Mounted | State::Ejecting | State::Unmounting | State::Stopping(_)
        )
    }
    /// "flashcp", "kernel" or "none": who serves the volume right now.
    pub fn mode(&self) -> &'static str {
        match self.state {
            State::Starting | State::Mounted | State::Ejecting | State::Unmounting | State::Stopping(_) => "flashcp",
            State::Kernel { .. } | State::KernelMounting { .. } => "kernel",
            _ => "none",
        }
    }
    pub fn state_name(&self) -> &'static str {
        match self.state {
            State::Checking => "checking",
            State::BusyWait { .. } => "busy",
            State::MakingMountpoint | State::Starting => "starting",
            State::Mounted if self.dirty() => "saving",
            State::Mounted => "ready",
            State::Ejecting | State::Unmounting | State::EjectChecking => "ejecting",
            State::Stopping(_) => "stopping",
            State::KernelMounting { .. } => "mounting",
            State::Kernel { read_only: true } => "read_only",
            State::Kernel { .. } => "kernel",
            State::Done { safe: true } => "safe_to_remove",
            State::Done { safe: false } => "stopped",
            State::Failed { .. } => "failed",
        }
    }
}

/// The daemon's state: every known drive, the off switch, the inhibitor.
pub struct Core {
    pub enabled: bool,
    pub drives: BTreeMap<String, Drive>,
    inhibiting: bool,
    /// PrepareForSleep(true) seen: flushes are in flight; release the inhibitor when done.
    sleeping: bool,
    shutting_down: bool,
}

pub fn mb(bytes: u64) -> String {
    let m = bytes as f64 / 1e6;
    if m >= 10.0 { format!("{m:.0} MB") } else { format!("{m:.1} MB") }
}

impl Core {
    pub fn new(enabled: bool) -> Core {
        Core { enabled, drives: BTreeMap::new(), inhibiting: false, sleeping: false, shutting_down: false }
    }

    /// Feed one input, get the actions to perform (in order).
    pub fn handle(&mut self, input: Input) -> Vec<Action> {
        let mut out = Vec::new();
        self.step(input, &mut out);
        self.update_inhibitor(&mut out);
        if self.shutting_down && !self.drives.values().any(Drive::driver_running) && !out.contains(&Action::Exit) {
            out.push(Action::Exit);
        }
        out
    }

    fn step(&mut self, input: Input, out: &mut Vec<Action>) {
        match input {
            Input::Added(info) => self.added(info, out),
            Input::SetEnabled(on) => self.set_enabled(on, out),
            Input::Sleep(before) => self.sleep(before, out),
            Input::Shutdown => {
                self.shutting_down = true;
                for d in self.drives.values_mut() {
                    match d.state {
                        // One SIGTERM per driver: ejecting/unmounting/stopping ones are on their way.
                        State::Starting | State::Mounted if !d.gone => {
                            d.state = State::Stopping(Then::Exit);
                            out.push(Action::StopDriver { dev: d.info.dev.clone() });
                        }
                        // Hidden from udisks but not served: hand it back now.
                        State::Checking
                        | State::BusyWait { .. }
                        | State::MakingMountpoint
                        | State::EjectChecking
                        | State::Failed { .. } => out.push(Action::Rescan { dev: d.info.dev.clone() }),
                        _ => {}
                    }
                }
            }
            Input::Removed { dev } => self.removed(&dev, out),
            Input::CheckExit { dev, code, reason } => self.check_exit(&dev, code, reason, out),
            Input::MountpointReady { dev, result } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                if d.state != State::MakingMountpoint {
                    return;
                }
                match result {
                    Ok(path) if self.enabled && !self.shutting_down => {
                        d.mountpoint = Some(path.clone());
                        d.state = State::Starting;
                        out.push(Action::StartDriver { dev, mountpoint: path, label: d.info.name() });
                    }
                    Ok(path) => {
                        out.push(Action::RemoveMountpoint { path });
                        kernel(d, false, out);
                    }
                    // No mountpoint (the helper is missing or refused): the kernel driver serves it.
                    Err(_) => kernel(d, false, out),
                }
            }
            Input::Driver { dev, ev } => self.driver_event(&dev, ev, out),
            Input::DriverExit { dev, code } => self.driver_exit(&dev, code, out),
            Input::KernelMounted { dev, result } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                let State::KernelMounting { read_only } = d.state else { return };
                match result {
                    Ok(path) => {
                        d.mountpoint = Some(path);
                        d.state = State::Kernel { read_only };
                    }
                    Err(e) => {
                        out.push(notify(
                            d,
                            "failed",
                            format!("Could not open {}", d.info.name()),
                            e.clone(),
                            Urgency::Normal,
                        ));
                        d.state = State::Failed { message: e };
                    }
                }
            }
            Input::Eject { dev } => {
                let sibling_active = self.sibling_active(&dev);
                let Some(d) = self.drives.get_mut(&dev) else { return };
                match d.state {
                    State::Mounted => {
                        d.state = State::Ejecting;
                        if d.dirty() {
                            out.push(saving(d));
                        }
                        out.push(Action::Control { dev, cmd: "prepare-eject" });
                    }
                    State::Kernel { read_only } => {
                        d.kernel_ro = read_only;
                        d.state = State::Done { safe: true };
                        out.push(Action::KernelEject { dev, disk: d.info.disk.clone(), checked: false });
                    }
                    // Nothing of ours is mounted, but someone else may hold the device (the busy
                    // retries ran out) or have mounted it since: re-check holders and mounts first,
                    // then eject through udisks (unmount + power off), never a bare power-off.
                    State::Done { .. } | State::Failed { .. } if !sibling_active => {
                        d.state = State::EjectChecking;
                        out.push(Action::Check { dev });
                    }
                    _ => {}
                }
            }
            Input::ControlReply { dev, cmd, ok, error } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                match (cmd.as_str(), &d.state) {
                    ("prepare-eject", State::Ejecting) if ok => out.push(Action::Control { dev, cmd: "unmount" }),
                    ("prepare-eject", State::Ejecting) => {
                        d.state = State::Mounted;
                        out.push(Action::Control { dev: dev.clone(), cmd: "resume" });
                        out.push(notify(
                            d,
                            "saving",
                            format!("Could not eject {}", d.info.name()),
                            format!("{} Nothing was lost; it is still in use.", error.unwrap_or_default()),
                            Urgency::Normal,
                        ));
                    }
                    // The control socket is gone (the driver is exiting): its exit decides.
                    ("unmount", _) => {}
                    ("flush", _) => {}
                    _ => {}
                }
            }
            Input::PoweredOff { dev, ok } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                let name = d.info.name();
                let saved = d.state == State::Done { safe: true };
                let body = match (saved, ok) {
                    (true, true) => "Everything was saved.",
                    (true, false) => "Everything was saved and it is unmounted (it could not be powered off).",
                    (false, true) => "It was powered off.",
                    (false, false) => "It is not mounted (it could not be powered off).",
                };
                out.push(notify(d, "removed", format!("{name} can be removed"), body.into(), Urgency::Low));
            }
            Input::KernelNotMounted { dev } => {
                // Someone unmounted it meanwhile: re-check holders and mounts before powering off.
                let Some(d) = self.drives.get_mut(&dev) else { return };
                d.state = State::EjectChecking;
                out.push(Action::Check { dev });
            }
            Input::KernelUnmountFailed { dev, error } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                d.state = State::Kernel { read_only: d.kernel_ro };
                out.push(notify(
                    d,
                    "removed",
                    format!("{} is in use", d.info.name()),
                    format!("It was not ejected: {error}. Close the files and windows using it, then try again."),
                    Urgency::Normal,
                ));
            }
            Input::Timer { dev, seq } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                if d.timer_gen != seq || !matches!(d.state, State::BusyWait { .. }) {
                    return;
                }
                if self.enabled {
                    d.state = State::Checking;
                    out.push(Action::Check { dev });
                } else {
                    kernel(d, false, out);
                }
            }
        }
    }

    fn added(&mut self, info: DevInfo, out: &mut Vec<Action>) {
        if let Some(d) = self.drives.get(&info.dev)
            && !d.gone
        {
            return; // a duplicate (change event, or enumeration racing the monitor)
        }
        let dev = info.dev.clone();
        let mut d = Drive {
            info,
            state: State::Checking,
            mountpoint: None,
            failures: 0,
            dirty_bytes: 0,
            dirty_files: 0,
            committing: false,
            gone: false,
            timer_gen: 0,
            busy_tries: 0,
            kernel_ro: false,
            clean_unmount: false,
        };
        if let Some(m) = d.info.mounted_at.clone() {
            // Already served by the kernel driver (mounted before the daemon started, or by hand).
            d.mountpoint = Some(m);
            d.state = State::Kernel { read_only: false };
        } else if !self.enabled || self.shutting_down {
            kernel(&mut d, false, out);
        } else {
            out.push(Action::Check { dev: dev.clone() });
        }
        self.drives.insert(dev, d);
    }

    fn check_exit(&mut self, dev: &str, code: i32, reason: Option<String>, out: &mut Vec<Action>) {
        let enabled = self.enabled && !self.shutting_down;
        let Some(d) = self.drives.get_mut(dev) else { return };
        if d.state == State::EjectChecking {
            // Allow-list: only a clean "nothing holds it" answer may lead to a power-off.
            if code == exit::OK && reason.is_none() {
                d.state = State::Done { safe: true };
                out.push(Action::KernelEject { dev: dev.into(), disk: d.info.disk.clone(), checked: true });
                return;
            }
            let name = d.info.name();
            let (title, body) = if code == exit::BUSY {
                (
                    format!("{name} is in use by another program"),
                    "Close the program using it, then try again.".to_string(),
                )
            } else {
                (
                    format!("{name} was not ejected"),
                    format!(
                        "flashcp could not make sure nothing is using it ({}). Unplug it only once nothing \
                         uses it.",
                        reason.unwrap_or_else(|| format!("check exit {code}"))
                    ),
                )
            };
            out.push(notify(d, "removed", title, body, Urgency::Normal));
            d.state = State::Failed { message: "eject refused".into() };
            return;
        }
        if d.state != State::Checking {
            return;
        }
        if !enabled {
            return kernel(d, false, out);
        }
        match code {
            // The check could not see holders/mounts: as good as busy.
            exit::OK if reason.as_deref() == Some("busy_unknown") => refused(d, exit::BUSY, reason, out),
            // Exit 0 without a proper `check` answer: not trusted, the kernel driver gets it.
            exit::OK if reason.is_some() => refused(d, exit::FALLBACK, reason, out),
            exit::OK => {
                d.state = State::MakingMountpoint;
                out.push(Action::MakeMountpoint { dev: dev.into(), name: mountpoint_name(&d.info) });
            }
            _ => refused(d, code, reason, out),
        }
    }

    fn driver_event(&mut self, dev: &str, ev: DriverEvent, out: &mut Vec<Action>) {
        let Some(d) = self.drives.get_mut(dev) else { return };
        match ev {
            DriverEvent::Mounted { recovered } => {
                d.clean_unmount = false;
                if d.state == State::Starting {
                    d.state = State::Mounted;
                }
                if let Some(r) = recovered {
                    let body = if r == "replayed" {
                        "An interrupted save was completed: the files copied before it was unplugged are there."
                    } else {
                        "It was not unmounted cleanly last time; the volume was checked and is consistent."
                    };
                    out.push(notify(
                        d,
                        "recovered",
                        format!("{} was repaired", d.info.name()),
                        body.into(),
                        Urgency::Normal,
                    ));
                }
            }
            DriverEvent::DirtyBytes { bytes, files } => {
                d.dirty_bytes = bytes;
                d.dirty_files = files;
                if bytes == 0 && files == 0 {
                    d.committing = false;
                }
                if matches!(d.state, State::Ejecting | State::Unmounting) && d.dirty() {
                    out.push(saving(d));
                }
            }
            DriverEvent::Committing { bytes, reason } => {
                d.committing = true;
                d.dirty_bytes = d.dirty_bytes.max(bytes);
                if reason == "unmount" && d.state == State::Mounted {
                    // Unmounted from outside (a file manager's eject button, fusermount3 -u).
                    d.state = State::Unmounting;
                }
                if matches!(d.state, State::Ejecting | State::Unmounting) {
                    out.push(saving(d));
                }
            }
            DriverEvent::Committed => {
                d.committing = false;
            }
            DriverEvent::Error { message, fatal } => {
                if !fatal && !d.gone {
                    out.push(notify(
                        d,
                        "error",
                        format!("Problem saving to {}", d.info.name()),
                        message,
                        Urgency::Normal,
                    ));
                }
            }
            DriverEvent::Unmounted { clean } => {
                d.clean_unmount = clean && !d.dirty();
                if d.state == State::Mounted {
                    d.state = State::Unmounting;
                }
            }
            DriverEvent::Refused { .. } | DriverEvent::Other => {}
        }
    }

    /// Another partition of the same disk is still mounted by us (flashcp or kernel).
    fn sibling_active(&self, dev: &str) -> bool {
        let Some(disk) = self.drives.get(dev).map(|d| d.info.disk.clone()) else { return false };
        self.drives.values().any(|o| {
            o.info.dev != dev
                && o.info.disk == disk
                && !o.gone
                && !matches!(o.state, State::Done { .. } | State::Failed { .. })
        })
    }

    fn driver_exit(&mut self, dev: &str, code: Option<i32>, out: &mut Vec<Action>) {
        let enabled = self.enabled && !self.shutting_down;
        let sibling_active = self.sibling_active(dev);
        let Some(d) = self.drives.get_mut(dev) else { return };
        let was_dirty = d.dirty();
        let (bytes, files) = (d.dirty_bytes, d.dirty_files);
        d.dirty_bytes = 0;
        d.dirty_files = 0;
        d.committing = false;
        let mp = d.mountpoint.clone();
        let rm = |out: &mut Vec<Action>| {
            if let Some(path) = mp.clone() {
                out.push(Action::RemoveMountpoint { path });
            }
        };
        if d.gone {
            rm(out);
            self.drives.remove(dev);
            return;
        }
        let name = d.info.name();
        match (d.state.clone(), code) {
            (State::Starting, Some(c))
                if [exit::FALLBACK, exit::BUSY, exit::NO_ACCESS, exit::RECOVERY_REFUSED].contains(&c) =>
            {
                rm(out);
                d.mountpoint = None;
                refused(d, c, None, out);
            }
            (State::Starting | State::Mounted, c) if !(d.state == State::Mounted && c == Some(exit::OK)) => {
                // The driver crashed (or failed to start): restart once, then the kernel driver.
                d.failures += 1;
                if was_dirty && d.failures == 1 && enabled {
                    out.push(notify(
                        d,
                        "lost",
                        format!("Problem saving to {name}"),
                        format!(
                            "flashcp's driver stopped while saving: {} in {} files may be missing. It is restarting \
                             and repairs the drive; copy those files again.",
                            mb(bytes),
                            files
                        ),
                        Urgency::Critical,
                    ));
                }
                if d.failures == 1 && enabled {
                    d.state = State::Starting;
                    out.push(Action::StartDriver {
                        dev: dev.into(),
                        mountpoint: mp.clone().unwrap_or_default(),
                        label: name.clone(),
                    });
                } else {
                    rm(out);
                    d.mountpoint = None;
                    let lost = if was_dirty {
                        format!(" {} in {} files that were still being saved may be missing.", mb(bytes), files)
                    } else {
                        String::new()
                    };
                    if enabled {
                        out.push(notify(
                            d,
                            "failed",
                            format!("{name}: fast mode stopped"),
                            format!("flashcp's driver failed twice, so {name} now uses the normal driver.{lost}"),
                            Urgency::Critical,
                        ));
                    }
                    kernel(d, false, out);
                }
            }
            (State::Mounted | State::Ejecting | State::Unmounting, Some(exit::OK)) => {
                // Our eject, or an unmount from outside, finished: everything is on the drive.
                rm(out);
                d.mountpoint = None;
                d.state = State::Done { safe: true };
                if sibling_active {
                    out.push(notify(
                        d,
                        "removed",
                        format!("{name} was saved and unmounted"),
                        "Another partition of this drive is still in use: eject it too before removing the drive."
                            .into(),
                        Urgency::Low,
                    ));
                } else {
                    out.push(Action::PowerOff { dev: dev.into(), disk: d.info.disk.clone() });
                }
            }
            (State::Ejecting | State::Unmounting, _) => {
                rm(out);
                d.mountpoint = None;
                let lost =
                    if was_dirty { format!(" Up to {} may not have been saved.", mb(bytes)) } else { String::new() };
                out.push(notify(
                    d,
                    "saving",
                    format!("Problem saving to {name}"),
                    format!(
                        "flashcp's driver stopped with an error while finishing.{lost} Plug it in again to repair it."
                    ),
                    Urgency::Critical,
                ));
                d.state = State::Done { safe: false };
            }
            (State::Stopping(Then::Kernel), c) => {
                rm(out);
                d.mountpoint = None;
                // Read-write only after a clean stop: exit 0 and `unmounted` with nothing pending.
                let clean = c == Some(exit::OK) && d.clean_unmount;
                if !clean {
                    let lost = if was_dirty { format!(" Up to {} may be missing.", mb(bytes)) } else { String::new() };
                    out.push(notify(
                        d,
                        "failed",
                        format!("flashcp couldn't finish saving to {name}"),
                        format!(
                            "It is opened read-only.{lost} To recover it, turn \"Speed up USB drives\" on and plug \
                             it in again."
                        ),
                        Urgency::Critical,
                    ));
                }
                kernel(d, !clean, out);
            }
            (State::Stopping(Then::Exit), _) => {
                rm(out);
                out.push(Action::Rescan { dev: dev.into() });
                d.mountpoint = None;
                d.state = State::Done { safe: code == Some(exit::OK) };
            }
            _ => {}
        }
    }

    fn removed(&mut self, dev: &str, out: &mut Vec<Action>) {
        let Some(d) = self.drives.get_mut(dev) else { return };
        if !d.driver_running() {
            if let (State::Kernel { .. } | State::Failed { .. }, Some(_)) = (&d.state, &d.mountpoint) {
                // udisks cleans its own mounts up.
            }
            self.drives.remove(dev);
            return;
        }
        d.gone = true;
        let name = d.info.name();
        if d.dirty() {
            out.push(notify(
                d,
                "saving",
                format!("{name} was removed too early"),
                format!(
                    "{} in {} files were still being saved and may be missing or incomplete on {name}. \
                     Plug it in again: flashcp repairs it automatically, then copy those files again.",
                    mb(d.dirty_bytes),
                    d.dirty_files
                ),
                Urgency::Critical,
            ));
        }
        // The driver gets I/O errors and exits; make sure it does, its exit removes the mountpoint.
        if matches!(d.state, State::Starting | State::Mounted) {
            out.push(Action::StopDriver { dev: dev.into() });
        }
    }

    fn set_enabled(&mut self, on: bool, out: &mut Vec<Action>) {
        if self.enabled == on {
            return;
        }
        self.enabled = on;
        if on {
            // Drives already on the kernel driver stay there (they may have open files); the next
            // plug-in gets flashcp.
            return;
        }
        for d in self.drives.values_mut() {
            match d.state {
                State::Starting | State::Mounted => {
                    d.state = State::Stopping(Then::Kernel);
                    out.push(Action::StopDriver { dev: d.info.dev.clone() });
                }
                State::BusyWait { .. } => kernel(d, false, out),
                // Checking / MakingMountpoint: the pending reply sees `enabled == false`.
                _ => {}
            }
        }
    }

    fn sleep(&mut self, before: bool, out: &mut Vec<Action>) {
        self.sleeping = before;
        if before {
            for d in self.drives.values() {
                if d.state == State::Mounted && d.dirty() {
                    out.push(Action::Control { dev: d.info.dev.clone(), cmd: "flush" });
                }
            }
        }
    }

    /// Hold the delay inhibitor while any driver has pending data, except after PrepareForSleep
    /// once nothing is pending any more (or the flush was answered).
    fn update_inhibitor(&mut self, out: &mut Vec<Action>) {
        let dirty = self.drives.values().any(|d| d.driver_running() && d.dirty() && !d.gone);
        let want = dirty && !self.sleeping;
        // Before sleep: keep the lock until the flushes have drained the dirty data.
        let want = want || (self.sleeping && dirty && self.inhibiting);
        if want != self.inhibiting {
            self.inhibiting = want;
            out.push(Action::Inhibit(want));
        }
    }

    /// The status the tray and `flashcp drive-mode status --json` show.
    pub fn status_json(&self) -> String {
        use crate::events::json_str;
        let drives: Vec<String> = self
            .drives
            .values()
            .map(|d| {
                format!(
                    r#"{{"device":{},"label":{},"mode":{},"state":{},"mountpoint":{},"dirty_bytes":{},"dirty_files":{},"safe_to_remove":{}}}"#,
                    json_str(&d.info.dev),
                    json_str(&d.info.name()),
                    json_str(d.mode()),
                    json_str(d.state_name()),
                    d.mountpoint.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
                    d.dirty_bytes,
                    d.dirty_files,
                    (d.mode() == "flashcp" && !d.dirty()) || d.state == State::Done { safe: true },
                )
            })
            .collect();
        format!(r#"{{"ok":true,"enabled":{},"drives":[{}]}}"#, self.enabled, drives.join(","))
    }
}

fn kernel(d: &mut Drive, read_only: bool, out: &mut Vec<Action>) {
    d.state = State::KernelMounting { read_only };
    out.push(Action::KernelMount { dev: d.info.dev.clone(), read_only });
}

/// The policy for a check/mount that did not serve the volume (docs/drive-mode-interface.md).
fn refused(d: &mut Drive, code: i32, _reason: Option<String>, out: &mut Vec<Action>) {
    let name = d.info.name();
    match code {
        exit::BUSY => {
            let tries = d.busy_tries;
            if tries < BUSY_RETRY_MS.len() {
                d.timer_gen += 1;
                d.busy_tries += 1;
                d.state = State::BusyWait { tries: tries + 1 };
                out.push(Action::Timer { dev: d.info.dev.clone(), ms: BUSY_RETRY_MS[tries], seq: d.timer_gen });
            } else {
                out.push(notify(
                    d,
                    "failed",
                    format!("{name} is in use"),
                    format!("Another program is using {name}, so it was not opened. Unplug it and plug it in again."),
                    Urgency::Normal,
                ));
                d.state = State::Failed { message: "in use by another program".into() };
            }
        }
        exit::RECOVERY_REFUSED => {
            out.push(notify(
                d,
                "failed",
                format!("{name} needs a check"),
                format!(
                    "An interrupted save on {name} could not be completed safely, so it is opened read-only. \
                     Run `fsck.exfat {}` (or check it on Windows) to repair it.",
                    d.info.dev
                ),
                Urgency::Critical,
            ));
            kernel(d, true, out);
        }
        // 3 (refused: not exFAT, dirty, damaged, unsupported), 5 (no access), anything unknown.
        _ => kernel(d, false, out),
    }
}

fn notify(d: &Drive, kind: &str, title: String, body: String, urgency: Urgency) -> Action {
    Action::Notify { key: format!("{}:{kind}", d.info.dev), title, body, urgency }
}

fn saving(d: &Drive) -> Action {
    let name = d.info.name();
    notify(
        d,
        "saving",
        format!("Saving {} to {name}…", mb(d.dirty_bytes)),
        "Don't remove the drive yet: you'll be told when it can be removed.".into(),
        Urgency::Normal,
    )
}

/// The mountpoint's file name, as udisks would choose it: the label (a `/` becomes `_`), else the
/// UUID, else the device name. The privileged side picks `NAME1`, `NAME2`, ... on a clash.
pub fn mountpoint_name(info: &DevInfo) -> String {
    let n: String = info.name().chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let n = n.trim().trim_start_matches('.').to_string();
    if n.is_empty() { "disk".into() } else { n }
}

/// A minimal parser for the driver's one-line, flat JSON objects. Values come back as strings
/// (numbers, `true`/`false`/`null` as their literal text); nested values are skipped.
pub mod json {
    use std::collections::BTreeMap;

    pub fn parse_flat(s: &str) -> Option<BTreeMap<String, String>> {
        let mut p = P { b: s.trim().as_bytes(), i: 0 };
        let mut m = BTreeMap::new();
        p.eat(b'{')?;
        p.ws();
        if p.peek() == Some(b'}') {
            return Some(m);
        }
        loop {
            p.ws();
            let k = p.string()?;
            p.ws();
            p.eat(b':')?;
            p.ws();
            match p.peek()? {
                b'"' => {
                    let v = p.string()?;
                    m.insert(k, v);
                }
                b'{' | b'[' => p.skip_nested()?,
                _ => {
                    let st = p.i;
                    while p.i < p.b.len() && !matches!(p.b[p.i], b',' | b'}' | b' ' | b'\t') {
                        p.i += 1;
                    }
                    m.insert(k, String::from_utf8_lossy(&p.b[st..p.i]).into_owned());
                }
            }
            p.ws();
            match p.next()? {
                b',' => continue,
                b'}' => return Some(m),
                _ => return None,
            }
        }
    }

    struct P<'a> {
        b: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn peek(&self) -> Option<u8> {
            self.b.get(self.i).copied()
        }
        fn next(&mut self) -> Option<u8> {
            let c = self.peek()?;
            self.i += 1;
            Some(c)
        }
        fn eat(&mut self, c: u8) -> Option<()> {
            (self.next()? == c).then_some(())
        }
        fn ws(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.i += 1;
            }
        }
        fn string(&mut self) -> Option<String> {
            self.eat(b'"')?;
            let mut out = Vec::new();
            loop {
                match self.next()? {
                    b'"' => return String::from_utf8(out).ok(),
                    b'\\' => match self.next()? {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let h = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
                            self.i += 4;
                            let mut cp = u32::from_str_radix(h, 16).ok()?;
                            if (0xD800..0xDC00).contains(&cp) && self.b.get(self.i..self.i + 2) == Some(b"\\u") {
                                let l = std::str::from_utf8(self.b.get(self.i + 2..self.i + 6)?).ok()?;
                                let lo = u32::from_str_radix(l, 16).ok()?;
                                self.i += 6;
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            }
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(
                                char::from_u32(cp).unwrap_or('\u{fffd}').encode_utf8(&mut buf).as_bytes(),
                            );
                        }
                        c => out.push(c),
                    },
                    c => out.push(c),
                }
            }
        }
        fn skip_nested(&mut self) -> Option<()> {
            let mut depth = 0usize;
            loop {
                match self.peek()? {
                    b'"' => {
                        self.string()?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            self.i += 1;
                            return Some(());
                        }
                    }
                    _ => {}
                }
                self.i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(dev: &str, label: &str) -> DevInfo {
        DevInfo {
            dev: dev.into(),
            disk: dev.trim_end_matches(char::is_numeric).into(),
            label: Some(label.into()),
            uuid: Some("1A2B-3C4D".into()),
            mounted_at: None,
        }
    }
    fn ev(dev: &str, line: &str) -> Input {
        Input::Driver { dev: dev.into(), ev: DriverEvent::parse(line) }
    }
    fn exit_(dev: &str, code: i32) -> Input {
        Input::DriverExit { dev: dev.into(), code: Some(code) }
    }
    fn check(dev: &str, code: i32) -> Input {
        Input::CheckExit { dev: dev.into(), code, reason: None }
    }
    fn mp(dev: &str) -> Input {
        Input::MountpointReady { dev: dev.into(), result: Ok("/media/u/K".into()) }
    }
    fn reply(dev: &str, cmd: &str, ok: bool) -> Input {
        Input::ControlReply { dev: dev.into(), cmd: cmd.into(), ok, error: None }
    }
    fn added(dev: &str) -> Input {
        Input::Added(info(dev, "K"))
    }
    const D: &str = "/dev/sdb1";
    const MOUNTED: &str = r#"{"event":"mounted","proto":1,"device":"/dev/sdb1","recovered":null}"#;
    const DIRTY: &str = r#"{"event":"dirty_bytes","bytes":5000000,"files":3}"#;
    const CLEAN: &str = r#"{"event":"dirty_bytes","bytes":0,"files":0}"#;

    /// The common prefix: plug in, check ok, mountpoint ready, the driver reports mounted.
    fn serving() -> Vec<(Input, &'static str, &'static str)> {
        vec![
            (added(D), "check /dev/sdb1", "checking"),
            (check(D, 0), "mkmp /dev/sdb1 K", "starting"),
            (mp(D), "start /dev/sdb1 /media/u/K", "starting"),
            (ev(D, MOUNTED), "", "ready"),
        ]
    }

    /// One table for every transition. Each step: an input, the actions it must produce (joined
    /// with "; ", in order, exactly), and the drive's state name afterwards ("-" = forgotten).
    #[test]
    fn transitions() {
        type Step = (Input, &'static str, &'static str);
        let with = |extra: Vec<Step>| -> Vec<Step> { serving().into_iter().chain(extra).collect() };
        let table: Vec<(&str, bool, Vec<Step>)> = vec![
            ("new drive, check ok, served", true, serving()),
            (
                "check refused (not exFAT/dirty/invalid): kernel rw",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 3), "kernel /dev/sdb1 rw", "mounting"),
                    (Input::KernelMounted { dev: D.into(), result: Ok("/media/u/K".into()) }, "", "kernel"),
                    (Input::Eject { dev: D.into() }, "kernel-eject /dev/sdb1 /dev/sdb", "safe_to_remove"),
                    (
                        Input::KernelUnmountFailed { dev: D.into(), error: "target is busy".into() },
                        "notify /dev/sdb1:removed",
                        "kernel",
                    ),
                    // Unmounted behind our back: re-check, and a holder refuses the power-off.
                    (Input::Eject { dev: D.into() }, "kernel-eject /dev/sdb1 /dev/sdb", "safe_to_remove"),
                    (Input::KernelNotMounted { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                    (check(D, 4), "notify /dev/sdb1:removed", "failed"),
                    (Input::Removed { dev: D.into() }, "", "-"),
                ],
            ),
            (
                "no access: kernel rw; recovery refused: kernel ro + notification",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 5), "kernel /dev/sdb1 rw", "mounting"),
                    (Input::Removed { dev: D.into() }, "", "-"),
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 6), "notify /dev/sdb1:failed; kernel /dev/sdb1 ro", "mounting"),
                    (Input::KernelMounted { dev: D.into(), result: Ok("/media/u/K".into()) }, "", "read_only"),
                ],
            ),
            (
                "busy: three retries, then notify only (never a kernel mount)",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 4), "timer 1000", "busy"),
                    (Input::Timer { dev: D.into(), seq: 0 }, "", "busy"), // stale timer
                    (Input::Timer { dev: D.into(), seq: 1 }, "check /dev/sdb1", "checking"),
                    (check(D, 4), "timer 3000", "busy"),
                    (Input::Timer { dev: D.into(), seq: 2 }, "check /dev/sdb1", "checking"),
                    (check(D, 4), "timer 10000", "busy"),
                    (Input::Timer { dev: D.into(), seq: 3 }, "check /dev/sdb1", "checking"),
                    (check(D, 4), "notify /dev/sdb1:failed", "failed"),
                    // Eject after the retries ran out: re-check first; anything but a clean "nothing
                    // holds it" refuses (busy, an error, an unknown code, a spawn failure,
                    // busy_unknown).
                    (Input::Eject { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                    (
                        Input::CheckExit { dev: D.into(), code: 1, reason: Some("I/O error".into()) },
                        "notify /dev/sdb1:removed",
                        "failed",
                    ),
                    (Input::Eject { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                    (check(D, 42), "notify /dev/sdb1:removed", "failed"),
                    (Input::Eject { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                    (
                        Input::CheckExit { dev: D.into(), code: 1, reason: Some("spawn: No such file".into()) },
                        "notify /dev/sdb1:removed",
                        "failed",
                    ),
                    (Input::Eject { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                    (
                        Input::CheckExit { dev: D.into(), code: 0, reason: Some("busy_unknown".into()) },
                        "notify /dev/sdb1:removed",
                        "failed",
                    ),
                    (Input::Eject { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                    (check(D, 4), "notify /dev/sdb1:removed", "failed"),
                    // Released since: the udisks path (unmount + power off), never a bare power-off.
                    (Input::Eject { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                    (check(D, 0), "kernel-eject /dev/sdb1 /dev/sdb checked", "safe_to_remove"),
                ],
            ),
            (
                "already kernel-mounted at startup: left alone",
                true,
                vec![(Input::Added(DevInfo { mounted_at: Some("/media/u/K".into()), ..info(D, "K") }), "", "kernel")],
            ),
            (
                "mount refuses at start (exit 3 after a good check): kernel",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 0), "mkmp /dev/sdb1 K", "starting"),
                    (mp(D), "start /dev/sdb1 /media/u/K", "starting"),
                    (exit_(D, 3), "rmmp /media/u/K; kernel /dev/sdb1 rw", "mounting"),
                ],
            ),
            (
                "no mountpoint (helper refused): kernel",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 0), "mkmp /dev/sdb1 K", "starting"),
                    (
                        Input::MountpointReady { dev: D.into(), result: Err("no".into()) },
                        "kernel /dev/sdb1 rw",
                        "mounting",
                    ),
                ],
            ),
            (
                "crash: restart once (recovery), second crash: kernel + notification",
                true,
                with(vec![
                    (ev(D, DIRTY), "inhibit on", "saving"),
                    (
                        Input::DriverExit { dev: D.into(), code: None },
                        "notify /dev/sdb1:lost; start /dev/sdb1 /media/u/K; inhibit off",
                        "starting",
                    ),
                    (
                        ev(D, r#"{"event":"mounted","proto":1,"recovered":"replayed"}"#),
                        "notify /dev/sdb1:recovered",
                        "ready",
                    ),
                    (exit_(D, 1), "rmmp /media/u/K; notify /dev/sdb1:failed; kernel /dev/sdb1 rw", "mounting"),
                ]),
            ),
            (
                "eject: prepare-eject, safe, unmount, exit 0, power off",
                true,
                with(vec![
                    (ev(D, DIRTY), "inhibit on", "saving"),
                    (
                        Input::Eject { dev: D.into() },
                        "notify /dev/sdb1:saving; ctl /dev/sdb1 prepare-eject",
                        "ejecting",
                    ),
                    (
                        ev(D, r#"{"event":"committing","bytes":5000000,"files":3,"reason":"eject"}"#),
                        "notify /dev/sdb1:saving",
                        "ejecting",
                    ),
                    (ev(D, r#"{"event":"committed","bytes":5000000,"files":3,"secs":0.5}"#), "", "ejecting"),
                    (ev(D, CLEAN), "inhibit off", "ejecting"),
                    (reply(D, "prepare-eject", true), "ctl /dev/sdb1 unmount", "ejecting"),
                    (ev(D, r#"{"event":"unmounted","clean":true}"#), "", "ejecting"),
                    (exit_(D, 0), "rmmp /media/u/K; poweroff /dev/sdb", "safe_to_remove"),
                    (Input::PoweredOff { dev: D.into(), ok: true }, "notify /dev/sdb1:removed", "safe_to_remove"),
                    (Input::Removed { dev: D.into() }, "", "-"),
                ]),
            ),
            (
                "eject refused by the driver: resume, still mounted",
                true,
                with(vec![
                    (Input::Eject { dev: D.into() }, "ctl /dev/sdb1 prepare-eject", "ejecting"),
                    (reply(D, "prepare-eject", false), "ctl /dev/sdb1 resume; notify /dev/sdb1:saving", "ready"),
                ]),
            ),
            (
                "unmounted from the file manager: saving notification, then power off",
                true,
                with(vec![
                    (ev(D, DIRTY), "inhibit on", "saving"),
                    (
                        ev(D, r#"{"event":"committing","bytes":5000000,"files":3,"reason":"unmount"}"#),
                        "notify /dev/sdb1:saving",
                        "ejecting",
                    ),
                    (ev(D, r#"{"event":"committed","bytes":5000000,"files":3,"secs":0.5}"#), "", "ejecting"),
                    (ev(D, CLEAN), "inhibit off", "ejecting"),
                    (exit_(D, 0), "rmmp /media/u/K; poweroff /dev/sdb", "safe_to_remove"),
                ]),
            ),
            (
                "yank while dirty: warning, driver stopped, mountpoint removed, replug recovers",
                true,
                with(vec![
                    (ev(D, DIRTY), "inhibit on", "saving"),
                    (
                        Input::Removed { dev: D.into() },
                        "notify /dev/sdb1:saving; stop /dev/sdb1; inhibit off",
                        "saving",
                    ),
                    (exit_(D, 1), "rmmp /media/u/K", "-"),
                    (added(D), "check /dev/sdb1", "checking"),
                ]),
            ),
            (
                "yank while clean: no warning",
                true,
                with(vec![
                    (Input::Removed { dev: D.into() }, "stop /dev/sdb1", "ready"),
                    (exit_(D, 1), "rmmp /media/u/K", "-"),
                ]),
            ),
            (
                "off switch: flush+unmount (SIGTERM), then kernel; new drives get the kernel",
                true,
                with(vec![
                    (Input::SetEnabled(false), "stop /dev/sdb1", "stopping"),
                    (ev(D, r#"{"event":"unmounted","clean":true}"#), "", "stopping"),
                    (exit_(D, 0), "rmmp /media/u/K; kernel /dev/sdb1 rw", "mounting"),
                    (Input::Added(info("/dev/sdc1", "L")), "kernel /dev/sdc1 rw", "mounting"),
                    (Input::SetEnabled(true), "", "mounting"),
                ]),
            ),
            (
                "off switch, but the driver could not finish (or never said unmounted): kernel READ-ONLY",
                true,
                with(vec![
                    (ev(D, DIRTY), "inhibit on", "saving"),
                    (Input::SetEnabled(false), "stop /dev/sdb1", "stopping"),
                    (
                        exit_(D, 1),
                        "rmmp /media/u/K; notify /dev/sdb1:failed; kernel /dev/sdb1 ro; inhibit off",
                        "mounting",
                    ),
                ]),
            ),
            (
                "off switch: exit 0 but no `unmounted` event: read-only",
                true,
                with(vec![
                    (Input::SetEnabled(false), "stop /dev/sdb1", "stopping"),
                    (exit_(D, 0), "rmmp /media/u/K; notify /dev/sdb1:failed; kernel /dev/sdb1 ro", "mounting"),
                ]),
            ),
            (
                "off while the check runs: the check result is ignored, kernel",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (Input::SetEnabled(false), "", "checking"),
                    (check(D, 0), "kernel /dev/sdb1 rw", "mounting"),
                ],
            ),
            ("disabled from the start: kernel", false, vec![(added(D), "kernel /dev/sdb1 rw", "mounting")]),
            (
                "sleep while dirty: flush, inhibitor released once clean; resume",
                true,
                with(vec![
                    (ev(D, DIRTY), "inhibit on", "saving"),
                    (Input::Sleep(true), "ctl /dev/sdb1 flush", "saving"),
                    (ev(D, CLEAN), "inhibit off", "ready"),
                    (Input::Sleep(false), "", "ready"),
                    (ev(D, DIRTY), "inhibit on", "saving"),
                ]),
            ),
            (
                "daemon stop (logout): SIGTERM every driver, exit after they are done",
                true,
                with(vec![
                    (Input::Shutdown, "stop /dev/sdb1", "stopping"),
                    (exit_(D, 0), "rmmp /media/u/K; rescan /dev/sdb1; exit", "safe_to_remove"),
                ]),
            ),
            ("duplicate add (change event) is ignored", true, with(vec![(added(D), "", "ready")])),
            (
                "two partitions of one disk: no power-off while the other one is served",
                true,
                with(vec![
                    (Input::Added(info("/dev/sdb2", "L")), "check /dev/sdb2", "ready"),
                    (
                        Input::CheckExit { dev: "/dev/sdb2".into(), code: 3, reason: None },
                        "kernel /dev/sdb2 rw",
                        "ready",
                    ),
                    (Input::KernelMounted { dev: "/dev/sdb2".into(), result: Ok("/media/u/L".into()) }, "", "ready"),
                    (Input::Eject { dev: D.into() }, "ctl /dev/sdb1 prepare-eject", "ejecting"),
                    (reply(D, "prepare-eject", true), "ctl /dev/sdb1 unmount", "ejecting"),
                    (exit_(D, 0), "rmmp /media/u/K; notify /dev/sdb1:removed", "safe_to_remove"),
                ]),
            ),
            (
                "busy drive: eject refused (another program holds it); logout hands it back",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 4), "timer 1000", "busy"),
                    (Input::Eject { dev: D.into() }, "", "busy"),
                    (Input::Shutdown, "rescan /dev/sdb1; exit", "busy"),
                ],
            ),
        ];

        for (name, enabled, steps) in table {
            let mut core = Core::new(enabled);
            for (i, (input, want, state)) in steps.into_iter().enumerate() {
                let dbg = format!("{input:?}");
                let got: Vec<String> = core.handle(input).iter().map(|a| a.to_string()).collect();
                assert_eq!(got.join("; "), want, "{name}: step {i} ({dbg})");
                let target = if dbg.contains("sdc1") { "/dev/sdc1" } else { D };
                let st = core.drives.get(target).map(Drive::state_name).unwrap_or("-");
                assert_eq!(st, state, "{name}: state after step {i} ({dbg})");
            }
            let s = core.status_json();
            assert!(json::parse_flat(&s).is_some(), "{name}: status is JSON: {s}");
        }
    }

    #[test]
    fn json_and_names() {
        let o = json::parse_flat(
            r#"{"event":"error","message":"a \"b\" é😀","fatal":true,"x":{"y":[1,{"z":2}]},"n":null}"#,
        )
        .unwrap();
        assert_eq!(o["message"], "a \"b\" é😀");
        assert_eq!(o["fatal"], "true");
        assert_eq!(o["n"], "null");
        assert!(!o.contains_key("x"));
        assert_eq!(DriverEvent::parse("not json"), DriverEvent::Other);
        let mut i = info(D, "a/b");
        assert_eq!(mountpoint_name(&i), "a_b");
        i.label = Some(" ".into());
        assert_eq!(mountpoint_name(&i), "1A2B-3C4D");
        i.uuid = None;
        assert_eq!(mountpoint_name(&i), "sdb1");
        i.label = Some("..x".into());
        assert_eq!(mountpoint_name(&i), "x");
    }
}
