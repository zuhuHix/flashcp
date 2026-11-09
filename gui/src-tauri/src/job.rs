//! One copy or erase job: stage loose files, unmount, run flashcp, remount, open the folder.
//! Emits "job" events. Only one job runs at a time (JobLock).

use crate::drives::Drive;
use crate::events::{JobAggregate, JobSummary, JobUpdate, Op, Stage};
use crate::runner::{self, Action, Invocation, StageDir};
use crate::udisks;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use tauri::{AppHandle, Emitter};

pub struct Request {
    pub op: Op,
    pub drive: Drive,
    pub action: Action,
    /// Number of picked sources, for "item i of N".
    pub total: usize,
    /// Where loose files are staged, and the (file, link) pairs to create there. Empty when none.
    pub stage_dir: PathBuf,
    pub staged: Vec<(PathBuf, PathBuf)>,
}

/// Holds the single-job flag; released on drop.
pub struct JobLock(Arc<AtomicBool>);

impl JobLock {
    pub fn acquire(flag: &Arc<AtomicBool>) -> Result<JobLock, String> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| JobLock(Arc::clone(flag)))
            .map_err(|_| "A job is already running".to_string())
    }
}

impl Drop for JobLock {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Runs the job on its own thread. The lock is held until the job finishes.
pub fn spawn(app: AppHandle, req: Request, lock: JobLock) -> Result<(), String> {
    thread::Builder::new()
        .name("flashcp-job".into())
        .spawn(move || {
            let _lock = lock;
            run(&app, &req);
        })
        .map(|_| ())
        .map_err(|e| format!("Cannot start job thread: {e}"))
}

fn emit(app: &AppHandle, update: JobUpdate) {
    let _ = app.emit("job", update);
}

fn plain(req: &Request, stage: Stage, index: usize, message: Option<String>) -> JobUpdate {
    JobAggregate::default().update(stage, req.op, index, req.total, message)
}

fn last_index(req: &Request) -> usize {
    req.total.saturating_sub(1)
}

fn run(app: &AppHandle, req: &Request) {
    let started = Instant::now();
    let fake = udisks::fake_mode();
    let was_mounted = !fake && req.drive.mount_point.is_some();

    // Stage loose files before touching the drive, so a staging failure leaves it as it was.
    // The staging folder is removed when `_staged` drops, whatever happens below.
    let _staged = match StageDir::create(&req.stage_dir, &req.staged) {
        Ok(staged) => staged,
        Err(e) => {
            emit(app, plain(req, Stage::Error, 0, Some(e)));
            return;
        }
    };

    if was_mounted {
        emit(app, plain(req, Stage::Unmounting, 0, None));
        if let Err(e) = udisks::unmount(&req.drive.id) {
            emit(app, plain(req, Stage::Error, 0, Some(format!("Could not unmount {}: {e}", req.drive.device))));
            return;
        }
    }

    match run_flashcp(app, req) {
        Ok(agg) => {
            emit(app, agg.update(Stage::Remounting, req.op, last_index(req), req.total, None));
            match remount(req, fake) {
                Ok(mount_point) => {
                    if let Some(p) = mount_point.as_deref().filter(|_| !fake) {
                        open_folder(p);
                    }
                    let mut update = agg.update(Stage::Done, req.op, last_index(req), req.total, None);
                    update.summary = Some(JobSummary {
                        files: agg.finished_files(),
                        bytes: agg.finished_bytes(),
                        secs: agg.secs(),
                        wall_secs: started.elapsed().as_secs_f64(),
                        mount_point,
                    });
                    emit(app, update);
                }
                Err(e) => emit(
                    app,
                    plain(
                        req,
                        Stage::Error,
                        last_index(req),
                        Some(format!("Copy finished, but the drive could not be remounted: {e}")),
                    ),
                ),
            }
        }
        Err(message) => {
            let mut message = message;
            if was_mounted {
                emit(app, plain(req, Stage::Remounting, last_index(req), None));
                if let Err(e) = remount(req, fake) {
                    message = format!("{message} (remounting also failed: {e})");
                }
            }
            emit(app, plain(req, Stage::Error, last_index(req), Some(message)));
        }
    }
}

fn run_flashcp(app: &AppHandle, req: &Request) -> Result<JobAggregate, String> {
    let inv = Invocation::from_env();
    let runs = inv.runs(&req.drive.device, &req.drive.serial, &req.action);
    runner::run_job(inv.program(), &runs, req.op, &[], |u| emit(app, u))
}

/// Rescan and mount (or, in fake mode, report the fake mount point).
fn remount(req: &Request, fake: bool) -> Result<Option<String>, String> {
    if fake {
        return Ok(req.drive.mount_point.clone());
    }
    if let Err(e) = udisks::rescan(&req.drive.id) {
        eprintln!("flashcp-gui: rescan of {} failed: {e}", req.drive.device);
    }
    udisks::mount(&req.drive.id).map(Some)
}

/// Opens the folder in the file manager. Not waited on.
fn open_folder(path: &str) {
    let path = path.to_string();
    thread::spawn(move || match Command::new("xdg-open").arg(&path).spawn() {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => eprintln!("flashcp-gui: cannot run xdg-open {path}: {e}"),
    });
}
