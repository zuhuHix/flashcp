//! flashcp desktop app: Tauri shell, IPC commands, tray, udisks watcher and notifications.

mod drivemode;
mod drives;
mod events;
mod job;
mod optimize;
mod pull;
mod runner;
mod settings;
mod trust;
mod udisks;
// The placeholder-identity rules are shared verbatim with the root helper.
#[path = "../../helper/src/weak.rs"]
mod weak;

use drives::{expected_confirm, require_trusted, Drive};
use job::{JobLock, Request};
use optimize::OptimizePlan;
use pull::{DestStatus, PullDefaults, PullRequest};
use runner::{Action, Invocation, Source};
use settings::{Paths, Settings};
use std::collections::HashSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, AppHandle, Emitter, Manager, State, WindowEvent};

const MAIN_WINDOW: &str = "main";
const NOTIFY_DELAY: Duration = Duration::from_secs(1);

/// Shared app state. Mutexes are never held across blocking calls.
pub struct AppState {
    job_running: Arc<AtomicBool>,
    pending: Mutex<Vec<String>>,
    /// A folder to copy from, from `--pull <path>` or a second instance, until the window takes it.
    pending_pull: Mutex<Option<PullRequest>>,
    /// Drive ids seen at the last refresh, used to notify only for newly plugged drives.
    known: Mutex<HashSet<String>>,
    /// Confirmations `optimize_plan` handed out; `start_optimize` redeems one.
    plans: Mutex<optimize::Plans>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            job_running: Arc::new(AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            pending_pull: Mutex::new(None),
            known: Mutex::new(HashSet::new()),
            plans: Mutex::new(optimize::Plans::default()),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the command line asks for: existing files to copy, and the folder of `--pull <path>`.
#[derive(Debug, Default, PartialEq)]
struct Argv {
    files: Vec<String>,
    pull: Option<String>,
}

fn absolute(arg: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(arg);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Splits arguments into files to copy (absolute, existing; flags skipped) and the folder that
/// follows `--pull`. The value after `--pull` is never treated as a file.
fn split_args<I>(args: I, cwd: &Path) -> Argv
where
    I: IntoIterator<Item = String>,
{
    let mut out = Argv::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--pull" {
            out.pull = args.next().filter(|p| !p.is_empty()).map(|p| absolute(&p, cwd).to_string_lossy().into_owned());
        } else if !arg.is_empty() && !arg.starts_with('-') {
            let p = absolute(&arg, cwd);
            if p.exists() {
                out.files.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn find_drive(drives: Vec<Drive>, id: &str) -> Result<Drive, String> {
    drives.into_iter().find(|d| d.id == id).ok_or_else(|| "Drive not found. Is it still plugged in?".to_string())
}

fn require_exfat(drive: &Drive) -> Result<(), String> {
    if drive.is_exfat {
        return Ok(());
    }
    Err(format!(
        "{} is not exFAT ({}). Use erase to format it.",
        drive.device,
        if drive.fs_type.is_empty() { "no filesystem" } else { &drive.fs_type }
    ))
}

async fn drives_now() -> Result<Vec<Drive>, String> {
    tauri::async_runtime::spawn_blocking(udisks::list_drives).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_drives() -> Result<Vec<Drive>, String> {
    drives_now().await
}

#[tauri::command]
fn get_settings() -> Result<Settings, String> {
    Ok(settings::load(&Paths::from_env()?))
}

#[tauri::command]
fn set_settings(settings: Settings) -> Result<Settings, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find own path: {e}"))?;
    settings::save(&Paths::from_env()?, settings, &exe)
}

#[tauri::command]
fn take_pending_files(state: State<'_, AppState>) -> Vec<String> {
    std::mem::take(&mut *lock(&state.pending))
}

#[tauri::command]
async fn start_copy(
    app: AppHandle,
    state: State<'_, AppState>,
    drive_id: String,
    sources: Vec<String>,
    dest_dir: String,
    skip_existing: bool,
) -> Result<(), String> {
    let lock = JobLock::acquire(&state.job_running)?;
    if sources.is_empty() {
        return Err("Nothing to copy".to_string());
    }
    let sources = sources.iter().map(|s| Source::probe(s)).collect::<Result<Vec<_>, _>>()?;
    let drive = find_drive(drives_now().await?, &drive_id)?;
    require_exfat(&drive)?;
    require_trusted(&drive)?;
    let stage_dir = if sources.iter().any(|s| !s.is_dir) { runner::new_stage_dir()? } else { PathBuf::new() };
    let plan = runner::plan_items(&sources, &dest_dir, &stage_dir)?;
    let request = Request {
        op: events::Op::Add,
        drive,
        action: Action::Add { items: plan.items, skip_existing },
        total: sources.len(),
        stage_dir: plan.stage_dir,
        staged: plan.staged,
    };
    job::spawn(app, request, lock)
}

#[tauri::command]
async fn start_erase(
    app: AppHandle,
    state: State<'_, AppState>,
    drive_id: String,
    sources: Vec<String>,
    confirm: String,
) -> Result<(), String> {
    let lock = JobLock::acquire(&state.job_running)?;
    if sources.len() != 1 {
        return Err("Erase takes exactly one source folder".to_string());
    }
    let source = Source::probe(&sources[0])?;
    if !source.is_dir {
        return Err("Erase takes a folder, not a file".to_string());
    }
    let drive = find_drive(drives_now().await?, &drive_id)?;
    require_trusted(&drive)?;
    if confirm != expected_confirm(&drive) {
        return Err("Confirmation text does not match".to_string());
    }
    // Keep the existing label so the stick keeps its name after the erase.
    let label = drive.label.clone();
    let request = Request {
        op: events::Op::Build,
        drive,
        action: Action::Build { src: source.path, confirm, label },
        total: 1,
        stage_dir: PathBuf::new(),
        staged: Vec::new(),
    };
    job::spawn(app, request, lock)
}

/// Copies files off a drive into a local folder (flashcp pull, through the helper or directly).
#[tauri::command]
async fn start_pull(
    app: AppHandle,
    state: State<'_, AppState>,
    drive_id: String,
    from: String,
    dest: String,
    existing: String,
) -> Result<(), String> {
    let lock = JobLock::acquire(&state.job_running)?;
    let from = pull::normalize_from(&from)?;
    let dest = pull::local_dest(&dest)?;
    let existing = pull::Existing::parse(&existing)?;
    let drive = find_drive(drives_now().await?, &drive_id)?;
    require_exfat(&drive)?;
    require_trusted(&drive)?;
    if pull::is_on_drive(&dest, &drive) {
        return Err("The destination is on the USB drive itself. Choose a folder on this computer.".to_string());
    }
    let request = Request {
        op: events::Op::Pull,
        drive,
        action: Action::Pull { from, existing, dest },
        total: 1,
        stage_dir: PathBuf::new(),
        staged: Vec::new(),
    };
    job::spawn(app, request, lock)
}

/// Optimize works on exFAT sticks and on blank ones: a half-built stick from an interrupted optimize
/// has no filesystem, and flashcp decides whether it can resume.
fn require_optimizable(drive: &Drive) -> Result<(), String> {
    if drive.is_exfat || drive.fs_type.is_empty() {
        return Ok(());
    }
    Err(format!("{} is not exFAT ({}). Optimize only works on exFAT sticks.", drive.device, drive.fs_type))
}

/// flashcp optimize --dry-run: plans the rewrite without touching the stick, then remounts the
/// stick if it was mounted. Blocks until flashcp exits.
fn optimize_dry_run(drive: Drive) -> Result<OptimizePlan, String> {
    let fake = udisks::fake_mode();
    let was_mounted = !fake && drive.mount_point.is_some();
    if was_mounted {
        udisks::unmount(&drive.id).map_err(|e| format!("Could not unmount {}: {e}", drive.device))?;
    }
    let inv = Invocation::from_env();
    let runs = inv.runs(&drive.device, &drive.serial, &Action::Optimize { dry_run: true });
    let result = runner::run_job(inv.program(), &runs, events::Op::Optimize, &[], |_| {});
    let remount_error = if was_mounted { job::remount(&drive, fake).err() } else { None };
    let agg = result.map_err(|failure| {
        let mut message = failure.message;
        if let Some(e) = &remount_error {
            message = format!("{message} (remounting also failed: {e})");
        }
        match failure.recovery {
            Some(recovery) => format!("{message}\n\n{recovery}"),
            None => message,
        }
    })?;
    if let Some(e) = remount_error {
        return Err(format!("The check finished, but the drive could not be remounted: {e}"));
    }
    let plan = agg.plan().ok_or_else(|| "flashcp did not report a plan".to_string())?;
    Ok(optimize::plan_from(
        plan.files,
        plan.dirs,
        plan.bytes,
        plan.write_bytes,
        plan.strategy.as_ref(),
        optimize::free_bytes,
    ))
}

/// What "Optimize this stick" would do to this drive, for the confirmation dialog.
#[tauri::command]
async fn optimize_plan(state: State<'_, AppState>, drive_id: String) -> Result<OptimizePlan, String> {
    let job_lock = JobLock::acquire(&state.job_running)?;
    let drive = find_drive(drives_now().await?, &drive_id)?;
    require_optimizable(&drive)?;
    require_trusted(&drive)?;
    runner::optimize_temp_dir()?;
    let planned = drive.clone();
    let mut plan = tauri::async_runtime::spawn_blocking(move || {
        let _lock = job_lock;
        optimize_dry_run(drive)
    })
    .await
    .map_err(|e| e.to_string())??;
    // Only a plan that can be started gets a confirmation token.
    if !plan.already_optimized && plan.enough_space && !plan.temp_volatile {
        plan.token = lock(&state.plans).issue(&planned, &plan, Instant::now())?;
    }
    Ok(plan)
}

/// The job for a confirmed plan: the token must be one `optimize_plan` issued, unused, unexpired,
/// for this very drive (same udisks id, device and identity tuple as now), with the space still
/// free. Anything else is refused before anything runs.
fn optimize_request(plans: &Mutex<optimize::Plans>, token: &str, drive: Drive) -> Result<Request, String> {
    require_optimizable(&drive)?;
    require_trusted(&drive)?;
    runner::optimize_temp_dir()?;
    lock(plans).redeem(token, &drive, Instant::now(), optimize::free_bytes)?;
    Ok(Request {
        op: events::Op::Optimize,
        drive,
        action: Action::Optimize { dry_run: false },
        total: 1,
        stage_dir: PathBuf::new(),
        staged: Vec::new(),
    })
}

/// Rewrites the drive with flashcp optimize (through the helper or directly). Runs as a job.
#[tauri::command]
async fn start_optimize(
    app: AppHandle,
    state: State<'_, AppState>,
    drive_id: String,
    token: String,
) -> Result<(), String> {
    let lock = JobLock::acquire(&state.job_running)?;
    let drive = find_drive(drives_now().await?, &drive_id).map_err(|_| optimize::DRIVE_CHANGED.to_string())?;
    let request = optimize_request(&state.plans, &token, drive)?;
    job::spawn(app, request, lock)
}

/// True when an optimize run left its working folder behind (an interrupted run to finish).
#[tauri::command]
fn optimize_pending() -> bool {
    runner::optimize_temp_dir().is_ok_and(|dir| optimize::pending_job(&dir))
}

/// Sub-folders of `rel` on a mounted drive, for the folder browser.
#[tauri::command]
async fn list_drive_folders(drive_id: String, rel: String) -> Result<Vec<String>, String> {
    let rel = pull::normalize_from(&rel)?;
    if udisks::fake_mode() {
        return Ok(pull::fake_folders(&rel));
    }
    let drive = find_drive(drives_now().await?, &drive_id)?;
    let mount = drive.mount_point.ok_or("The drive is not mounted, type a folder name instead")?;
    tauri::async_runtime::spawn_blocking(move || pull::list_subdirs(&Path::new(&mount).join(&rel)))
        .await
        .map_err(|e| e.to_string())?
}

/// The default "Copy to" root: the user's Downloads folder.
#[tauri::command]
fn pull_defaults() -> Result<PullDefaults, String> {
    let home = std::env::var("HOME").ok().filter(|h| Path::new(h).is_absolute());
    let user_dirs =
        home.as_ref().and_then(|h| std::fs::read_to_string(Path::new(h).join(".config").join("user-dirs.dirs")).ok());
    Ok(PullDefaults { downloads: pull::downloads_from(user_dirs.as_deref(), home.as_deref())? })
}

/// Whether a destination folder exists, is a folder, and is empty.
#[tauri::command]
fn dest_status(path: String) -> Result<DestStatus, String> {
    pull::dest_status(Path::new(&path))
}

/// The pull request from `--pull <path>` at start-up, taken once.
#[tauri::command]
fn take_pending_pull(state: State<'_, AppState>) -> Option<PullRequest> {
    lock(&state.pending_pull).take()
}

/// Trusts a drive after admin authentication (flashcp-trust). Dev mode marks the fake drive.
#[tauri::command]
async fn trust_drive(app: AppHandle, drive_id: String) -> Result<(), String> {
    let drive = find_drive(drives_now().await?, &drive_id)?;
    if drive.serial.is_empty() {
        return Err(drives::NO_SERIAL_MESSAGE.to_string());
    }
    if udisks::fake_mode() {
        drives::fake_trust(&drive.id);
    } else {
        let shown = drive.clone();
        let result = tauri::async_runtime::spawn_blocking(move || runner::trust_device(&shown))
            .await
            .map_err(|e| e.to_string())?;
        result?;
    }
    let _ = app.emit("drives-changed", ());
    Ok(())
}

/// Remembers new files and tells the window about them.
fn receive_files(app: &AppHandle, files: Vec<String>) {
    if files.is_empty() {
        return;
    }
    lock(&app.state::<AppState>().pending).extend(files.iter().cloned());
    let _ = app.emit("files-received", files);
}

/// Resolves a folder from a second instance (a file manager) and tells the window.
fn receive_pull(app: &AppHandle, path: String) {
    let app = app.clone();
    thread::spawn(move || {
        let request = pull::request_for_path(&udisks::list_drives(), Path::new(&path));
        *lock(&app.state::<AppState>().pending_pull) = Some(request.clone());
        let _ = app.emit("pull-requested", request);
    });
}

/// Refreshes the known-drive set. Drives that were not known before are reported
/// (when `notify` is set and they are exFAT).
fn refresh_known(app: &AppHandle, notify: bool) {
    let state = app.state::<AppState>();
    let mut known = lock(&state.known);
    let current = udisks::list_drives();
    let fresh: Vec<Drive> = current.iter().filter(|d| !known.contains(&d.id)).cloned().collect();
    *known = current.iter().map(|d| d.id.clone()).collect();
    drop(known);

    if notify && notify_enabled() {
        for drive in fresh.iter().filter(|d| d.is_exfat) {
            notify_drive(app, drive);
        }
    }
}

fn notify_enabled() -> bool {
    Paths::from_env().map(|p| settings::load(&p).notify_on_plug).unwrap_or(true)
}

fn on_udisks_change(app: &AppHandle, change: udisks::Change) {
    let _ = app.emit("drives-changed", ());
    let app = app.clone();
    let added = matches!(change, udisks::Change::Added(_));
    thread::spawn(move || {
        thread::sleep(NOTIFY_DELAY);
        refresh_known(&app, added);
    });
}

/// Desktop notification with a "Fast copy" action that opens the window.
fn notify_drive(app: &AppHandle, drive: &Drive) {
    let name = if drive.label.is_empty() { drive.device.clone() } else { drive.label.clone() };
    let summary = format!("USB drive {name} connected");
    let app = app.clone();
    thread::spawn(move || {
        let child = Command::new("notify-send")
            .args([
                "--app-name=flashcp",
                "--icon=flashcp-gui",
                "--action=default=Fast copy",
                "--wait",
                "--",
                &summary,
                "Fast copy files to it with flashcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match child.and_then(|c| c.wait_with_output()) {
            Ok(out) => {
                if String::from_utf8_lossy(&out.stdout).contains("default") {
                    show_main(&app);
                }
            }
            Err(e) => eprintln!("flashcp-gui: notification skipped (notify-send): {e}"),
        }
    });
}

/// Shows the real drive-mode setting on the tray check item (the CLI may take a moment).
fn sync_fast_item(item: &CheckMenuItem<tauri::Wry>) {
    let item = item.clone();
    thread::spawn(move || {
        if let Ok(status) = tauri::async_runtime::block_on(drivemode::drive_mode_status()) {
            let _ = item.set_checked(status.enabled);
        }
    });
}

/// The tray check item was clicked: apply its new state, and revert it if that fails.
fn toggle_fast(app: &AppHandle, item: &CheckMenuItem<tauri::Wry>) {
    let wanted = item.is_checked().unwrap_or(true);
    let (app, item) = (app.clone(), item.clone());
    thread::spawn(move || {
        match tauri::async_runtime::block_on(drivemode::drive_mode_set(wanted)) {
            Ok(done) => {
                let _ = item.set_checked(done.enabled);
            }
            Err(_) => {
                let _ = item.set_checked(!wanted);
            }
        }
        let _ = app.emit("drive-mode-changed", ());
    });
}

fn setup_tray(app: &App) -> Result<(), Box<dyn Error>> {
    let open = MenuItem::with_id(app, "open", "Open flashcp", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let fast = CheckMenuItem::with_id(app, "fast", "Speed up USB drives", true, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &fast, &quit])?;
    sync_fast_item(&fast);
    let fast_item = fast.clone();
    let mut builder = TrayIconBuilder::with_id("flashcp-tray")
        .tooltip("flashcp")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            // The menu has already flipped the check mark; make the setting match, or flip it back.
            "fast" => toggle_fast(app, &fast_item),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn setup_app(app: &mut App) -> Result<(), Box<dyn Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let start_hidden = argv.iter().any(|a| a == "--tray");
    let cwd = std::env::current_dir().unwrap_or_default();
    let parsed = split_args(argv, &cwd);
    *lock(&app.state::<AppState>().pending) = parsed.files;
    if let Some(path) = parsed.pull {
        let request = pull::request_for_path(&udisks::list_drives(), Path::new(&path));
        *lock(&app.state::<AppState>().pending_pull) = Some(request);
    }

    setup_tray(app)?;

    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let handle = app.handle().clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(w) = handle.get_webview_window(MAIN_WINDOW) {
                    let _ = w.hide();
                }
            }
        });
    }
    if !start_hidden {
        show_main(app.handle());
    }

    if !udisks::fake_mode() {
        // Record drives present at startup so they do not trigger a notification.
        let initial: HashSet<String> = udisks::list_drives().into_iter().map(|d| d.id).collect();
        *lock(&app.state::<AppState>().known) = initial;
        let handle = app.handle().clone();
        udisks::watch(Arc::new(move |change| on_udisks_change(&handle, change)));
    }
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            let parsed = split_args(args.into_iter().skip(1), Path::new(&cwd));
            receive_files(app, parsed.files);
            if let Some(path) = parsed.pull {
                receive_pull(app, path);
            }
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_drives,
            get_settings,
            set_settings,
            take_pending_files,
            start_copy,
            start_erase,
            trust_drive,
            start_pull,
            list_drive_folders,
            pull_defaults,
            dest_status,
            take_pending_pull,
            optimize_plan,
            start_optimize,
            optimize_pending,
            drivemode::drive_mode_status,
            drivemode::drive_mode_set,
            drivemode::drive_mode_eject
        ])
        .setup(setup_app)
        .run(tauri::generate_context!())
        .expect("error while running flashcp-gui");
}

#[cfg(test)]
mod tests {
    #[test]
    fn optimize_start_needs_a_valid_token_for_the_same_drive() {
        use crate::optimize::{Plans, DRIVE_CHANGED, PLAN_EXPIRED, PLAN_TTL, PLAN_UNKNOWN};
        let mut d = crate::drives::fake_drives().into_iter().find(|d| d.is_exfat).unwrap();
        d.trusted = true;
        let plan = crate::optimize::plan_from(1, 0, 1, 1, None, |_| None);
        let plans = std::sync::Mutex::new(Plans::default());
        let err = |tok: &str, drive: &Drive| super::optimize_request(&plans, tok, drive.clone()).err().unwrap();
        // No token, an invented one.
        assert_eq!(err("", &d), PLAN_UNKNOWN);
        assert_eq!(err("00112233445566778899aabbccddeeff", &d), PLAN_UNKNOWN);
        // Reused.
        let tok = super::lock(&plans).issue(&d, &plan, std::time::Instant::now()).unwrap();
        let req = super::optimize_request(&plans, &tok, d.clone()).unwrap();
        assert_eq!(req.action, Action::Optimize { dry_run: false });
        assert_eq!(err(&tok, &d), PLAN_UNKNOWN);
        // Expired.
        let old = std::time::Instant::now().checked_sub(PLAN_TTL).unwrap();
        let tok = super::lock(&plans).issue(&d, &plan, old).unwrap();
        assert_eq!(err(&tok, &d), PLAN_EXPIRED);
        // Another drive behind the same id (swapped between plan and confirm).
        let tok = super::lock(&plans).issue(&d, &plan, std::time::Instant::now()).unwrap();
        let mut swapped = d.clone();
        swapped.identity.key = format!("{}X", swapped.identity.key);
        assert_eq!(err(&tok, &swapped), DRIVE_CHANGED);
        assert_eq!(err(&tok, &d), PLAN_UNKNOWN, "a refused token is burnt");
        // An untrusted drive is refused before the token is even looked at.
        let tok = super::lock(&plans).issue(&d, &plan, std::time::Instant::now()).unwrap();
        let untrusted = Drive { trusted: false, ..d.clone() };
        assert!(super::optimize_request(&plans, &tok, untrusted).is_err());
    }

    use super::*;

    #[test]
    fn args_keep_existing_absolute_paths_and_skip_flags() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let file_s = file.to_string_lossy().into_owned();
        let missing = dir.path().join("missing").to_string_lossy().into_owned();

        let got = split_args(vec!["--tray".to_string(), file_s.clone(), missing, String::new()], Path::new("/"));
        assert_eq!(got, Argv { files: vec![file_s], pull: None });
    }

    #[test]
    fn relative_args_resolve_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Photos")).unwrap();
        let got = split_args(vec!["Photos".to_string()], dir.path()).files;
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with("Photos"));
        assert!(Path::new(&got[0]).is_absolute());
    }

    #[test]
    fn pull_value_is_not_a_file_to_copy() {
        let dir = tempfile::tempdir().unwrap();
        let stick = dir.path().join("STICK");
        std::fs::create_dir(&stick).unwrap();
        let stick_s = stick.to_string_lossy().into_owned();
        let got = split_args(vec!["--pull".to_string(), stick_s.clone()], Path::new("/"));
        assert_eq!(got, Argv { files: Vec::new(), pull: Some(stick_s) });
    }

    #[test]
    fn relative_pull_path_resolves_against_cwd() {
        let got = split_args(vec!["--pull".to_string(), "Photos/2026".to_string()], Path::new("/media/u"));
        assert_eq!(got.pull.as_deref(), Some("/media/u/Photos/2026"));
        assert!(got.files.is_empty());
        assert_eq!(split_args(vec!["--pull".to_string()], Path::new("/")).pull, None);
    }
}
