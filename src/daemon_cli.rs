//! `flashcp drived` (the daemon) and `flashcp drive-mode status|on|off|eject|recluster|offer`
//! (its client).

use crate::daemon_io::{self, Env};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// The system-wide off switch the udev rule tests (`flashcp drive-mode off --system`).
pub const SYSTEM_OFF: &str = "/etc/flashcp/drive-mode-disabled";

#[derive(Subcommand)]
pub enum DriveModeCmd {
    /// Show whether drive mode is on and how each USB drive is served
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Speed up USB drives: new USB exFAT drives are served by flashcp (the default)
    On {
        /// System-wide (root): undo `off --system`
        #[arg(long)]
        system: bool,
        #[arg(long)]
        json: bool,
    },
    /// Use the normal (kernel) driver: current drives are flushed and remounted normally
    Off {
        /// System-wide (root): the udev rule stops taking over drives for every user (rollback)
        #[arg(long)]
        system: bool,
        #[arg(long)]
        json: bool,
    },
    /// Save everything, unmount and power off a drive (e.g. /dev/sdb1)
    Eject {
        device: String,
        #[arg(long)]
        json: bool,
    },
    /// Make a drive flashcp serves faster for small files (`flashcp recluster`, run by the
    /// drive service: it pauses the drive, converts it and mounts it again). Nothing is erased
    Recluster {
        device: String,
        /// Do not ask first
        #[arg(long)]
        yes: bool,
        /// Only if the drive is still the plug-in with this `plug` number (from `status --json`,
        /// as the app passes it): refused when another drive got its device path meanwhile
        #[arg(long, value_name = "N")]
        plug: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Turn the "Make this drive faster?" notification on or off (the app button stays)
    Offer {
        #[arg(value_parser = ["on", "off"])]
        state: String,
        #[arg(long)]
        json: bool,
    },
}

/// Print to stdout; a closed pipe (`... | head -1`) ends the program quietly instead of a panic.
fn emit(text: &str) {
    let mut o = std::io::stdout().lock();
    if let Err(e) = o.write_all(text.as_bytes()).and_then(|_| o.flush()) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("flashcp: cannot write the output: {e}");
        std::process::exit(1);
    }
}

macro_rules! say {
    ($($t:tt)*) => { emit(&format!("{}\n", format_args!($($t)*))) };
}

/// The one-sentence Windows caveat of a conversion (docs/drive-mode-interface.md).
pub const WINDOWS_NOTE: &str = "If it loses power in the second it switches over, Windows may show it as \
    unformatted until you plug it in here again; your files stay safe.";

/// Ask the running daemon; None if it is not running.
fn ask(line: &str) -> Option<String> {
    let env = Env::from_env().ok()?;
    let mut s = UnixStream::connect(env.socket()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok()?;
    s.write_all(format!("{line}\n").as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).ok()?;
    Some(reply.trim().to_string())
}

pub fn run_drived() -> Result<()> {
    daemon_io::run(Env::from_env()?)
}

pub fn drive_mode(cmd: DriveModeCmd) -> Result<()> {
    match cmd {
        DriveModeCmd::Status { json } => {
            let reply = ask("status").unwrap_or_else(|| {
                let on = daemon_io::read_enabled(&daemon_io::config_path());
                let offers = daemon_io::read_offers(&daemon_io::config_path());
                format!(r#"{{"ok":true,"running":false,"enabled":{on},"offers":{offers},"drives":[]}}"#)
            });
            if json {
                say!("{reply}");
            } else {
                print_status(&reply);
            }
            Ok(())
        }
        DriveModeCmd::On { system: true, json } => set_system(true, json),
        DriveModeCmd::Off { system: true, json } => set_system(false, json),
        DriveModeCmd::On { json, .. } => set_user(true, json),
        DriveModeCmd::Off { json, .. } => set_user(false, json),
        DriveModeCmd::Eject { device, json } => {
            anyhow::ensure!(device.starts_with("/dev/") || Path::new(&device).is_absolute(), "not a device: {device}");
            let reply = ask(&format!("eject {device}")).context("the drive service (flashcp drived) is not running")?;
            if json {
                say!("{reply}");
            } else if reply.contains("\"ok\":true") {
                say!("ejecting {device}: you'll get a notification when it can be removed");
            } else {
                bail!("{reply}");
            }
            Ok(())
        }
        DriveModeCmd::Recluster { device, yes, plug, json } => recluster(&device, yes, plug, json),
        DriveModeCmd::Offer { state, json } => {
            let on = state == "on";
            daemon_io::write_offers(&daemon_io::config_path(), on)?;
            let running = ask(&format!("offers {state}")).is_some();
            if json {
                say!(r#"{{"ok":true,"offers":{on},"running":{running}}}"#);
            } else if on {
                say!("flashcp offers once per drive to make it faster (a notification with Accept / Not now)");
            } else {
                say!("no more offers to make drives faster; the app's \"Make faster\" button still works");
            }
            Ok(())
        }
    }
}

/// The objects of the status reply's `drives` array (they are flat).
fn drives(reply: &str) -> Vec<BTreeMap<String, String>> {
    let Some(arr) = reply.split_once("\"drives\":[").map(|(_, a)| a).filter(|a| !a.starts_with(']')) else {
        return vec![];
    };
    arr.split("},{")
        .filter_map(|obj| {
            let obj = format!("{{{}}}", obj.trim_start_matches('{').trim_end_matches(['}', ']']).trim_end_matches('}'));
            crate::daemon::json::parse_flat(&obj)
        })
        .collect()
}

fn drive_of(reply: &str, device: &str) -> Option<BTreeMap<String, String>> {
    drives(reply).into_iter().find(|d| d.get("device").is_some_and(|v| v == device))
}

/// `flashcp drive-mode recluster DEV`: the daemon does the work (the same path as the
/// notification's Accept); this prints what it says and waits for the end.
fn recluster(device: &str, yes: bool, expect_plug: Option<u64>, json: bool) -> Result<()> {
    let no_daemon = "the drive service (flashcp drived) is not running";
    let refuse = |why: &str| -> Result<()> {
        if json {
            say!(r#"{{"ok":false,"error":{}}}"#, crate::events::json_str(why));
            std::process::exit(3);
        }
        bail!("{why}");
    };
    let reply = ask("status").context(no_daemon)?;
    let Some(d) = drive_of(&reply, device) else {
        if expect_plug.is_some() {
            return refuse(crate::daemon::REPLACED);
        }
        let served: Vec<String> = drives(&reply).iter().filter_map(|d| d.get("device").cloned()).collect();
        bail!("{device} is not a drive the drive service knows (it knows: {})", served.join(", "));
    };
    // The plug-in these texts are about: the daemon refuses if the drive at this path changes.
    let plug: u64 = d.get("plug").and_then(|v| v.parse().ok()).unwrap_or(0);
    if expect_plug.is_some_and(|p| p != plug) {
        return refuse(crate::daemon::REPLACED);
    }
    let name = d.get("label").cloned().unwrap_or_else(|| device.to_string());
    let secs: u64 = d.get("convert_secs").and_then(|v| v.parse().ok()).unwrap_or(0);
    if !json {
        let (title, body) = crate::daemon::offer_texts(&name, secs.max(1));
        say!("{title}\n{body}\n{WINDOWS_NOTE}");
    }
    if !yes {
        anyhow::ensure!(std::io::stdin().is_terminal(), "not a terminal: add --yes to go ahead");
        emit("Go ahead? [y/N] ");
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if !matches!(a.trim(), "y" | "Y" | "yes") {
            say!("nothing was changed");
            return Ok(());
        }
    }
    let r = ask(&format!("recluster {device} {plug}")).context(no_daemon)?;
    if !r.contains("\"ok\":true") {
        let m = crate::daemon::json::parse_flat(&r).unwrap_or_default();
        let why = m.get("error").cloned().unwrap_or(r);
        if json {
            say!(r#"{{"ok":false,"error":{}}}"#, crate::events::json_str(&why));
            std::process::exit(3);
        }
        bail!("{name} cannot be made faster now: {why}");
    }
    let mut shown = false;
    let mut last_pct = None;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let Some(st) = ask("status") else { bail!("the drive service stopped") };
        let Some(d) = drive_of(&st, device).filter(|d| d.get("plug").is_some_and(|p| *p == plug.to_string())) else {
            bail!("{name} was removed")
        };
        let g = |k: &str| d.get(k).cloned().unwrap_or_default();
        let state = g("state");
        if state == "converting" {
            let (done, total): (u64, u64) =
                (g("convert_done").parse().unwrap_or(0), g("convert_total").parse().unwrap_or(0));
            let pct = (total > 0).then(|| done * 100 / total);
            if !json && (!shown || pct != last_pct) {
                let p = pct.map(|p| format!(" {p} %")).unwrap_or_default();
                say!("Making {name} faster… don't unplug it{p}");
                (shown, last_pct) = (true, pct);
            }
            continue;
        }
        let result = g("convert_result");
        if ["checking", "starting", "mounting"].contains(&state.as_str()) || result.is_empty() || result == "null" {
            continue;
        }
        let message = g("convert_message");
        if json {
            let m = crate::events::json_str(&message);
            let err = if result == "done" { String::new() } else { format!(r#","error":{m}"#) };
            say!(r#"{{"ok":{},"result":{},"message":{m}{err}}}"#, result == "done", crate::events::json_str(&result));
        } else {
            say!("{message}");
        }
        std::process::exit(match result.as_str() {
            "done" => 0,
            "refused" => 3,
            _ => 1,
        });
    }
}

fn set_user(on: bool, json: bool) -> Result<()> {
    daemon_io::write_enabled(&daemon_io::config_path(), on)?;
    let running = ask(if on { "enable" } else { "disable" }).is_some();
    if json {
        say!(r#"{{"ok":true,"enabled":{on},"running":{running}}}"#);
    } else {
        say!(
            "drive mode {}{}",
            if on { "on: new USB exFAT drives are served by flashcp" } else { "off: USB drives use the normal driver" },
            if running { "" } else { " (the drive service is not running; it applies when it starts)" }
        );
    }
    Ok(())
}

/// The administrator's switch: a flag file the udev rule tests, then re-run the rules for block
/// devices so drives already plugged in are handed back to udisks (or taken over again).
fn set_system(on: bool, json: bool) -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "--system needs root (sudo flashcp drive-mode ...)");
    if on {
        match std::fs::remove_file(SYSTEM_OFF) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context(SYSTEM_OFF),
        }
    } else {
        std::fs::create_dir_all("/etc/flashcp")?;
        std::fs::write(SYSTEM_OFF, "# flashcp drive mode is off for every user (flashcp drive-mode on --system)\n")?;
    }
    let udev = |args: &[&str]| std::process::Command::new("udevadm").args(args).status().map(|s| s.success());
    let reloaded = udev(&["control", "--reload"]).unwrap_or(false)
        && udev(&["trigger", "--subsystem-match=block", "--action=change", "--property-match=ID_FS_TYPE=exfat"])
            .unwrap_or(false);
    if json {
        say!(r#"{{"ok":true,"system_enabled":{on},"udev_reloaded":{reloaded}}}"#);
    } else {
        say!(
            "drive mode {} for every user{}",
            if on { "allowed" } else { "disabled" },
            if reloaded { "" } else { " (udevadm failed: replug the drives)" }
        );
    }
    Ok(())
}

fn print_status(reply: &str) {
    let top = crate::daemon::json::parse_flat(reply).unwrap_or_default();
    let flag = |k: &str| top.get(k).is_some_and(|v| v == "true");
    let mut text = format!(
        "drive mode: {}{}\n",
        if flag("enabled") { "on" } else { "off" },
        if flag("running") { "" } else { " (the drive service is not running)" }
    );
    if Path::new(SYSTEM_OFF).exists() {
        text += &format!("  disabled system-wide ({SYSTEM_OFF})\n");
    }
    // Older daemons do not report it: the setting decides.
    let offers =
        top.get("offers").map(|v| v == "true").unwrap_or_else(|| daemon_io::read_offers(&daemon_io::config_path()));
    text +=
        &format!("offer to make drives faster: {}\n", if offers { "on" } else { "off (flashcp drive-mode offer on)" });
    for d in drives(reply) {
        let g = |k: &str| d.get(k).cloned().unwrap_or_default();
        let bytes: u64 = g("dirty_bytes").parse().unwrap_or(0);
        let mut extra = if bytes > 0 { format!(", saving {}", crate::daemon::mb(bytes)) } else { String::new() };
        let (done, total): (u64, u64) =
            (g("convert_done").parse().unwrap_or(0), g("convert_total").parse().unwrap_or(0));
        if g("state") == "converting" && total > 0 {
            extra += &format!(", {} %", done * 100 / total);
        } else if g("can_convert") == "true" {
            extra += &format!(", can be made faster (flashcp drive-mode recluster {})", g("device"));
        }
        let at = g("mountpoint");
        text += &format!(
            "  {} {:?}: {} ({}{}){}\n",
            g("device"),
            g("label"),
            g("mode"),
            g("state"),
            extra,
            if at != "null" && !at.is_empty() { format!(" at {at}") } else { String::new() }
        );
    }
    emit(&text);
}
