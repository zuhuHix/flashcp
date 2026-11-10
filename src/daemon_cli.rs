//! `flashcp drived` (the daemon) and `flashcp drive-mode status|on|off|eject` (its client).

use crate::daemon_io::{self, Env};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::io::{BufRead, BufReader, Write};
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
}

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
                format!(r#"{{"ok":true,"running":false,"enabled":{on},"drives":[]}}"#)
            });
            if json {
                println!("{reply}");
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
                println!("{reply}");
            } else if reply.contains("\"ok\":true") {
                println!("ejecting {device}: you'll get a notification when it can be removed");
            } else {
                bail!("{reply}");
            }
            Ok(())
        }
    }
}

fn set_user(on: bool, json: bool) -> Result<()> {
    daemon_io::write_enabled(&daemon_io::config_path(), on)?;
    let running = ask(if on { "enable" } else { "disable" }).is_some();
    if json {
        println!(r#"{{"ok":true,"enabled":{on},"running":{running}}}"#);
    } else {
        println!(
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
        println!(r#"{{"ok":true,"system_enabled":{on},"udev_reloaded":{reloaded}}}"#);
    } else {
        println!(
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
    println!(
        "drive mode: {}{}",
        if flag("enabled") { "on" } else { "off" },
        if flag("running") { "" } else { " (the drive service is not running)" }
    );
    if Path::new(SYSTEM_OFF).exists() {
        println!("  disabled system-wide ({SYSTEM_OFF})");
    }
    // The drives array: split its objects (they are flat).
    let Some(arr) = reply.split_once("\"drives\":[").map(|(_, a)| a).filter(|a| !a.starts_with(']')) else { return };
    for obj in arr.split("},{") {
        let obj = format!("{{{}}}", obj.trim_start_matches('{').trim_end_matches(['}', ']']).trim_end_matches('}'));
        let Some(d) = crate::daemon::json::parse_flat(&obj) else { continue };
        let g = |k: &str| d.get(k).cloned().unwrap_or_default();
        let bytes: u64 = g("dirty_bytes").parse().unwrap_or(0);
        let pending = if bytes > 0 { format!(", saving {}", crate::daemon::mb(bytes)) } else { String::new() };
        println!(
            "  {} {:?}: {} ({}{}){}",
            g("device"),
            g("label"),
            g("mode"),
            g("state"),
            pending,
            if g("mountpoint") != "null" && !g("mountpoint").is_empty() {
                format!(" at {}", g("mountpoint"))
            } else {
                String::new()
            }
        );
    }
}
