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
//! - "Make this stick faster" (`flashcp recluster`): after a good `--check`, `recluster --check`
//!   says whether the volume would gain from smaller clusters. It is never run without a click:
//!   once per volume (key [`volume_key`]) a notification with Accept / Not now offers it, and the
//!   app and `flashcp drive-mode recluster` can ask for it any time. Accepted: `prepare-eject` →
//!   `unmount` → a Clean [`handover`] → `flashcp recluster` → flashcp mounts it again. An
//!   interrupted conversion (`"finish":true`) is completed once, before mounting, without asking.

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

/// The notification offers a conversion only from this cluster size up. `mkfs.exfat`, Windows
/// and macOS give every stick from 256 MB to 32 GB 32 KB clusters and bigger ones 128 KB, so
/// this covers every stick formatted the usual way; 16 KB (which `recluster --check` also
/// accepts) is a deliberate choice somebody made, and gains less: the app and the CLI still
/// offer it there.
pub const OFFER_MIN_CLUSTER: u64 = 32 << 10;

/// What `flashcp recluster --check --json` said.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ReclusterCheck {
    /// Exit 0 and `"benefit":true`: a conversion can run.
    pub benefit: bool,
    pub reason: String,
    pub cluster: u64,
    /// `est_secs` rounded up.
    pub secs: u64,
    /// An interrupted conversion left its last write to do.
    pub finish: bool,
}

impl ReclusterCheck {
    /// Parse the `check` line (None if it is not one).
    pub fn parse(line: &str, code: Option<i32>) -> Option<ReclusterCheck> {
        let o = json::parse_flat(line)?;
        if o.get("event").map(String::as_str) != Some("check") || o.get("op").map(String::as_str) != Some("recluster") {
            return None;
        }
        let num = |k: &str| o.get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        Some(ReclusterCheck {
            benefit: code == Some(0) && o.get("benefit").is_some_and(|v| v == "true"),
            reason: o.get("reason").cloned().unwrap_or_default(),
            cluster: num("cluster") as u64,
            secs: (num("est_secs").ceil() as u64).max(1),
            finish: o.get("finish").is_some_and(|v| v == "true"),
        })
    }
}

/// A JSON line from `flashcp recluster --json`, reduced to what the daemon uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReclusterEvent {
    Plan { secs: u64 },
    Progress { done: u64, total: u64 },
    Done,
    Refused { reason: String, message: String },
    Error { message: String },
    Other,
}

impl ReclusterEvent {
    pub fn parse(line: &str) -> ReclusterEvent {
        let Some(o) = json::parse_flat(line) else { return ReclusterEvent::Other };
        let num = |k: &str| o.get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let text = |k: &str| o.get(k).cloned().unwrap_or_default();
        match o.get("event").map(String::as_str) {
            Some("plan") => ReclusterEvent::Plan { secs: num("est_secs").ceil() as u64 },
            Some("progress") => {
                ReclusterEvent::Progress { done: num("bytes_done") as u64, total: num("bytes_total") as u64 }
            }
            Some("done") => ReclusterEvent::Done,
            Some("refused") => ReclusterEvent::Refused { reason: text("reason"), message: text("message") },
            Some("error") => ReclusterEvent::Error { message: text("message") },
            _ => ReclusterEvent::Other,
        }
    }
}

/// The user's answer to the offer notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Accept,
    NotNow,
    /// Closed without a choice (dismissed, or it expired).
    Dismissed,
    /// No answer: the server cannot show buttons, there is no notification server, or the
    /// daemon closed it itself (the drive was removed). Nothing is remembered.
    None,
}

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
    /// The volume has an interrupted flashcp commit (udev: `FLASHCP_RECOVER=1`): flashcp mounts
    /// it once to recover, even with drive mode off, then hands it over.
    pub recover: bool,
    /// The disk's hardware serial (udev `ID_SERIAL_SHORT`, else `ID_SERIAL`), if it has one.
    pub disk_serial: Option<String>,
}

impl DevInfo {
    /// The name users see: the label (control characters and newlines dropped), else the
    /// serial, else the device name.
    pub fn name(&self) -> String {
        self.label
            .as_deref()
            .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
            .filter(|l| !l.trim().is_empty())
            .or_else(|| self.uuid.clone())
            .unwrap_or_else(|| self.dev.rsplit('/').next().unwrap_or(&self.dev).to_string())
    }
}

/// A JSON event from `flashcp mount --json`, reduced to what the daemon uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverEvent {
    Mounted {
        recovered: Option<String>,
    },
    DirtyBytes {
        bytes: u64,
        files: u64,
    },
    /// `by`: the process behind an fsync's commit (from 0.4.1, best effort).
    Committing {
        bytes: u64,
        reason: String,
        by: Option<String>,
    },
    Committed,
    Error {
        message: String,
        fatal: bool,
    },
    Unmounted {
        clean: bool,
    },
    Refused {
        reason: String,
    },
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
            Some("committing") => {
                DriverEvent::Committing { bytes: num("bytes"), reason: text("reason"), by: o.get("by").cloned() }
            }
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
    /// `flashcp recluster --check` finished (None: it failed to run or said nothing usable).
    ReclusterChecked {
        dev: String,
        check: Option<ReclusterCheck>,
    },
    Recluster {
        dev: String,
        ev: ReclusterEvent,
    },
    /// `flashcp recluster` exited (None: killed by a signal).
    ReclusterExit {
        dev: String,
        code: Option<i32>,
    },
    /// "Make faster" from the app or `flashcp drive-mode recluster`, for the plug-in `plug`
    /// whose texts the user saw (see [`Drive::plug`]).
    Convert {
        dev: String,
        plug: String,
        /// The [`drive_identity`] the user was shown.
        identity: String,
        /// The identity of what is at `dev` now, read from the device and sysfs when the request
        /// came (None: it could not be read).
        live: Option<String>,
    },
    /// The answer to the offer made for plug-in `plug` of the drive `identity`.
    OfferAnswer {
        dev: String,
        plug: String,
        identity: String,
        /// As for [`Input::Convert`], read when the answer came.
        live: Option<String>,
        answer: Answer,
    },
    /// The answer could not be saved (the offer may come back after a restart).
    RememberFailed {
        dev: String,
    },
    /// The offer notification on or off (`flashcp drive-mode offer on|off`).
    SetOffers(bool),
    /// Where the kernel driver has the drive mounted now (None: not mounted any more, e.g. after
    /// `udisksctl unmount`). Only looked at for drives in the kernel state.
    KernelMountSeen {
        dev: String,
        at: Option<String>,
    },
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
    /// `flashcp recluster --check --json DEV`.
    ReclusterCheck {
        dev: String,
    },
    /// `flashcp recluster --json DEV` (the driver is not running; recluster takes the claim).
    /// `serial`: `--expect-serial`, checked on the claimed fd before the first write.
    Recluster {
        dev: String,
        serial: Option<String>,
        /// `--expect-hw`: the hardware serial, checked through the claimed fd.
        hw: Option<String>,
    },
    /// A notification with Accept / Not now buttons for plug-in `plug`; the answer comes back
    /// as [`Input::OfferAnswer`] with the same `plug`.
    Offer {
        dev: String,
        plug: String,
        identity: String,
        title: String,
        body: String,
    },
    /// Close the offer notification of that plug-in if it is still up.
    CloseOffer {
        dev: String,
        plug: String,
    },
    /// Persist the answer for this volume (`$XDG_STATE_HOME/flashcp/recluster-offers`).
    Remember {
        dev: String,
        volume: String,
        answer: &'static str,
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
            Action::ReclusterCheck { dev } => write!(f, "recluster-check {dev}"),
            Action::Recluster { dev, .. } => write!(f, "recluster {dev}"),
            Action::Offer { dev, .. } => write!(f, "offer {dev}"),
            Action::CloseOffer { dev, .. } => write!(f, "close-offer {dev}"),
            Action::Remember { volume, answer, .. } => write!(f, "remember {volume} {answer}"),
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

/// Where a conversion (`flashcp recluster`) is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conv {
    /// `prepare-eject` sent: writes are refused, everything pending is being committed.
    Flushing,
    /// `unmount` sent: waiting for the driver's clean exit.
    Stopping,
    /// `flashcp recluster` runs (`finish`: completing an interrupted one before mounting).
    Running { finish: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Checking,
    /// `flashcp recluster --check` runs (after a good `mount --check`).
    ReclusterChecking,
    /// Making the volume faster; the volume is not mounted from `Stopping` on.
    Converting(Conv),
    /// It was kernel-mounted and someone unmounted it (`udisksctl unmount`, a file manager).
    Unmounted,
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
    /// A driver reported `mounted` for this plug-in at least once.
    ever_mounted: bool,
    /// `recluster --check` said a conversion can run: (cluster size, about how many seconds).
    pub can_convert: Option<(u64, u64)>,
    /// The offer notification was shown for this plug-in / is still up.
    offered: bool,
    offering: bool,
    /// The interrupted conversion was completed (or tried) for this plug-in.
    finish_tried: bool,
    /// Progress of a running conversion (bytes done, bytes in total) and its estimate.
    pub conv_done: u64,
    pub conv_total: u64,
    pub conv_secs: u64,
    /// An eject came while converting: power off afterwards instead of mounting again.
    eject_after: bool,
    /// The last conversion's message from `flashcp recluster` (refusal or error text).
    conv_message: Option<(String, String)>,
    /// "done" | "refused" | "failed" and the text the user was shown, for the app and the CLI.
    pub conv_result: Option<(&'static str, String)>,
    /// The notification to show once the driver has mounted it again.
    after_mount: Option<Action>,
    /// This plug-in's token: 128 random bits from the OS, new for every `Added`, so it is
    /// unique across daemon restarts too. Offers, the app's dialog and `drive-mode recluster`
    /// carry it (and the [`drive_identity`] they showed), so an answer given for one stick never
    /// reaches another one that got the same device path.
    pub plug: String,
}

impl Drive {
    fn dirty(&self) -> bool {
        self.dirty_bytes > 0 || self.dirty_files > 0 || self.committing
    }
    fn driver_running(&self) -> bool {
        matches!(
            self.state,
            State::Starting
                | State::Mounted
                | State::Ejecting
                | State::Unmounting
                | State::Stopping(_)
                | State::Converting(Conv::Flushing | Conv::Stopping)
        )
    }
    /// A process of ours (the driver or `flashcp recluster`) works on the device.
    fn busy(&self) -> bool {
        self.driver_running() || matches!(self.state, State::Converting(_))
    }
    /// "flashcp", "kernel" or "none": who serves the volume right now.
    pub fn mode(&self) -> &'static str {
        match self.state {
            State::Starting
            | State::Mounted
            | State::Ejecting
            | State::Unmounting
            | State::Stopping(_)
            | State::Converting(_) => "flashcp",
            State::Kernel { .. } | State::KernelMounting { .. } => "kernel",
            _ => "none",
        }
    }
    pub fn state_name(&self) -> &'static str {
        match self.state {
            State::Checking | State::ReclusterChecking => "checking",
            State::Converting(_) => "converting",
            State::Unmounted => "unmounted",
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
    /// The offer notification is on (`offer_faster=false` in drive-mode.conf turns it off).
    pub offers: bool,
    /// Volumes ([`volume_key`]) whose offer was answered: never offered by notification again.
    pub answered: std::collections::BTreeSet<String>,
    /// Tests only: deterministic tokens `t1`, `t2`, ... instead of random ones.
    pub test_tokens: Option<u64>,
    /// "Your answer could not be saved" was said once already.
    told_unsaved: bool,
}

/// 128 random bits from the OS, as 32 hex digits.
/// None when the OS gives no random bytes (neither getrandom nor /dev/urandom).
pub fn random_token() -> Option<String> {
    #[cfg(test)]
    if NO_RANDOM.with(|c| c.get()) {
        return None;
    }
    let mut b = [0u8; 16];
    let mut got = 0;
    while got < b.len() {
        let n = unsafe { libc::getrandom(b[got..].as_mut_ptr().cast(), b.len() - got, 0) };
        if n > 0 {
            got += n as usize;
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // No getrandom: the kernel's pool through the device instead.
            use std::io::Read;
            std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).ok()?;
            break;
        }
    }
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(test)]
thread_local! {
    /// Tests: [`random_token`] finds no random bytes.
    static NO_RANDOM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What identifies the stick a confirmation was made for: the volume serial and the disk's
/// hardware serial (empty when absent), as `UUID:SERIAL`. Whitespace and control characters
/// are replaced, so it is one word on the control socket.
pub fn drive_identity(info: &DevInfo) -> String {
    let clean = |s: Option<&str>| -> String {
        s.unwrap_or("").chars().map(|c| if c.is_whitespace() || c.is_control() { '_' } else { c }).collect()
    };
    format!("{}:{}", clean(info.uuid.as_deref()), clean(info.disk_serial.as_deref()))
}

/// What a conversion request for a stick that is no longer there gets.
pub const REPLACED: &str = "That drive was replaced. Nothing was changed.";

/// The identity the offer is remembered by: `UUID:HWSERIAL:` (the volume serial and the disk's
/// hardware serial; a relabel keeps it, two cloned sticks differ), or `UUID::LABEL` when the
/// disk reports no hardware serial (then two clones of one image count as one stick). Both
/// survive a conversion; a reformat gives a new volume serial. None without a volume serial.
pub fn volume_key(info: &DevInfo) -> Option<String> {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let uuid = info.uuid.as_deref().filter(|u| !u.trim().is_empty())?;
    Some(match info.disk_serial.as_deref().filter(|s| !s.is_empty()) {
        Some(hw) => format!("{}:{}:", clean(uuid), clean(hw)),
        None => format!("{}::{}", clean(uuid), clean(info.label.as_deref().unwrap_or(""))),
    })
}

/// A key as an earlier build of this branch wrote it (`UUID:HWSERIAL:LABEL`), in today's form.
pub fn normalize_key(k: &str) -> String {
    let mut p = k.splitn(3, ':');
    match (p.next(), p.next(), p.next()) {
        (Some(u), Some(hw), Some(_)) if !hw.is_empty() => format!("{u}:{hw}:"),
        _ => k.to_string(),
    }
}

pub fn mb(bytes: u64) -> String {
    let m = bytes as f64 / 1e6;
    if m >= 10.0 { format!("{m:.0} MB") } else { format!("{m:.1} MB") }
}

impl Core {
    pub fn new(enabled: bool) -> Core {
        Core {
            enabled,
            drives: BTreeMap::new(),
            inhibiting: false,
            sleeping: false,
            shutting_down: false,
            offers: true,
            answered: Default::default(),
            test_tokens: None,
            told_unsaved: false,
        }
    }

    /// Feed one input, get the actions to perform (in order).
    pub fn handle(&mut self, input: Input) -> Vec<Action> {
        let mut out = Vec::new();
        self.step(input, &mut out);
        self.update_inhibitor(&mut out);
        if self.shutting_down && !self.drives.values().any(Drive::busy) && !out.contains(&Action::Exit) {
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
                    // An offer can never be answered once this daemon is gone: take it down.
                    if d.offering {
                        d.offering = false;
                        out.push(Action::CloseOffer { dev: d.info.dev.clone(), plug: d.plug.clone() });
                    }
                    match d.state {
                        // One SIGTERM per driver: ejecting/unmounting/stopping ones are on their way.
                        State::Starting | State::Mounted if !d.gone => {
                            d.state = State::Stopping(Then::Exit);
                            out.push(Action::StopDriver { dev: d.info.dev.clone() });
                        }
                        // Hidden from udisks but not served: hand it back now.
                        State::Checking
                        | State::ReclusterChecking
                        | State::BusyWait { .. }
                        | State::MakingMountpoint
                        | State::EjectChecking
                        | State::Failed { .. } => handover(d, Outcome::NotServed, To::Release, false, out),
                        _ => {}
                    }
                }
            }
            Input::Removed { dev } => self.removed(&dev, out),
            Input::ReclusterChecked { dev, check } => self.recluster_checked(&dev, check, out),
            Input::Recluster { dev, ev } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                match ev {
                    ReclusterEvent::Plan { secs } => d.conv_secs = secs.max(1),
                    ReclusterEvent::Progress { done, total } => (d.conv_done, d.conv_total) = (done, total),
                    ReclusterEvent::Refused { reason, message } => d.conv_message = Some((reason, message)),
                    ReclusterEvent::Error { message } => d.conv_message = Some((String::new(), message)),
                    ReclusterEvent::Done | ReclusterEvent::Other => {}
                }
            }
            Input::ReclusterExit { dev, code } => self.recluster_exit(&dev, code, out),
            Input::Convert { dev, plug, identity, live } => {
                if self.convert_blocker(&dev, &plug, &identity, live.as_deref()).is_none() {
                    self.start_convert(&dev, out);
                }
            }
            Input::RememberFailed { dev } => {
                if self.told_unsaved {
                    return;
                }
                self.told_unsaved = true;
                let Some(d) = self.drives.get(&dev) else { return };
                out.push(notify(
                    d,
                    "offer",
                    "flashcp could not save your answer".into(),
                    format!("It may ask about {} again.", d.info.name()),
                    Urgency::Low,
                ));
            }
            Input::OfferAnswer { dev, plug, identity, live, answer } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                // Only the offer of this very plug-in of this drive, and only while that drive is
                // what the device at this path holds now (udev may not have told us of a swap yet).
                if !d.offering
                    || d.plug != plug
                    || drive_identity(&d.info) != identity
                    || live.as_deref() != Some(identity.as_str())
                {
                    return;
                }
                d.offering = false;
                let Some(key) = volume_key(&d.info) else { return };
                let word = match answer {
                    Answer::Accept => "accepted",
                    Answer::NotNow | Answer::Dismissed => "declined",
                    Answer::None => return,
                };
                self.answered.insert(key.clone());
                out.push(Action::Remember { dev: dev.clone(), volume: key, answer: word });
                if answer == Answer::Accept && self.convert_blocker(&dev, &plug, &identity, live.as_deref()).is_none() {
                    self.start_convert(&dev, out);
                }
            }
            Input::SetOffers(on) => self.offers = on,
            Input::KernelMountSeen { dev, at } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                match (&d.state, at) {
                    (State::Kernel { .. }, None) => {
                        d.state = State::Unmounted;
                        d.mountpoint = None;
                    }
                    (State::Kernel { .. } | State::Unmounted, Some(path)) => {
                        if d.state == State::Unmounted {
                            d.state = State::Kernel { read_only: d.kernel_ro };
                        }
                        d.mountpoint = Some(path);
                    }
                    _ => {}
                }
            }
            Input::CheckExit { dev, code, reason } => self.check_exit(&dev, code, reason, out),
            Input::MountpointReady { dev, result } => {
                let Some(d) = self.drives.get_mut(&dev) else { return };
                if d.state != State::MakingMountpoint {
                    return;
                }
                match result {
                    Ok(path) if (self.enabled || d.info.recover) && !self.shutting_down => {
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
                        handover(d, Outcome::NotServed, To::KernelEject { checked: false }, false, out);
                    }
                    // Eject waits for the conversion; then the drive is powered off, not remounted.
                    State::Converting(_) => d.eject_after = true,
                    // flashcp couldn't finish: nothing is mounted, no power-off, say so again.
                    State::Done { safe: false } => handover(d, Outcome::Unclean, To::PowerOff, false, out),
                    // Nothing of ours is mounted, but someone else may hold the device (the busy
                    // retries ran out) or have mounted it since: re-check holders and mounts first,
                    // then eject through udisks (unmount + power off), never a bare power-off.
                    State::Done { .. } | State::Failed { .. } | State::Unmounted if !sibling_active => {
                        d.state = State::EjectChecking;
                        out.push(Action::Check { dev });
                    }
                    _ => {}
                }
            }
            Input::ControlReply { dev, cmd, ok, error } => {
                let (shutting_down, enabled) = (self.shutting_down, self.enabled);
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
                    ("prepare-eject", State::Converting(Conv::Flushing)) if ok => {
                        d.state = State::Converting(Conv::Stopping);
                        out.push(Action::Control { dev, cmd: "unmount" });
                    }
                    ("prepare-eject", State::Converting(Conv::Flushing)) => {
                        let name = d.info.name();
                        let error = error.unwrap_or_else(|| "it could not be paused".into());
                        let body = format!(
                            "It is in use ({error}). Nothing was changed. Close the files on it and try again."
                        );
                        out.push(conv_finished(d, "failed", format!("{name} was not changed"), body));
                        // What came in meanwhile goes its normal way now.
                        if shutting_down || !enabled {
                            // SIGTERM: the driver commits and unmounts; its exit hands it over.
                            d.state = State::Stopping(if shutting_down { Then::Exit } else { Then::Kernel });
                            out.push(Action::StopDriver { dev });
                        } else if d.eject_after {
                            // The same pause failed, so the eject cannot go ahead either.
                            d.eject_after = false;
                            d.state = State::Mounted;
                            out.push(Action::Control { dev: dev.clone(), cmd: "resume" });
                            out.push(notify(
                                d,
                                "saving",
                                format!("Could not eject {name}"),
                                format!("{error} Nothing was lost; it is still in use."),
                                Urgency::Normal,
                            ));
                        } else {
                            d.state = State::Mounted;
                            out.push(Action::Control { dev, cmd: "resume" });
                        }
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
            ever_mounted: false,
            can_convert: None,
            offered: false,
            offering: false,
            finish_tried: false,
            conv_done: 0,
            conv_total: 0,
            conv_secs: 0,
            eject_after: false,
            conv_message: None,
            conv_result: None,
            after_mount: None,
            plug: String::new(),
        };
        d.plug = match self.test_tokens.as_mut() {
            Some(n) => {
                *n += 1;
                format!("t{n}")
            }
            // No token: the drive is served as usual, but it is never offered or converted.
            None => random_token().unwrap_or_else(|| {
                eprintln!("flashcp drived: no random bytes for {}: no offer to make it faster", d.info.dev);
                String::new()
            }),
        };
        if let Some(m) = d.info.mounted_at.clone() {
            // Already served by the kernel driver (mounted before the daemon started, or by hand).
            d.mountpoint = Some(m);
            d.state = State::Kernel { read_only: false };
        } else if self.shutting_down || !(self.enabled || d.info.recover) {
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
                handover(d, Outcome::NotServed, To::KernelEject { checked: true }, false, out);
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
        if !(enabled || d.info.recover && !self.shutting_down) {
            return kernel(d, false, out);
        }
        match code {
            // The check could not see holders/mounts: as good as busy.
            exit::OK if reason.as_deref() == Some("busy_unknown") => refused(d, exit::BUSY, reason, out),
            // Exit 0 without a proper `check` answer: not trusted, the kernel driver gets it.
            exit::OK if reason.is_some() => refused(d, exit::FALLBACK, reason, out),
            // Drive mode on: does it want a conversion, or the end of an interrupted one?
            exit::OK if enabled => {
                d.state = State::ReclusterChecking;
                out.push(Action::ReclusterCheck { dev: dev.into() });
            }
            exit::OK => to_mountpoint(d, out),
            _ => refused(d, code, reason, out),
        }
    }

    fn driver_event(&mut self, dev: &str, ev: DriverEvent, out: &mut Vec<Action>) {
        let Some(d) = self.drives.get_mut(dev) else { return };
        match ev {
            DriverEvent::Mounted { recovered } => {
                d.clean_unmount = false;
                d.ever_mounted = true;
                if d.state == State::Starting {
                    d.state = State::Mounted;
                    if !self.enabled {
                        // Mounted only to recover (drive mode off): hand it to the kernel now.
                        d.state = State::Stopping(Then::Kernel);
                        out.push(Action::StopDriver { dev: dev.into() });
                    } else if let Some(n) = d.after_mount.take() {
                        out.push(n);
                    } else if self.offers
                        && !d.plug.is_empty()
                        && !d.offered
                        && d.can_convert.is_some_and(|(cluster, _)| cluster >= OFFER_MIN_CLUSTER)
                        && volume_key(&d.info).is_some_and(|k| !self.answered.contains(&k))
                    {
                        d.offered = true;
                        d.offering = true;
                        out.push(offer(d));
                    }
                }
                if let Some(r) = recovered {
                    let body = if r == "replayed" {
                        "An interrupted save was completed: the files copied before it was unplugged are there."
                    } else if r == "abandoned" {
                        "The last save before it was unplugged was not completed (the drive was changed elsewhere \
                         since); the files saved before that are there."
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
            DriverEvent::Committing { bytes, reason, .. } => {
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
        // The evidence for a clean end: exit 0 AND a clean `unmounted` with nothing pending.
        let outcome = if code == Some(exit::OK) && d.clean_unmount { Outcome::Clean } else { Outcome::Unclean };
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
                    if enabled {
                        out.push(notify(
                            d,
                            "failed",
                            format!("{name}: fast mode stopped"),
                            format!("flashcp's driver failed twice, so {name} now uses the normal driver."),
                            Urgency::Critical,
                        ));
                    }
                    // A driver that never got as far as mounting wrote nothing.
                    let o = if d.ever_mounted { Outcome::Unclean } else { Outcome::NotServed };
                    handover(d, o, To::Kernel { read_only: false }, false, out);
                }
            }
            // Our eject, or an unmount from outside (exit 0 from Mounted), finished.
            (State::Mounted | State::Ejecting | State::Unmounting, _) => {
                rm(out);
                d.mountpoint = None;
                handover(d, outcome, To::PowerOff, sibling_active, out);
            }
            (State::Stopping(Then::Kernel), _) => {
                rm(out);
                d.mountpoint = None;
                handover(d, outcome, To::Kernel { read_only: false }, false, out);
            }
            (State::Stopping(Then::Exit), _) => {
                rm(out);
                d.mountpoint = None;
                handover(d, outcome, To::Release, false, out);
            }
            // Stopped for a conversion: only a Clean end may go on to `flashcp recluster`. The
            // mountpoint stays for the remount (same path for the user's windows).
            (State::Converting(Conv::Flushing | Conv::Stopping), _) => {
                let to = if self.shutting_down {
                    To::Release
                } else if !enabled {
                    To::Kernel { read_only: false }
                } else {
                    To::Recluster
                };
                if to != To::Recluster || outcome != Outcome::Clean {
                    rm(out);
                    d.mountpoint = None;
                }
                if outcome != Outcome::Clean {
                    d.conv_result =
                        Some(("failed", "It was not changed: flashcp could not finish saving to it.".into()));
                }
                handover(d, outcome, to, false, out);
            }
            _ => {}
        }
    }

    fn recluster_checked(&mut self, dev: &str, check: Option<ReclusterCheck>, out: &mut Vec<Action>) {
        let enabled = self.enabled && !self.shutting_down;
        let Some(d) = self.drives.get_mut(dev) else { return };
        if d.state != State::ReclusterChecking {
            return;
        }
        if !enabled {
            return kernel(d, false, out);
        }
        let check = check.unwrap_or_default();
        if check.finish && !d.finish_tried {
            // The user accepted this conversion earlier; its last write is missing (until then
            // Windows may not open the stick). One attempt per plug-in, before mounting.
            d.finish_tried = true;
            d.state = State::Converting(Conv::Running { finish: true });
            out.push(Action::Recluster {
                dev: dev.into(),
                serial: d.info.uuid.clone(),
                hw: d.info.disk_serial.clone(),
            });
            return;
        }
        d.can_convert = check.benefit.then_some((check.cluster, check.secs));
        to_mountpoint(d, out);
    }

    /// Why a conversion of plug-in `plug` cannot start now (None: it can). `live` is the
    /// identity read from what is at `dev` now; it must be `identity`, as the cached one must.
    pub fn convert_blocker(&self, dev: &str, plug: &str, identity: &str, live: Option<&str>) -> Option<&'static str> {
        let Some(d) = self.drives.get(dev) else { return Some(REPLACED) };
        if d.plug.is_empty()
            || d.plug != plug
            || drive_identity(&d.info) != identity
            || live != Some(identity)
            || d.gone
        {
            return Some(REPLACED);
        }
        if d.info.uuid.is_none() {
            return Some("flashcp cannot tell this drive apart from others (it has no volume serial)");
        }
        if !self.enabled || self.shutting_down {
            return Some("drive mode is off");
        }
        if matches!(d.state, State::Converting(_)) {
            return Some("it is being converted already");
        }
        if d.state != State::Mounted || d.gone {
            return Some("it is not ready (flashcp must have it mounted)");
        }
        if d.can_convert.is_none() {
            return Some("it would not get faster (its clusters are small already, or it cannot be converted)");
        }
        None
    }

    fn start_convert(&mut self, dev: &str, out: &mut Vec<Action>) {
        let Some(d) = self.drives.get_mut(dev) else { return };
        if d.offering {
            d.offering = false;
            out.push(Action::CloseOffer { dev: dev.into(), plug: d.plug.clone() });
            if let Some(key) = volume_key(&d.info)
                && self.answered.insert(key.clone())
            {
                out.push(Action::Remember { dev: dev.into(), volume: key, answer: "accepted" });
            }
        }
        d.state = State::Converting(Conv::Flushing);
        d.conv_secs = d.can_convert.map(|(_, s)| s).unwrap_or(1);
        (d.conv_done, d.conv_total) = (0, 0);
        d.conv_message = None;
        d.conv_result = None;
        d.eject_after = false;
        let name = d.info.name();
        out.push(notify(
            d,
            "convert",
            format!("Making {name} faster…"),
            format!("Don't unplug it. This takes about {}.", secs_text(d.conv_secs)),
            Urgency::Normal,
        ));
        out.push(Action::Control { dev: dev.into(), cmd: "prepare-eject" });
    }

    fn recluster_exit(&mut self, dev: &str, code: Option<i32>, out: &mut Vec<Action>) {
        let enabled = self.enabled && !self.shutting_down;
        let shutting_down = self.shutting_down;
        let Some(d) = self.drives.get_mut(dev) else { return };
        let State::Converting(Conv::Running { finish }) = d.state else { return };
        let message = d.conv_message.take();
        if d.gone {
            if let Some(path) = d.mountpoint.clone() {
                out.push(Action::RemoveMountpoint { path });
            }
            self.drives.remove(dev);
            return;
        }
        let name = d.info.name();
        if finish {
            match code {
                Some(exit::OK) => out.push(notify(
                    d,
                    "convert",
                    format!("{name} was repaired"),
                    "A change to make it faster had been interrupted; it is now complete. Nothing was lost.".into(),
                    Urgency::Normal,
                )),
                // Refused: it wrote nothing (or only the missing sector); mount it as usual.
                Some(exit::FALLBACK) => {}
                _ => {
                    out.push(notify(
                        d,
                        "convert",
                        format!("{name} is read-only for now"),
                        format!(
                            "flashcp could not complete an earlier change to it{}. Your files are there: copy \
                             them off, or plug it in again to retry.",
                            message.map(|(_, m)| format!(" ({m})")).unwrap_or_default()
                        ),
                        Urgency::Critical,
                    ));
                    return kernel(d, true, out);
                }
            }
            if !enabled {
                return kernel(d, false, out);
            }
            return to_mountpoint(d, out);
        }
        let (word, title, body) = match code {
            Some(exit::OK) => {
                ("done", format!("{name} is ready"), "Small files now copy to it much faster.".to_string())
            }
            Some(exit::FALLBACK) => {
                let why = match message.as_ref().map(|(r, _)| r.as_str()) {
                    Some("no-space" | "no-contiguous-space") => {
                        "There is not enough free space on it. Delete some files and try again.".to_string()
                    }
                    _ => format!(
                        "flashcp found something it does not change safely ({}).",
                        message.as_ref().map(|(_, m)| m.as_str()).unwrap_or("no reason given")
                    ),
                };
                (
                    "refused",
                    format!("{name} was not changed"),
                    format!("{why} Nothing was written; it works as before."),
                )
            }
            _ => (
                "failed",
                format!("Could not make {name} faster"),
                format!(
                    "{} Your files are safe and it works as before.",
                    message.map(|(_, m)| format!("{m}.")).unwrap_or_else(|| "flashcp recluster stopped.".into())
                ),
            ),
        };
        let note = conv_finished(d, word, title, body);
        if d.eject_after || shutting_down || !enabled {
            out.push(note);
            if let Some(path) = d.mountpoint.take() {
                out.push(Action::RemoveMountpoint { path });
            }
            // recluster leaves the volume complete and consistent whatever its exit code.
            let to = if d.eject_after {
                To::PowerOff
            } else if shutting_down {
                To::Release
            } else {
                To::Kernel { read_only: false }
            };
            return handover(d, Outcome::Clean, to, false, out);
        }
        d.after_mount = Some(note);
        match d.mountpoint.clone() {
            Some(mountpoint) => {
                d.state = State::Starting;
                out.push(Action::StartDriver { dev: dev.into(), mountpoint, label: name });
            }
            None => to_mountpoint(d, out),
        }
    }

    fn removed(&mut self, dev: &str, out: &mut Vec<Action>) {
        let Some(d) = self.drives.get_mut(dev) else { return };
        if d.offering {
            d.offering = false;
            out.push(Action::CloseOffer { dev: dev.into(), plug: d.plug.clone() });
        }
        if d.state == State::Converting(Conv::Running { finish: false }) {
            // recluster gets I/O errors and exits; its exit forgets the drive.
            d.gone = true;
            let name = d.info.name();
            out.push(notify(
                d,
                "convert",
                format!("{name} was unplugged while it was being changed"),
                "Your files are safe. Plug it in here again before using it on Windows: flashcp checks it and \
                 finishes the change if needed."
                    .into(),
                Urgency::Critical,
            ));
            return;
        }
        if !d.busy() {
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
        let dirty = self
            .drives
            .values()
            .any(|d| (d.driver_running() && d.dirty() || matches!(d.state, State::Converting(_))) && !d.gone);
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
        let opt = |s: Option<&str>| s.map(json_str).unwrap_or_else(|| "null".into());
        let drives: Vec<String> = self
            .drives
            .values()
            .map(|d| {
                format!(
                    r#"{{"device":{},"label":{},"mode":{},"state":{},"mountpoint":{},"dirty_bytes":{},"dirty_files":{},"safe_to_remove":{},"can_convert":{},"cluster":{},"convert_secs":{},"convert_done":{},"convert_total":{},"convert_result":{},"convert_message":{},"plug":{},"identity":{},"volume":{}}}"#,
                    json_str(&d.info.dev),
                    json_str(&d.info.name()),
                    json_str(d.mode()),
                    json_str(d.state_name()),
                    opt(d.mountpoint.as_deref()),
                    d.dirty_bytes,
                    d.dirty_files,
                    (d.mode() == "flashcp" && !d.dirty() && !matches!(d.state, State::Converting(_)))
                        || d.state == State::Done { safe: true },
                    d.can_convert.is_some(),
                    d.can_convert.map(|(c, _)| c).unwrap_or(0),
                    if matches!(d.state, State::Converting(_)) {
                        d.conv_secs
                    } else {
                        d.can_convert.map(|(_, s)| s).unwrap_or(0)
                    },
                    d.conv_done,
                    d.conv_total,
                    opt(d.conv_result.as_ref().map(|r| r.0)),
                    opt(d.conv_result.as_ref().map(|r| r.1.as_str())),
                    json_str(&d.plug),
                    json_str(&drive_identity(&d.info)),
                    opt(volume_key(&d.info).as_deref()),
                )
            })
            .collect();
        format!(r#"{{"ok":true,"enabled":{},"offers":{},"drives":[{}]}}"#, self.enabled, self.offers, drives.join(","))
    }
}

/// How the volume was left when it is handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// flashcp never served it (refused, busy, no mountpoint, drive mode off): nothing was written.
    NotServed,
    /// The driver exited 0 after an `unmounted` event that was clean with nothing pending.
    Clean,
    /// Anything else (a crash, an error exit, exit 0 without that evidence): data may be missing
    /// or a commit half done.
    Unclean,
}

/// Where a drive goes when flashcp (no longer) serves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum To {
    Kernel {
        read_only: bool,
    },
    /// Eject: power off (nothing of ours is mounted any more).
    PowerOff,
    /// Eject a kernel mount: udisks unmount, then power off (`checked`: holders were re-checked).
    KernelEject {
        checked: bool,
    },
    /// The daemon is going away: Rescan so the udev rule hands it to udisks.
    Release,
    /// Stopped for a conversion: run `flashcp recluster` (only after a Clean end).
    Recluster,
}

/// THE choke point: the only place that powers a drive off, mounts it with the kernel driver or
/// gives it back to udisks (Rescan). After an unclean end there is no power-off, no "safe"
/// message and no Rescan: the kernel gets it read-only at most (else it stays unmounted and
/// hidden), with a persistent notification. The interrupted commit's journal is on the volume,
/// so on the next plug-in the udev check (`pending_journal`) gives it to flashcp's recovery
/// first, even with drive mode off.
fn handover(d: &mut Drive, outcome: Outcome, to: To, sibling_active: bool, out: &mut Vec<Action>) {
    let dev = d.info.dev.clone();
    let disk = d.info.disk.clone();
    if outcome == Outcome::Unclean {
        let name = d.info.name();
        let to_kernel = matches!(to, To::Kernel { .. } | To::Recluster);
        let meanwhile = if to_kernel { " Until then it is opened read-only." } else { "" };
        out.push(notify(
            d,
            "unclean",
            format!("flashcp couldn't finish saving to {name}"),
            format!("Plug it in again to recover.{meanwhile}"),
            Urgency::Critical,
        ));
        if to_kernel {
            d.state = State::KernelMounting { read_only: true };
            out.push(Action::KernelMount { dev, read_only: true });
        } else {
            d.state = State::Done { safe: false };
        }
        return;
    }
    match to {
        To::Kernel { read_only } => {
            d.state = State::KernelMounting { read_only };
            out.push(Action::KernelMount { dev, read_only });
        }
        To::PowerOff if sibling_active => {
            d.state = State::Done { safe: true };
            out.push(notify(
                d,
                "removed",
                format!("{} was saved and unmounted", d.info.name()),
                "Another partition of this drive is still in use: eject it too before removing the drive.".into(),
                Urgency::Low,
            ));
        }
        To::PowerOff => {
            d.state = State::Done { safe: true };
            out.push(Action::PowerOff { dev, disk });
        }
        To::KernelEject { checked } => {
            d.state = State::Done { safe: true };
            out.push(Action::KernelEject { dev, disk, checked });
        }
        To::Release => {
            d.state = State::Done { safe: outcome == Outcome::Clean };
            out.push(Action::Rescan { dev });
        }
        To::Recluster => {
            d.state = State::Converting(Conv::Running { finish: false });
            out.push(Action::Recluster { dev, serial: d.info.uuid.clone(), hw: d.info.disk_serial.clone() });
        }
    }
}

fn to_mountpoint(d: &mut Drive, out: &mut Vec<Action>) {
    d.state = State::MakingMountpoint;
    out.push(Action::MakeMountpoint { dev: d.info.dev.clone(), name: mountpoint_name(&d.info) });
}

/// "about 7 seconds", "about a minute", "about 3 minutes".
pub fn secs_text(secs: u64) -> String {
    match secs {
        0..=1 => "a second".into(),
        2..=89 => format!("{secs} seconds"),
        90..=119 => "a minute and a half".into(),
        _ => format!("{} minutes", secs.div_ceil(60)),
    }
}

/// The offer: what it does, how long, not to unplug. No jargon (no "clusters").
pub fn offer_texts(name: &str, secs: u64) -> (String, String) {
    (
        format!("Make {name} faster for small files?"),
        format!(
            "flashcp can change how {name} stores files so that small files copy much faster. Nothing is \
             erased. It takes about {} and you must not unplug it meanwhile.",
            secs_text(secs)
        ),
    )
}

fn offer(d: &Drive) -> Action {
    let (title, body) = offer_texts(&d.info.name(), d.can_convert.map(|(_, s)| s).unwrap_or(1));
    Action::Offer { dev: d.info.dev.clone(), plug: d.plug.clone(), identity: drive_identity(&d.info), title, body }
}

/// The end of a conversion: remembered for the app / CLI, and the notification saying so.
fn conv_finished(d: &mut Drive, word: &'static str, title: String, body: String) -> Action {
    d.conv_result = Some((word, format!("{title}. {body}")));
    if word == "done" {
        d.can_convert = None;
    }
    notify(d, "convert", title, body, if word == "done" { Urgency::Normal } else { Urgency::Critical })
}

fn kernel(d: &mut Drive, read_only: bool, out: &mut Vec<Action>) {
    handover(d, Outcome::NotServed, To::Kernel { read_only }, false, out);
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
            recover: false,
            disk_serial: None,
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
    /// `recluster --check` failed or said "no".
    fn no_gain(dev: &str) -> Input {
        Input::ReclusterChecked { dev: dev.into(), check: None }
    }
    const D: &str = "/dev/sdb1";
    const MOUNTED: &str = r#"{"event":"mounted","proto":1,"device":"/dev/sdb1","recovered":null}"#;
    const DIRTY: &str = r#"{"event":"dirty_bytes","bytes":5000000,"files":3}"#;
    const CLEAN: &str = r#"{"event":"dirty_bytes","bytes":0,"files":0}"#;
    const UNMOUNTED: &str = r#"{"event":"unmounted","clean":true}"#;

    /// The common prefix: plug in, check ok, mountpoint ready, the driver reports mounted.
    fn serving() -> Vec<(Input, &'static str, &'static str)> {
        vec![
            (added(D), "check /dev/sdb1", "checking"),
            (check(D, 0), "recluster-check /dev/sdb1", "checking"),
            (no_gain(D), "mkmp /dev/sdb1 K", "starting"),
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
                    (check(D, 0), "recluster-check /dev/sdb1", "checking"),
                    (no_gain(D), "mkmp /dev/sdb1 K", "starting"),
                    (mp(D), "start /dev/sdb1 /media/u/K", "starting"),
                    (exit_(D, 3), "rmmp /media/u/K; kernel /dev/sdb1 rw", "mounting"),
                ],
            ),
            (
                "no mountpoint (helper refused): kernel",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 0), "recluster-check /dev/sdb1", "checking"),
                    (no_gain(D), "mkmp /dev/sdb1 K", "starting"),
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
                    (
                        exit_(D, 1),
                        "rmmp /media/u/K; notify /dev/sdb1:failed; notify /dev/sdb1:unclean; kernel /dev/sdb1 ro",
                        "mounting",
                    ),
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
                    (ev(D, UNMOUNTED), "", "ejecting"),
                    (exit_(D, 0), "rmmp /media/u/K; poweroff /dev/sdb", "safe_to_remove"),
                ]),
            ),
            (
                "handover, unclean: outside unmount whose driver fails: no power-off, no 'safe'",
                true,
                with(vec![
                    (ev(D, DIRTY), "inhibit on", "saving"),
                    (
                        ev(D, r#"{"event":"committing","bytes":5000000,"files":3,"reason":"unmount"}"#),
                        "notify /dev/sdb1:saving",
                        "ejecting",
                    ),
                    (exit_(D, 1), "rmmp /media/u/K; notify /dev/sdb1:unclean; inhibit off", "stopped"),
                    (Input::Eject { dev: D.into() }, "notify /dev/sdb1:unclean", "stopped"),
                ]),
            ),
            (
                "handover, unclean: our eject ends with exit 0 but no clean `unmounted`",
                true,
                with(vec![
                    (Input::Eject { dev: D.into() }, "ctl /dev/sdb1 prepare-eject", "ejecting"),
                    (reply(D, "prepare-eject", true), "ctl /dev/sdb1 unmount", "ejecting"),
                    (ev(D, r#"{"event":"unmounted","clean":false}"#), "", "ejecting"),
                    (exit_(D, 0), "rmmp /media/u/K; notify /dev/sdb1:unclean", "stopped"),
                ]),
            ),
            (
                "handover: a driver that never mounted fails twice: nothing was written, kernel rw",
                true,
                vec![
                    (added(D), "check /dev/sdb1", "checking"),
                    (check(D, 0), "recluster-check /dev/sdb1", "checking"),
                    (no_gain(D), "mkmp /dev/sdb1 K", "starting"),
                    (mp(D), "start /dev/sdb1 /media/u/K", "starting"),
                    (exit_(D, 1), "start /dev/sdb1 /media/u/K", "starting"),
                    (exit_(D, 1), "rmmp /media/u/K; notify /dev/sdb1:failed; kernel /dev/sdb1 rw", "mounting"),
                ],
            ),
            (
                "handover: pending journal with drive mode off: flashcp recovers once, then the kernel (rw if clean)",
                false,
                vec![
                    (Input::Added(DevInfo { recover: true, ..info(D, "K") }), "check /dev/sdb1", "checking"),
                    (check(D, 0), "mkmp /dev/sdb1 K", "starting"),
                    (mp(D), "start /dev/sdb1 /media/u/K", "starting"),
                    (
                        ev(D, r#"{"event":"mounted","proto":1,"recovered":"replayed"}"#),
                        "stop /dev/sdb1; notify /dev/sdb1:recovered",
                        "stopping",
                    ),
                    (ev(D, UNMOUNTED), "", "stopping"),
                    (exit_(D, 0), "rmmp /media/u/K; kernel /dev/sdb1 rw", "mounting"),
                ],
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
                        "rmmp /media/u/K; notify /dev/sdb1:unclean; kernel /dev/sdb1 ro; inhibit off",
                        "mounting",
                    ),
                ]),
            ),
            (
                "off switch: exit 0 but no `unmounted` event: read-only",
                true,
                with(vec![
                    (Input::SetEnabled(false), "stop /dev/sdb1", "stopping"),
                    (exit_(D, 0), "rmmp /media/u/K; notify /dev/sdb1:unclean; kernel /dev/sdb1 ro", "mounting"),
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
                    (ev(D, UNMOUNTED), "", "stopping"),
                    (exit_(D, 0), "rmmp /media/u/K; rescan /dev/sdb1; exit", "safe_to_remove"),
                ]),
            ),
            (
                "handover, unclean: daemon stop and the driver fails: no Rescan (stays hidden for recovery)",
                true,
                with(vec![
                    (Input::Shutdown, "stop /dev/sdb1", "stopping"),
                    (exit_(D, 1), "rmmp /media/u/K; notify /dev/sdb1:unclean; exit", "stopped"),
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
                    (ev(D, UNMOUNTED), "", "ejecting"),
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
                    (Input::Shutdown, "rescan /dev/sdb1; exit", "stopped"),
                ],
            ),
        ];

        for (name, enabled, steps) in table {
            let mut core = test_core(enabled);
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

    fn gain(dev: &str, cluster: u64) -> Input {
        let c = ReclusterCheck { benefit: true, reason: "ok".into(), cluster, secs: 7, finish: false };
        Input::ReclusterChecked { dev: dev.into(), check: Some(c) }
    }
    fn finish(dev: &str) -> Input {
        let c =
            ReclusterCheck { benefit: false, reason: "small-clusters".into(), cluster: 4096, secs: 1, finish: true };
        Input::ReclusterChecked { dev: dev.into(), check: Some(c) }
    }
    fn rexit(dev: &str, code: i32) -> Input {
        Input::ReclusterExit { dev: dev.into(), code: Some(code) }
    }
    fn rev(dev: &str, line: &str) -> Input {
        Input::Recluster { dev: dev.into(), ev: ReclusterEvent::parse(line) }
    }
    /// A core with deterministic plug tokens `t1`, `t2`, ... (the daemon's are random).
    fn test_core(enabled: bool) -> Core {
        let mut c = Core::new(enabled);
        c.test_tokens = Some(0);
        c
    }
    /// The identity of `info(D, ..)`: its volume serial, no disk serial.
    const ID: &str = "1A2B-3C4D:";
    /// The answer to the offer of the drive now at `dev` (its current plug-in).
    fn answer_for(core: &Core, dev: &str, answer: Answer) -> Input {
        let d = &core.drives[dev];
        let id = drive_identity(&d.info);
        Input::OfferAnswer { dev: dev.into(), plug: d.plug.clone(), live: Some(id.clone()), identity: id, answer }
    }
    fn answer(dev: &str, answer: Answer) -> Input {
        // Every test core starts at plug-in t1; replugs pass their own token.
        Input::OfferAnswer { dev: dev.into(), plug: "t1".into(), identity: ID.into(), live: Some(ID.into()), answer }
    }
    fn convert(dev: &str, plug: &str) -> Input {
        Input::Convert { dev: dev.into(), plug: plug.into(), identity: ID.into(), live: Some(ID.into()) }
    }

    type Step = (Input, &'static str, &'static str);

    fn run(name: &str, core: &mut Core, steps: Vec<Step>) {
        for (i, (input, want, state)) in steps.into_iter().enumerate() {
            let dbg = format!("{input:?}");
            let got: Vec<String> = core.handle(input).iter().map(|a| a.to_string()).collect();
            assert_eq!(got.join("; "), want, "{name}: step {i} ({dbg})");
            let st = core.drives.get(D).map(Drive::state_name).unwrap_or("-");
            assert_eq!(st, state, "{name}: state after step {i} ({dbg})");
            assert!(json::parse_flat(&core.status_json()).is_some(), "{name}: status is JSON");
        }
    }

    /// Plug in a stick with big clusters (or `cluster`), up to "mounted" (the offer, if any, is
    /// the last step's action).
    fn served_big(cluster: u64, mounted: &'static str) -> Vec<Step> {
        vec![
            (added(D), "check /dev/sdb1", "checking"),
            (check(D, 0), "recluster-check /dev/sdb1", "checking"),
            (gain(D, cluster), "mkmp /dev/sdb1 K", "starting"),
            (mp(D), "start /dev/sdb1 /media/u/K", "starting"),
            (ev(D, MOUNTED), mounted, "ready"),
        ]
    }

    /// From "ready" to `flashcp recluster` running: pause, clean stop, then recluster.
    fn stop_for_convert(first: Input, first_out: &'static str) -> Vec<Step> {
        vec![
            (first, first_out, "converting"),
            (reply(D, "prepare-eject", true), "ctl /dev/sdb1 unmount", "converting"),
            (ev(D, UNMOUNTED), "", "converting"),
            (exit_(D, 0), "recluster /dev/sdb1", "converting"),
        ]
    }

    const ACCEPTED: &str =
        "remember 1A2B-3C4D::K accepted; notify /dev/sdb1:convert; ctl /dev/sdb1 prepare-eject; inhibit on";

    #[test]
    fn offer_accept_convert_remount() {
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps.extend(stop_for_convert(answer(D, Answer::Accept), ACCEPTED));
        steps.extend(vec![
            (rev(D, r#"{"event":"plan","op":"recluster","est_secs":6.2,"switches":2}"#), "", "converting"),
            (rev(D, r#"{"event":"progress","op":"recluster","bytes_done":50,"bytes_total":200}"#), "", "converting"),
            (rev(D, r#"{"event":"done","op":"recluster","changed":true}"#), "", "converting"),
            // Same mountpoint again; "ready" only once flashcp has it mounted.
            (rexit(D, 0), "start /dev/sdb1 /media/u/K; inhibit off", "starting"),
            (ev(D, MOUNTED), "notify /dev/sdb1:convert", "ready"),
        ]);
        run("accept", &mut core, steps);
        let d = &core.drives[D];
        assert_eq!(d.conv_result.as_ref().map(|r| r.0), Some("done"));
        assert!(d.conv_result.as_ref().unwrap().1.contains("K is ready. Small files now copy"));
        assert_eq!((d.conv_done, d.conv_total, d.conv_secs), (50, 200, 7));
        assert!(d.can_convert.is_none(), "no second offer, no button after the conversion");
        assert!(core.answered.contains("1A2B-3C4D::K"));
        // Plugged in again (the check says no now): no offer.
        run(
            "replug",
            &mut core,
            vec![(Input::Removed { dev: D.into() }, "stop /dev/sdb1", "ready"), (exit_(D, 1), "rmmp /media/u/K", "-")],
        );
        run("replug", &mut core, served_big(128 << 10, ""));
    }

    #[test]
    fn not_now_and_dismissed_are_remembered_no_answer_is_not() {
        for (a, remembered) in [(Answer::NotNow, true), (Answer::Dismissed, true), (Answer::None, false)] {
            let name = format!("{a:?}");
            let mut core = test_core(true);
            let mut steps = served_big(32 << 10, "offer /dev/sdb1");
            steps.push((answer(D, a), if remembered { "remember 1A2B-3C4D::K declined" } else { "" }, "ready"));
            // A second answer (or a stale one) does nothing.
            steps.push((answer(D, Answer::Accept), "", "ready"));
            steps.push((Input::Removed { dev: D.into() }, "stop /dev/sdb1", "ready"));
            steps.push((exit_(D, 1), "rmmp /media/u/K", "-"));
            run(&name, &mut core, steps);
            // Plugged in again: asked again only if there was no answer.
            run(&name, &mut core, served_big(32 << 10, if remembered { "" } else { "offer /dev/sdb1" }));
            // The app button works either way (and closes an offer still on screen).
            let want = if remembered {
                "notify /dev/sdb1:convert; ctl /dev/sdb1 prepare-eject; inhibit on"
            } else {
                "close-offer /dev/sdb1; remember 1A2B-3C4D::K accepted; notify /dev/sdb1:convert; ctl /dev/sdb1 prepare-eject; inhibit on"
            };
            run(&name, &mut core, vec![(convert(D, "t2"), want, "converting")]);
        }
        // An answer persisted by an earlier daemon.
        let mut core = test_core(true);
        core.answered.insert("1A2B-3C4D::K".into());
        run("loaded", &mut core, served_big(128 << 10, ""));
    }

    #[test]
    fn no_offer_unless_it_is_worth_it() {
        // 16 KB: the check says yes, the notification does not ask; the app button can.
        let mut core = test_core(true);
        run("16 KB", &mut core, served_big(16 << 10, ""));
        assert_eq!(core.convert_blocker(D, "t1", ID, Some(ID)), None);
        // The check says no (or failed): neither.
        let mut core = test_core(true);
        run("no", &mut core, serving());
        assert!(core.convert_blocker(D, "t1", ID, Some(ID)).is_some());
        run("no", &mut core, vec![(convert(D, "t1"), "", "ready")]);
        // The offer turned off.
        let mut core = test_core(true);
        core.offers = false;
        run("offers off", &mut core, served_big(128 << 10, ""));
        assert_eq!(core.convert_blocker(D, "t1", ID, Some(ID)), None);
        // No volume serial: nothing to remember it by, so no notification.
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "");
        steps[0].0 = Input::Added(DevInfo { uuid: None, ..info(D, "K") });
        run("no serial", &mut core, steps);
        // Drive mode off: the kernel driver, no check at all.
        let mut core = test_core(false);
        run("off", &mut core, vec![(added(D), "kernel /dev/sdb1 rw", "mounting")]);
        // A drive being saved / ejected cannot start one.
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps.push((Input::Eject { dev: D.into() }, "ctl /dev/sdb1 prepare-eject", "ejecting"));
        steps.push((answer(D, Answer::Accept), "remember 1A2B-3C4D::K accepted", "ejecting"));
        run("ejecting", &mut core, steps);
        assert!(core.convert_blocker(D, "t1", ID, Some(ID)).is_some());
        // The CLI/app path refuses a drive that is not ready.
        let mut core = test_core(true);
        run("checking", &mut core, vec![(added(D), "check /dev/sdb1", "checking")]);
        assert_eq!(core.convert_blocker(D, "t1", ID, Some(ID)), Some("it is not ready (flashcp must have it mounted)"));
        assert_eq!(core.convert_blocker("/dev/sdz1", "t1", ID, Some(ID)), Some(REPLACED));
    }

    #[test]
    fn only_a_clean_stop_leads_to_recluster() {
        // The driver dies while flushing: no recluster, the kernel gets it read-only.
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps.push((ev(D, DIRTY), "inhibit on", "saving"));
        steps.push((
            answer(D, Answer::Accept),
            "remember 1A2B-3C4D::K accepted; notify /dev/sdb1:convert; ctl /dev/sdb1 prepare-eject",
            "converting",
        ));
        steps.push((
            exit_(D, 1),
            "rmmp /media/u/K; notify /dev/sdb1:unclean; kernel /dev/sdb1 ro; inhibit off",
            "mounting",
        ));
        run("crash", &mut core, steps);
        assert_eq!(core.drives[D].conv_result.as_ref().map(|r| r.0), Some("failed"));
        // Exit 0 without a clean `unmounted`: not Clean either.
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps.push((answer(D, Answer::Accept), ACCEPTED, "converting"));
        steps.push((reply(D, "prepare-eject", true), "ctl /dev/sdb1 unmount", "converting"));
        steps.push((ev(D, r#"{"event":"unmounted","clean":false}"#), "", "converting"));
        steps.push((
            exit_(D, 0),
            "rmmp /media/u/K; notify /dev/sdb1:unclean; kernel /dev/sdb1 ro; inhibit off",
            "mounting",
        ));
        run("unclean exit 0", &mut core, steps);
        // The driver refuses to pause (files in use): resumed, nothing changed.
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps.push((answer(D, Answer::Accept), ACCEPTED, "converting"));
        steps.push((
            reply(D, "prepare-eject", false),
            "notify /dev/sdb1:convert; ctl /dev/sdb1 resume; inhibit off",
            "ready",
        ));
        run("pause refused", &mut core, steps);
    }

    #[test]
    fn refusal_and_failure_remount_as_before() {
        for (code, line, word, text) in [
            (
                3,
                r#"{"event":"refused","op":"recluster","reason":"no-space","message":"not enough free space"}"#,
                "refused",
                "K was not changed. There is not enough free space on it.",
            ),
            (
                3,
                r#"{"event":"refused","op":"recluster","reason":"bad-clusters","message":"clusters marked bad"}"#,
                "refused",
                "flashcp found something it does not change safely (clusters marked bad)",
            ),
            (1, r#"{"event":"error","message":"read /dev/sdb1: I/O error"}"#, "failed", "Could not make K faster."),
        ] {
            let mut core = test_core(true);
            let mut steps = served_big(128 << 10, "offer /dev/sdb1");
            steps.extend(stop_for_convert(answer(D, Answer::Accept), ACCEPTED));
            steps.push((rev(D, line), "", "converting"));
            steps.push((rexit(D, code), "start /dev/sdb1 /media/u/K; inhibit off", "starting"));
            steps.push((ev(D, MOUNTED), "notify /dev/sdb1:convert", "ready"));
            run(word, &mut core, steps);
            let r = core.drives[D].conv_result.clone().unwrap();
            assert_eq!(r.0, word);
            assert!(r.1.contains(text), "{r:?}");
            assert!(r.1.contains("works as before"), "{r:?}");
            // Still offered by the app button (the check said yes and nothing changed).
            assert_eq!(core.convert_blocker(D, "t1", ID, Some(ID)), None);
        }
    }

    #[test]
    fn yank_eject_shutdown_and_off_during_a_conversion() {
        let to_running = || {
            let mut s = served_big(128 << 10, "offer /dev/sdb1");
            s.extend(stop_for_convert(answer(D, Answer::Accept), ACCEPTED));
            s
        };
        // Yanked: a warning; recluster's exit forgets the drive and removes the mountpoint.
        let mut core = test_core(true);
        let mut steps = to_running();
        steps.push((Input::Removed { dev: D.into() }, "notify /dev/sdb1:convert; inhibit off", "converting"));
        steps.push((rexit(D, 1), "rmmp /media/u/K", "-"));
        run("yank", &mut core, steps);
        // Eject waits for the end, then powers off instead of remounting.
        let mut core = test_core(true);
        let mut steps = to_running();
        steps.push((Input::Eject { dev: D.into() }, "", "converting"));
        steps.push((
            rexit(D, 0),
            "notify /dev/sdb1:convert; rmmp /media/u/K; poweroff /dev/sdb; inhibit off",
            "safe_to_remove",
        ));
        run("eject", &mut core, steps);
        // An eject during the pause also waits.
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps.push((answer(D, Answer::Accept), ACCEPTED, "converting"));
        steps.push((Input::Eject { dev: D.into() }, "", "converting"));
        run("eject while pausing", &mut core, steps);
        assert!(core.drives[D].eject_after);
        // The daemon is stopped: it waits for recluster, then hands the drive back.
        let mut core = test_core(true);
        let mut steps = to_running();
        steps.push((Input::Shutdown, "", "converting"));
        steps.push((
            rexit(D, 0),
            "notify /dev/sdb1:convert; rmmp /media/u/K; rescan /dev/sdb1; inhibit off; exit",
            "safe_to_remove",
        ));
        run("shutdown", &mut core, steps);
        // Drive mode turned off meanwhile: the kernel driver afterwards.
        let mut core = test_core(true);
        let mut steps = to_running();
        steps.push((Input::SetEnabled(false), "", "converting"));
        steps.push((
            rexit(D, 0),
            "notify /dev/sdb1:convert; rmmp /media/u/K; kernel /dev/sdb1 rw; inhibit off",
            "mounting",
        ));
        run("off", &mut core, steps);
    }

    #[test]
    fn interrupted_conversion_is_finished_once_before_mounting() {
        let start = || -> Vec<Step> {
            vec![
                (added(D), "check /dev/sdb1", "checking"),
                (check(D, 0), "recluster-check /dev/sdb1", "checking"),
                (finish(D), "recluster /dev/sdb1; inhibit on", "converting"),
            ]
        };
        let mut core = test_core(true);
        let mut steps = start();
        steps.push((rexit(D, 0), "notify /dev/sdb1:convert; mkmp /dev/sdb1 K; inhibit off", "starting"));
        steps.push((mp(D), "start /dev/sdb1 /media/u/K", "starting"));
        // No offer: it has small clusters now (the finish check said benefit false).
        steps.push((ev(D, MOUNTED), "", "ready"));
        run("finished", &mut core, steps);
        // Refused (e.g. a pending drive-mode commit): mounted as usual, the mount replays it.
        let mut core = test_core(true);
        let mut steps = start();
        steps.push((rexit(D, 3), "mkmp /dev/sdb1 K; inhibit off", "starting"));
        run("refused", &mut core, steps);
        // Failed: one attempt, then the kernel read-only with a message; no loop.
        let mut core = test_core(true);
        let mut steps = start();
        steps.push((rexit(D, 1), "notify /dev/sdb1:convert; kernel /dev/sdb1 ro; inhibit off", "mounting"));
        steps.push((Input::KernelMounted { dev: D.into(), result: Ok("/media/u/K".into()) }, "", "read_only"));
        run("failed", &mut core, steps);
        assert!(core.drives[D].finish_tried);
        // Yanked meanwhile: forgotten.
        let mut core = test_core(true);
        let mut steps = start();
        steps.push((Input::Removed { dev: D.into() }, "inhibit off", "converting"));
        steps.push((rexit(D, 1), "", "-"));
        run("yank", &mut core, steps);
        // Drive mode off before the recluster check answered: the kernel, nothing run.
        let mut core = test_core(true);
        let mut steps = start();
        steps.truncate(2);
        steps.push((Input::SetEnabled(false), "", "checking"));
        steps.push((finish(D), "kernel /dev/sdb1 rw", "mounting"));
        run("off", &mut core, steps);
    }

    #[test]
    fn kernel_mount_unmounted_by_hand_is_not_listed_as_mounted() {
        let mut core = test_core(true);
        run(
            "unmount by hand",
            &mut core,
            vec![
                (added(D), "check /dev/sdb1", "checking"),
                (check(D, 3), "kernel /dev/sdb1 rw", "mounting"),
                (Input::KernelMounted { dev: D.into(), result: Ok("/media/u/K".into()) }, "", "kernel"),
                (Input::KernelMountSeen { dev: D.into(), at: None }, "", "unmounted"),
            ],
        );
        let s = core.status_json();
        assert!(s.contains(r#""mode":"none","state":"unmounted","mountpoint":null"#), "{s}");
        run(
            "mounted again by hand",
            &mut core,
            vec![(Input::KernelMountSeen { dev: D.into(), at: Some("/media/u/K1".into()) }, "", "kernel")],
        );
        assert_eq!(core.drives[D].mountpoint.as_deref(), Some("/media/u/K1"));
        // Eject of an unmounted one: re-check holders first, then the udisks path.
        run(
            "eject",
            &mut core,
            vec![
                (Input::KernelMountSeen { dev: D.into(), at: None }, "", "unmounted"),
                (Input::Eject { dev: D.into() }, "check /dev/sdb1", "ejecting"),
                (check(D, 0), "kernel-eject /dev/sdb1 /dev/sdb checked", "safe_to_remove"),
            ],
        );
        // A flashcp mount is not affected.
        let mut core = test_core(true);
        run("flashcp", &mut core, serving());
        run("flashcp", &mut core, vec![(Input::KernelMountSeen { dev: D.into(), at: None }, "", "ready")]);
    }

    /// Stick A is offered, unplugged, and stick B gets the same device path. A's answer (a
    /// late notification click, the app's dialog, the CLI's confirmation) never reaches B.
    #[test]
    fn an_answer_for_one_stick_never_converts_another_at_the_same_path() {
        for (name, b) in [
            ("other stick", DevInfo { uuid: Some("5E6F-7A8B".into()), ..info(D, "OTHER") }),
            ("same label", DevInfo { uuid: Some("5E6F-7A8B".into()), ..info(D, "K") }),
            ("a clone of A", info(D, "K")),
        ] {
            let mut core = test_core(true);
            let mut steps = served_big(128 << 10, "offer /dev/sdb1");
            steps.push((Input::Removed { dev: D.into() }, "close-offer /dev/sdb1; stop /dev/sdb1", "ready"));
            steps.push((exit_(D, 1), "rmmp /media/u/K", "-"));
            run(name, &mut core, steps);
            let mut steps = served_big(128 << 10, "");
            steps[0].0 = Input::Added(b.clone());
            steps[2].1 = if b.label.as_deref() == Some("OTHER") { "mkmp /dev/sdb1 OTHER" } else { "mkmp /dev/sdb1 K" };
            steps[4].1 = "offer /dev/sdb1";
            if b.label.as_deref() == Some("OTHER") {
                steps[3].0 = Input::MountpointReady { dev: D.into(), result: Ok("/media/u/K".into()) };
            }
            run(name, &mut core, steps);
            assert_eq!(core.drives[D].plug, "t2");
            run(
                name,
                &mut core,
                vec![
                    // A's notification answered late, A's dialog, A's CLI confirmation: plug 1.
                    (answer(D, Answer::Accept), "", "ready"),
                    (answer(D, Answer::NotNow), "", "ready"),
                    (convert(D, "t1"), "", "ready"),
                ],
            );
            assert!(core.answered.is_empty(), "{name}: nothing remembered for B");
            assert_eq!(core.convert_blocker(D, "t1", ID, Some(ID)), Some(REPLACED), "{name}");
            assert_eq!(
                core.convert_blocker(D, "t2", &drive_identity(&b), Some(&drive_identity(&b))),
                None,
                "{name}: B's own token works"
            );
            // B's token with A's identity (when B is another volume) is not B either.
            if drive_identity(&b) != ID {
                assert_eq!(core.convert_blocker(D, "t2", ID, Some(ID)), Some(REPLACED), "{name}");
            }
            // B's own answer is B's.
            let a = answer_for(&core, D, Answer::NotNow);
            let got: Vec<String> = core.handle(a).iter().map(|a| a.to_string()).collect();
            assert_eq!(got.len(), 1, "{name}: {got:?}");
            assert!(got[0].starts_with("remember "), "{name}: {got:?}");
        }
    }

    /// An offer (or an app dialog, or a CLI confirmation) made by one daemon run is never
    /// accepted by the next one, even for the very same stick at the same path: plug tokens
    /// are random, not a counter that starts over.
    #[test]
    fn consent_does_not_survive_a_daemon_restart() {
        let steps = || {
            let mut s = served_big(128 << 10, "offer /dev/sdb1");
            s[0].0 = Input::Added(DevInfo { disk_serial: Some("AA01".into()), ..info(D, "K") });
            s
        };
        let mut first = Core::new(true);
        run("first run", &mut first, steps());
        let (old_plug, old_id) = (first.drives[D].plug.clone(), drive_identity(&first.drives[D].info));
        assert_eq!(old_plug.len(), 32, "128 bits: {old_plug}");
        assert!(old_plug.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(old_id, "1A2B-3C4D:AA01");
        // The daemon stops: the offer notification is taken down with it.
        let got: Vec<String> = first.handle(Input::Shutdown).iter().map(|a| a.to_string()).collect();
        assert_eq!(got.join("; "), "close-offer /dev/sdb1; stop /dev/sdb1");

        let mut second = Core::new(true);
        run("second run", &mut second, steps());
        assert_ne!(second.drives[D].plug, old_plug, "tokens differ across runs");
        let stale = [
            Input::OfferAnswer {
                dev: D.into(),
                plug: old_plug.clone(),
                identity: old_id.clone(),
                live: Some(old_id.clone()),
                answer: Answer::Accept,
            },
            Input::OfferAnswer {
                dev: D.into(),
                plug: old_plug.clone(),
                identity: old_id.clone(),
                live: Some(old_id.clone()),
                answer: Answer::NotNow,
            },
            Input::Convert {
                dev: D.into(),
                plug: old_plug.clone(),
                identity: old_id.clone(),
                live: Some(old_id.clone()),
            },
        ];
        for input in stale {
            let dbg = format!("{input:?}");
            assert!(second.handle(input).is_empty(), "{dbg}");
            assert_eq!(second.drives[D].state_name(), "ready", "{dbg}");
        }
        assert!(second.answered.is_empty(), "nothing remembered");
        assert_eq!(second.convert_blocker(D, &old_plug, &old_id, Some(&old_id)), Some(REPLACED));
        // The right token with the wrong identity, and the reverse, are refused too.
        let plug = second.drives[D].plug.clone();
        assert_eq!(second.convert_blocker(D, &plug, "1A2B-3C4D:", Some("1A2B-3C4D:")), Some(REPLACED));
        assert_eq!(second.convert_blocker(D, &plug, &old_id, Some(&old_id)), None);
        assert_eq!(second.convert_blocker(D, "", "", Some("")), Some(REPLACED));
    }

    /// The cached info says stick A, but the device at the path is now a clone of A (same
    /// volume serial, another hardware serial) and udev has not told us yet: the identity read
    /// from the device when the answer or request comes does not match, so nothing happens.
    #[test]
    fn a_clone_swapped_in_before_udev_tells_us_is_refused() {
        let a = DevInfo { disk_serial: Some("AA01".into()), ..info(D, "K") };
        let id = drive_identity(&a);
        let clone = "1A2B-3C4D:BB02".to_string();
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps[0].0 = Input::Added(a);
        run("A", &mut core, steps);
        for live in [Some(clone.clone()), None] {
            let ans = Input::OfferAnswer {
                dev: D.into(),
                plug: "t1".into(),
                identity: id.clone(),
                live: live.clone(),
                answer: Answer::Accept,
            };
            run("late answer", &mut core, vec![(ans, "", "ready")]);
            let conv = Input::Convert { dev: D.into(), plug: "t1".into(), identity: id.clone(), live: live.clone() };
            run("request", &mut core, vec![(conv, "", "ready")]);
            assert_eq!(core.convert_blocker(D, "t1", &id, live.as_deref()), Some(REPLACED), "{live:?}");
        }
        assert!(core.answered.is_empty(), "nothing remembered for the clone");
        // The real A at the path: accepted, and recluster gets A's hardware serial to check.
        let conv = Input::Convert { dev: D.into(), plug: "t1".into(), identity: id.clone(), live: Some(id.clone()) };
        let got: Vec<String> = core.handle(conv).iter().map(|a| a.to_string()).collect();
        assert!(got.iter().any(|a| a == "ctl /dev/sdb1 prepare-eject"), "{got:?}");
        let mut steps = vec![
            (reply(D, "prepare-eject", true), "ctl /dev/sdb1 unmount", "converting"),
            (ev(D, UNMOUNTED), "", "converting"),
        ];
        steps.push((exit_(D, 0), "recluster /dev/sdb1", "converting"));
        let mut c2 = core;
        for (input, want, state) in steps {
            let acts = c2.handle(input);
            let got: Vec<String> = acts.iter().map(|a| a.to_string()).collect();
            assert_eq!(got.join("; "), want);
            assert_eq!(c2.drives[D].state_name(), state);
            if let Some(Action::Recluster { serial, hw, .. }) = acts.first() {
                assert_eq!((serial.as_deref(), hw.as_deref()), (Some("1A2B-3C4D"), Some("AA01")));
            }
        }
    }

    /// No random bytes from the OS: the drive is served as usual, never offered or converted.
    #[test]
    fn no_token_means_no_offer_and_no_conversion() {
        NO_RANDOM.with(|c| c.set(true));
        let mut core = Core::new(true);
        run("no token", &mut core, served_big(128 << 10, ""));
        NO_RANDOM.with(|c| c.set(false));
        assert_eq!(core.drives[D].plug, "");
        assert_eq!(core.convert_blocker(D, "", ID, Some(ID)), Some(REPLACED));
        let conv = Input::Convert { dev: D.into(), plug: String::new(), identity: ID.into(), live: Some(ID.into()) };
        run("no token", &mut core, vec![(conv, "", "ready")]);
    }

    /// With a hardware serial the key has no label: renaming the stick does not bring the offer
    /// back. Without one the label stays in the key (the documented fallback).
    #[test]
    fn a_relabelled_stick_is_not_asked_again() {
        let k = DevInfo { disk_serial: Some("AA01".into()), ..info(D, "K") };
        let l = DevInfo { label: Some("RENAMED".into()), ..k.clone() };
        assert_eq!(volume_key(&k), volume_key(&l));
        assert_eq!(volume_key(&k).as_deref(), Some("1A2B-3C4D:AA01:"));
        assert_eq!(volume_key(&info(D, "K")).as_deref(), Some("1A2B-3C4D::K"));
        let mut core = test_core(true);
        core.answered.insert(volume_key(&k).unwrap());
        let mut steps = served_big(128 << 10, "");
        steps[0].0 = Input::Added(l);
        steps[2].1 = "mkmp /dev/sdb1 RENAMED";
        run("relabelled", &mut core, steps);
        // Keys an earlier build of this branch wrote (with the label) still count.
        assert_eq!(normalize_key("1A2B-3C4D:AA01:K"), "1A2B-3C4D:AA01:");
        assert_eq!(normalize_key("1A2B-3C4D::K"), "1A2B-3C4D::K");
        assert_eq!(normalize_key("1A2B-3C4D:AA01:"), "1A2B-3C4D:AA01:");
    }

    #[test]
    fn labels_lose_control_characters() {
        let i = info(D, "K\nX\t<b>");
        assert_eq!(i.name(), "KX<b>");
        let (t, b) = offer_texts(&i.name(), 3);
        assert!(!t.contains('\n') && !b.contains('\n'), "{t} {b}");
    }

    /// The conversion's pause is refused while a shutdown, drive mode off or an eject waits:
    /// each then goes its normal way instead of being dropped.
    #[test]
    fn a_failed_pause_still_carries_out_shutdown_off_and_eject() {
        let pausing = || {
            let mut s = served_big(128 << 10, "offer /dev/sdb1");
            s.push((answer(D, Answer::Accept), ACCEPTED, "converting"));
            s
        };
        let mut core = test_core(true);
        let mut steps = pausing();
        steps.push((Input::Shutdown, "", "converting"));
        steps.push((
            reply(D, "prepare-eject", false),
            "notify /dev/sdb1:convert; stop /dev/sdb1; inhibit off",
            "stopping",
        ));
        steps.push((ev(D, UNMOUNTED), "", "stopping"));
        steps.push((exit_(D, 0), "rmmp /media/u/K; rescan /dev/sdb1; exit", "safe_to_remove"));
        run("shutdown", &mut core, steps);

        let mut core = test_core(true);
        let mut steps = pausing();
        steps.push((Input::SetEnabled(false), "", "converting"));
        steps.push((
            reply(D, "prepare-eject", false),
            "notify /dev/sdb1:convert; stop /dev/sdb1; inhibit off",
            "stopping",
        ));
        steps.push((ev(D, UNMOUNTED), "", "stopping"));
        steps.push((exit_(D, 0), "rmmp /media/u/K; kernel /dev/sdb1 rw", "mounting"));
        run("off", &mut core, steps);

        // The eject needed the same pause: it cannot go ahead, and the user is told so.
        let mut core = test_core(true);
        let mut steps = pausing();
        steps.push((Input::Eject { dev: D.into() }, "", "converting"));
        steps.push((
            reply(D, "prepare-eject", false),
            "notify /dev/sdb1:convert; ctl /dev/sdb1 resume; notify /dev/sdb1:saving; inhibit off",
            "ready",
        ));
        run("eject", &mut core, steps);
        assert!(!core.drives[D].eject_after);
    }

    /// Two sticks cloned from one image share the volume serial and the label, not the disk's
    /// hardware serial: an answer for one is not an answer for the other.
    #[test]
    fn clones_with_different_disk_serials_are_asked_separately() {
        let one = DevInfo { disk_serial: Some("AA01".into()), ..info(D, "K") };
        let two = DevInfo { disk_serial: Some("BB02".into()), ..info(D, "K") };
        assert_eq!(volume_key(&one).as_deref(), Some("1A2B-3C4D:AA01:"));
        assert_ne!(volume_key(&one), volume_key(&two));
        let mut core = test_core(true);
        core.answered.insert(volume_key(&one).unwrap());
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps[0].0 = Input::Added(two);
        run("clone", &mut core, steps);
        let mut core = test_core(true);
        core.answered.insert(volume_key(&one).unwrap());
        let mut steps = served_big(128 << 10, "");
        steps[0].0 = Input::Added(one);
        run("same stick", &mut core, steps);
    }

    /// The answer could not be written: the user hears it once ("it may ask again"), nothing
    /// claims it was saved.
    #[test]
    fn an_unsaved_answer_is_said_once() {
        let mut core = test_core(true);
        let mut steps = served_big(128 << 10, "offer /dev/sdb1");
        steps.push((answer(D, Answer::NotNow), "remember 1A2B-3C4D::K declined", "ready"));
        steps.push((Input::RememberFailed { dev: D.into() }, "notify /dev/sdb1:offer", "ready"));
        steps.push((Input::RememberFailed { dev: D.into() }, "", "ready"));
        run("unsaved", &mut core, steps);
    }

    #[test]
    fn recluster_lines_parse() {
        let c = ReclusterCheck::parse(
            r#"{"event":"check","op":"recluster","benefit":true,"reason":"ok","message":"x","cluster":131072,"new_cluster":4096,"free":1,"need_free":1,"write_bytes":1,"est_secs":3.2,"finish":false}"#,
            Some(0),
        )
        .unwrap();
        assert_eq!(c, ReclusterCheck { benefit: true, reason: "ok".into(), cluster: 131072, secs: 4, finish: false });
        // Exit 3 is never a benefit, whatever the line says; finish is read with either code.
        let c = ReclusterCheck::parse(
            r#"{"event":"check","op":"recluster","benefit":true,"reason":"ok","cluster":4096,"est_secs":0.0,"finish":true}"#,
            Some(3),
        )
        .unwrap();
        assert!(!c.benefit && c.finish && c.secs == 1);
        assert_eq!(ReclusterCheck::parse(r#"{"event":"check","ok":true}"#, Some(0)), None, "mount --check's line");
        assert_eq!(ReclusterCheck::parse("garbage", Some(0)), None);
        assert_eq!(
            ReclusterEvent::parse(r#"{"event":"refused","op":"recluster","reason":"dirty","message":"m"}"#),
            ReclusterEvent::Refused { reason: "dirty".into(), message: "m".into() }
        );
        assert_eq!(secs_text(7), "7 seconds");
        assert_eq!(secs_text(200), "4 minutes");
        let (t, b) = offer_texts("KINGSTON", 7);
        assert_eq!(t, "Make KINGSTON faster for small files?");
        assert!(b.contains("Nothing is erased") && b.contains("about 7 seconds") && !b.contains("cluster"), "{b}");
        assert_eq!(volume_key(&info(D, "K\tX")).as_deref(), Some("1A2B-3C4D::KX"));
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
