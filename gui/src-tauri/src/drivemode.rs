//! Drive mode: the per-user `flashcp drived` daemon that serves USB exFAT drives through flashcp.
//! The app never talks to the daemon; it runs `flashcp drive-mode ...` and parses its one JSON line.

use crate::runner::Invocation;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
const SET_TIMEOUT: Duration = Duration::from_secs(60);
const EJECT_TIMEOUT: Duration = Duration::from_secs(120);
/// A conversion of a big, full stick can take minutes; the window shows its progress meanwhile.
const RECLUSTER_TIMEOUT: Duration = Duration::from_secs(3600);

/// One drive as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveModeDrive {
    pub device: String,
    #[serde(default)]
    pub label: String,
    /// "flashcp", "kernel" or "none".
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub mountpoint: Option<String>,
    #[serde(default, alias = "dirty_bytes")]
    pub dirty_bytes: u64,
    #[serde(default, alias = "dirty_files")]
    pub dirty_files: u64,
    #[serde(default, alias = "safe_to_remove")]
    pub safe_to_remove: bool,
    /// "Make faster" can run: the volume would gain from smaller clusters.
    #[serde(default, alias = "can_convert")]
    pub can_convert: bool,
    /// About how many seconds the conversion takes (its estimate while it runs).
    #[serde(default, alias = "convert_secs")]
    pub convert_secs: u64,
    #[serde(default, alias = "convert_done")]
    pub convert_done: u64,
    #[serde(default, alias = "convert_total")]
    pub convert_total: u64,
    /// The last conversion: "done", "refused" or "failed", and what the user was told.
    #[serde(default, alias = "convert_result")]
    pub convert_result: Option<String>,
    #[serde(default, alias = "convert_message")]
    pub convert_message: Option<String>,
    /// This plug-in's token: "Make faster" sends it back, so a dialog opened for one stick is
    /// never applied to another one that got the same device path.
    #[serde(default)]
    pub plug: u64,
}

/// The daemon and setting state for the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveModeStatus {
    pub running: bool,
    pub enabled: bool,
    pub drives: Vec<DriveModeDrive>,
}

/// The reply to on/off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveModeSet {
    pub enabled: bool,
    pub running: bool,
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    running: bool,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    drives: Vec<DriveModeDrive>,
}

/// The CLI's reply: the last non-empty stdout line as JSON; `ok:false` becomes its error text.
fn envelope(stdout: &str) -> Result<Envelope, String> {
    let line = stdout.lines().map(str::trim).rev().find(|l| !l.is_empty()).ok_or("flashcp gave no answer")?;
    let env: Envelope =
        serde_json::from_str(line).map_err(|_| "flashcp gave an answer the app cannot read".to_string())?;
    if env.ok {
        Ok(env)
    } else {
        Err(env.error.filter(|e| !e.is_empty()).unwrap_or_else(|| "flashcp reported an error".to_string()))
    }
}

pub fn parse_status(stdout: &str) -> Result<DriveModeStatus, String> {
    let env = envelope(stdout)?;
    Ok(DriveModeStatus { running: env.running, enabled: env.enabled, drives: env.drives })
}

pub fn parse_set(stdout: &str) -> Result<DriveModeSet, String> {
    let env = envelope(stdout)?;
    Ok(DriveModeSet { enabled: env.enabled, running: env.running })
}

pub fn parse_ok(stdout: &str) -> Result<(), String> {
    envelope(stdout).map(|_| ())
}

#[derive(Deserialize)]
struct ReclusterReply {
    #[serde(default)]
    message: String,
}

/// The reply of `drive-mode recluster --json` that exited 0: the text to show.
pub fn parse_recluster(stdout: &str) -> Result<String, String> {
    envelope(stdout)?;
    let line = stdout.lines().map(str::trim).rev().find(|l| !l.is_empty()).unwrap_or("");
    let r: ReclusterReply = serde_json::from_str(line).map_err(|e| e.to_string())?;
    Ok(r.message)
}

/// Only plain `/dev/...` device paths reach the CLI.
pub fn validate_device(device: &str) -> Result<(), String> {
    let plain = device.len() > "/dev/".len()
        && device.starts_with("/dev/")
        && !device.chars().any(|c| c.is_whitespace() || c.is_control())
        && !device.split('/').any(|part| part == "..");
    if plain {
        Ok(())
    } else {
        Err("Not a valid device path".to_string())
    }
}

/// The error to show for a failed run: the JSON error if there is one, else stderr, else the code.
fn failure(stdout: &str, stderr: &str, code: Option<i32>) -> String {
    if let Err(e) = envelope(stdout) {
        if e != "flashcp gave no answer" && e != "flashcp gave an answer the app cannot read" {
            return e;
        }
    }
    let err = stderr.trim();
    if !err.is_empty() {
        return err.lines().next().unwrap_or(err).to_string();
    }
    match code {
        Some(c) => format!("flashcp exited with code {c}"),
        None => "flashcp was stopped".to_string(),
    }
}

/// Runs `program args...`, killing it after `timeout`. Returns stdout when it exits 0.
fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not run flashcp: {e}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut p) = pipe {
                let mut bytes = Vec::new();
                let _ = p.read_to_end(&mut bytes);
                text = String::from_utf8_lossy(&bytes).into_owned();
            }
            text
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("flashcp did not answer within {} seconds", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("Could not wait for flashcp: {e}")),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        Err(failure(&stdout, &stderr, status.code()))
    }
}

/// The flashcp program for drive-mode: the dev override, else `flashcp` on PATH.
fn program() -> String {
    match Invocation::from_env() {
        Invocation::Direct(path) => path,
        Invocation::Helper => "flashcp".to_string(),
    }
}

async fn blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn drive_mode_status() -> Result<DriveModeStatus, String> {
    blocking(|| parse_status(&run(&program(), &["drive-mode", "status", "--json"], STATUS_TIMEOUT)?)).await
}

#[tauri::command]
pub async fn drive_mode_set(enabled: bool) -> Result<DriveModeSet, String> {
    let word = if enabled { "on" } else { "off" };
    blocking(move || parse_set(&run(&program(), &["drive-mode", word, "--json"], SET_TIMEOUT)?)).await
}

#[tauri::command]
pub async fn drive_mode_eject(device: String) -> Result<(), String> {
    validate_device(&device)?;
    blocking(move || parse_ok(&run(&program(), &["drive-mode", "eject", &device, "--json"], EJECT_TIMEOUT)?)).await
}

/// "Make faster": the daemon pauses the drive, converts it and mounts it again. Returns the
/// result text; a refusal or failure comes back as the error (the drive works as before).
#[tauri::command]
pub async fn drive_mode_recluster(device: String, plug: u64) -> Result<String, String> {
    validate_device(&device)?;
    blocking(move || {
        let plug = plug.to_string();
        let args = ["drive-mode", "recluster", device.as_str(), "--yes", "--plug", plug.as_str(), "--json"];
        parse_recluster(&run(&program(), &args, RECLUSTER_TIMEOUT)?)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_replies_parse() {
        let running = r#"{"ok":true,"running":true,"enabled":true,"drives":[{"device":"/dev/sdb1","label":"KINGSTON","mode":"flashcp","state":"saving","mountpoint":"/media/u/KINGSTON","dirty_bytes":12000000,"dirty_files":3,"safe_to_remove":false}]}"#;
        let stopped = "{\"ok\":true,\"running\":false,\"enabled\":true,\"drives\":[]}\n";
        let s = parse_status(running).unwrap();
        assert!(s.running && s.enabled);
        assert_eq!(s.drives.len(), 1);
        assert_eq!(s.drives[0].dirty_bytes, 12_000_000);
        assert_eq!(s.drives[0].mountpoint.as_deref(), Some("/media/u/KINGSTON"));
        assert_eq!(parse_status(stopped).unwrap(), DriveModeStatus { running: false, enabled: true, drives: vec![] });
        assert!(parse_status("").is_err());
        assert!(parse_status("not json").unwrap_err().contains("cannot read"));
        assert_eq!(parse_status(r#"{"ok":false,"error":"no config"}"#).unwrap_err(), "no config");
        assert_eq!(parse_status(r#"{"ok":false}"#).unwrap_err(), "flashcp reported an error");
        assert_eq!(
            parse_set(r#"{"ok":true,"enabled":false,"running":true}"#).unwrap(),
            DriveModeSet { enabled: false, running: true }
        );
        assert!(parse_ok(r#"{"ok":true}"#).is_ok());
        let conv = r#"{"ok":true,"running":true,"enabled":true,"offers":true,"drives":[{"device":"/dev/sdb1","label":"K","mode":"flashcp","state":"converting","mountpoint":"/media/u/K","dirty_bytes":0,"dirty_files":0,"safe_to_remove":false,"can_convert":true,"cluster":131072,"convert_secs":7,"convert_done":5,"convert_total":10,"convert_result":null,"convert_message":null,"plug":4,"volume":"1A2B-3C4D::K"}]}"#;
        let d = &parse_status(conv).unwrap().drives[0];
        assert!(d.can_convert && d.convert_secs == 7 && d.convert_done == 5 && d.convert_total == 10);
        assert_eq!(d.plug, 4);
        assert_eq!(d.convert_result, None);
        assert_eq!(
            parse_recluster(
                r#"{"ok":true,"result":"done","message":"K is ready. Small files now copy to it much faster."}"#
            )
            .unwrap(),
            "K is ready. Small files now copy to it much faster."
        );
        assert_eq!(
            failure(
                r#"{"ok":false,"result":"refused","message":"K was not changed.","error":"K was not changed."}"#,
                "",
                Some(3)
            ),
            "K was not changed."
        );
        assert_eq!(parse_ok(r#"{"ok":false,"error":"busy"}"#).unwrap_err(), "busy");
    }

    #[test]
    fn failures_prefer_json_then_stderr_then_code() {
        assert_eq!(failure(r#"{"ok":false,"error":"busy"}"#, "x", Some(1)), "busy");
        assert_eq!(failure("", "boom\nmore", Some(1)), "boom");
        assert_eq!(failure("", "", Some(3)), "flashcp exited with code 3");
    }

    #[test]
    fn only_plain_dev_paths_are_accepted() {
        for ok in ["/dev/sdb1", "/dev/disk/by-id/usb-X-part1"] {
            assert!(validate_device(ok).is_ok(), "{ok}");
        }
        for bad in
            ["", "/dev/", "sdb1", "/tmp/x", "/dev/sdb1 --force", "/dev/sd\tb", "/dev/sd\nb", "/dev/../etc", "--help"]
        {
            assert!(validate_device(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_slow_command_is_killed() {
        let err = run("sh", &["-c", "sleep 5"], Duration::from_millis(100)).unwrap_err();
        assert!(err.contains("did not answer"));
        assert_eq!(run("sh", &["-c", "echo hi"], Duration::from_secs(5)).unwrap().trim(), "hi");
    }
}
