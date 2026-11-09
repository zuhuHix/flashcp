//! One copy, erase, pull or optimize job: stage loose files, unmount, run flashcp, remount, open the folder.
//! Emits "job" events. Only one job runs at a time (JobLock).

use crate::drives::Drive;
use crate::events::{JobAggregate, JobSummary, JobUpdate, Op, Stage};
use crate::optimize;
use crate::runner::{self, Action, Invocation, JobFailure, StageDir};
use crate::udisks;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use tauri::{AppHandle, Emitter};

/// Cluster size reported after an add in fake mode, so the UI hint can be tried without a stick.
const FAKE_CLUSTER_BYTES: u64 = 32768;

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
    if let Action::Pull { .. } = req.action {
        return run_pull(app, req);
    }
    if let Action::Optimize { .. } = req.action {
        return run_optimize(app, req);
    }
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
            match remount(&req.drive, fake) {
                Ok(mount_point) => {
                    if let Some(p) = mount_point.as_deref().filter(|_| !fake) {
                        open_folder(p);
                    }
                    let cluster_bytes = match req.op {
                        Op::Add => cluster_after_add(mount_point.as_deref(), fake),
                        _ => None,
                    };
                    let mut update = agg.update(Stage::Done, req.op, last_index(req), req.total, None);
                    update.summary = Some(JobSummary {
                        files: agg.finished_files(),
                        bytes: agg.finished_bytes(),
                        secs: agg.secs(),
                        wall_secs: started.elapsed().as_secs_f64(),
                        mount_point,
                        dest: None,
                        warning: None,
                        cluster_bytes,
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
        Err(failure) => {
            let mut message = failure.message;
            if was_mounted && failure.recovery.is_none() {
                emit(app, plain(req, Stage::Remounting, last_index(req), None));
                if let Err(e) = remount(&req.drive, fake) {
                    message = format!("{message} (remounting also failed: {e})");
                }
            }
            let mut update = plain(req, Stage::Error, last_index(req), Some(message));
            update.recovery = failure.recovery;
            emit(app, update);
        }
    }
}

/// The exFAT cluster size of the stick after an add (None when it cannot be read).
fn cluster_after_add(mount_point: Option<&str>, fake: bool) -> Option<u64> {
    if fake {
        return Some(FAKE_CLUSTER_BYTES);
    }
    mount_point.and_then(|p| optimize::exfat_cluster(Path::new(p)))
}

/// Optimize: unmount if mounted, rewrite the volume with flashcp, remount. A failure that may have
/// left the stick half-written carries recovery text and is not remounted: the data is in the
/// temporary copy, and the user finishes by running optimize again. No folder is opened.
fn run_optimize(app: &AppHandle, req: &Request) {
    let started = Instant::now();
    let fake = udisks::fake_mode();
    let was_mounted = !fake && req.drive.mount_point.is_some();

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
            match remount(&req.drive, fake) {
                Ok(mount_point) => {
                    let mut update = agg.update(Stage::Done, req.op, last_index(req), req.total, None);
                    update.summary = Some(JobSummary {
                        files: agg.finished_files(),
                        bytes: agg.finished_bytes(),
                        secs: agg.secs(),
                        wall_secs: started.elapsed().as_secs_f64(),
                        mount_point,
                        dest: None,
                        warning: None,
                        cluster_bytes: None,
                    });
                    emit(app, update);
                }
                Err(e) => emit(
                    app,
                    plain(
                        req,
                        Stage::Error,
                        last_index(req),
                        Some(format!("Optimize finished, but the drive could not be remounted: {e}")),
                    ),
                ),
            }
        }
        Err(failure) => {
            let mut message = failure.message;
            if was_mounted && failure.recovery.is_none() {
                emit(app, plain(req, Stage::Remounting, last_index(req), None));
                if let Err(e) = remount(&req.drive, fake) {
                    message = format!("{message} (remounting also failed: {e})");
                }
            }
            let mut update = plain(req, Stage::Error, last_index(req), Some(message));
            update.recovery = failure.recovery;
            emit(app, update);
        }
    }
}

/// Pull: unmount if mounted, copy off the drive, remount if it was mounted, open the destination.
/// A copy that succeeded is Done even when the remount failed (the summary carries a warning).
fn run_pull(app: &AppHandle, req: &Request) {
    let Action::Pull { dest, .. } = &req.action else { return };
    let started = Instant::now();
    let fake = udisks::fake_mode();
    let was_mounted = !fake && req.drive.mount_point.is_some();

    if was_mounted {
        emit(app, plain(req, Stage::Unmounting, 0, None));
        if let Err(e) = udisks::unmount(&req.drive.id) {
            emit(app, plain(req, Stage::Error, 0, Some(format!("Could not unmount {}: {e}", req.drive.device))));
            return;
        }
    }

    let outcome = run_flashcp(app, req).map_err(|f| f.message);
    let remounted = if was_mounted {
        let stage_update = match &outcome {
            Ok(agg) => agg.update(Stage::Remounting, req.op, last_index(req), req.total, None),
            Err(_) => plain(req, Stage::Remounting, last_index(req), None),
        };
        emit(app, stage_update);
        Some(remount(&req.drive, fake))
    } else {
        None
    };

    match outcome {
        Ok(agg) => {
            let (mount_point, warning) = match remounted {
                Some(Err(e)) => (None, Some(format!("Copied, but the drive could not be remounted: {e}"))),
                Some(Ok(point)) => (point, None),
                None => (None, None),
            };
            if !fake {
                open_folder(dest);
            }
            let mut update = agg.update(Stage::Done, req.op, last_index(req), req.total, None);
            update.summary = Some(JobSummary {
                files: agg.finished_files(),
                bytes: agg.finished_bytes(),
                secs: agg.secs(),
                wall_secs: started.elapsed().as_secs_f64(),
                mount_point,
                dest: Some(dest.clone()),
                warning,
                cluster_bytes: None,
            });
            emit(app, update);
        }
        Err(message) => {
            let message = match remounted {
                Some(Err(e)) => format!("{message} (remounting also failed: {e})"),
                _ => message,
            };
            emit(app, plain(req, Stage::Error, last_index(req), Some(message)));
        }
    }
}

fn run_flashcp(app: &AppHandle, req: &Request) -> Result<JobAggregate, JobFailure> {
    let inv = Invocation::from_env();
    let runs = inv.runs(&req.drive.device, &req.drive.serial, &req.action);
    runner::run_job(inv.program(), &runs, req.op, &[], |u| emit(app, u))
}

/// Rescan and mount (or, in fake mode, report the fake mount point).
pub fn remount(drive: &Drive, fake: bool) -> Result<Option<String>, String> {
    if fake {
        return Ok(drive.mount_point.clone());
    }
    if let Err(e) = udisks::rescan(&drive.id) {
        eprintln!("flashcp-gui: rescan of {} failed: {e}", drive.device);
    }
    udisks::mount(&drive.id).map(Some)
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
