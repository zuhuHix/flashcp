//! flashcp's JSON-lines events: a strict per-process validator, and the aggregated job state the GUI shows.

use crate::pull::is_collision;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::ExitStatus;

/// A validated event from flashcp's stdout. Only the fields the GUI uses are kept.
#[derive(Debug, Clone, PartialEq)]
pub enum FlashcpEvent {
    Plan { files: u64, bytes: u64 },
    Progress { files_done: u64, bytes_done: u64 },
    Done { files: u64, bytes: u64, secs: f64 },
}

#[derive(Deserialize)]
struct PlanLine {
    files: u64,
    bytes: u64,
}

#[derive(Deserialize)]
struct ProgressLine {
    files_done: u64,
    bytes_done: u64,
}

#[derive(Deserialize)]
struct DoneLine {
    files: u64,
    bytes: u64,
    secs: f64,
}

#[derive(Deserialize)]
struct ErrorLine {
    message: String,
}

const SNIPPET_CHARS: usize = 120;

fn snippet(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn unreadable(text: &str) -> String {
    format!("flashcp sent unreadable output: {}", snippet(text, SNIPPET_CHARS))
}

fn typed<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|_| unreadable(text))
}

/// Checks one flashcp process's stdout, strictly. Per item the stream must be plan, progress*, done.
/// The process must exit 0 after exactly `expected` dones. Any violation is an error, and the job
/// fails with its message. Empty lines are ignored; extra fields are tolerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunValidator {
    expected: usize,
    dones: usize,
    in_run: bool,
}

impl RunValidator {
    pub fn new(expected: usize) -> Self {
        RunValidator { expected, dones: 0, in_run: false }
    }

    /// Items this process has finished so far.
    pub fn dones(&self) -> usize {
        self.dones
    }

    /// Checks one stdout line. Valid plan, progress and done lines come back as events.
    pub fn feed(&mut self, line: &str) -> Result<Option<FlashcpEvent>, String> {
        let text = line.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(text).map_err(|_| unreadable(text))?;
        let name = value.get("event").and_then(Value::as_str).ok_or_else(|| unreadable(text))?;
        match name {
            "plan" => {
                let p: PlanLine = typed(text)?;
                if self.in_run {
                    return Err("flashcp started an item before the previous one was done".to_string());
                }
                self.in_run = true;
                Ok(Some(FlashcpEvent::Plan { files: p.files, bytes: p.bytes }))
            }
            "progress" => {
                if !self.in_run {
                    return Err("flashcp sent progress before a plan".to_string());
                }
                let p: ProgressLine = typed(text)?;
                Ok(Some(FlashcpEvent::Progress { files_done: p.files_done, bytes_done: p.bytes_done }))
            }
            "done" => {
                if !self.in_run {
                    return Err("flashcp sent done before a plan".to_string());
                }
                if self.dones >= self.expected {
                    return Err(format!("flashcp finished more items than expected ({})", self.expected));
                }
                let d: DoneLine = typed(text)?;
                self.in_run = false;
                self.dones += 1;
                Ok(Some(FlashcpEvent::Done { files: d.files, bytes: d.bytes, secs: d.secs }))
            }
            "error" => {
                let e: ErrorLine = typed(text)?;
                if e.message.is_empty() {
                    Err("flashcp reported an error".to_string())
                } else {
                    Err(e.message)
                }
            }
            other => Err(format!("flashcp sent an unknown event: {}", snippet(other, 40))),
        }
    }

    /// Called at end of stdout with the process's exit status. Non-zero exit fails even when every
    /// done arrived; a clean exit still fails when fewer items finished or an item was left open.
    pub fn finish(&self, status: ExitStatus) -> Result<(), String> {
        match status.code() {
            Some(0) => {}
            Some(code) => return Err(format!("flashcp exited with status {code}")),
            None => return Err("flashcp was killed by a signal".to_string()),
        }
        if self.in_run || self.dones < self.expected {
            return Err(format!("flashcp stopped before finishing ({} of {} items done)", self.dones, self.expected));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Unmounting,
    Authorizing,
    Running,
    Remounting,
    Done,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Add,
    Build,
    Pull,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub files: u64,
    pub bytes: u64,
    pub secs: f64,
    pub wall_secs: f64,
    pub mount_point: Option<String>,
    /// Pull: the local destination folder. None for add and build.
    pub dest: Option<String>,
    /// Pull: set when the copy succeeded but the drive could not be remounted.
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobUpdate {
    pub stage: Stage,
    pub op: Op,
    pub index: usize,
    pub total: usize,
    pub files_done: u64,
    pub bytes_done: u64,
    pub plan_files: u64,
    pub plan_bytes: u64,
    pub message: Option<String>,
    /// True only on an error caused by flashcp's collision message (see `pull::is_collision`).
    pub conflict: bool,
    pub summary: Option<JobSummary>,
}

/// Folds validated events from the job's flashcp processes into job totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobAggregate {
    finished_files: u64,
    finished_bytes: u64,
    secs: f64,
    run_files: u64,
    run_bytes: u64,
    plan_files: u64,
    plan_bytes: u64,
}

impl JobAggregate {
    /// Call before each process starts; resets the current process's progress.
    pub fn start_run(&mut self) {
        self.run_files = 0;
        self.run_bytes = 0;
    }

    pub fn apply(&mut self, event: &FlashcpEvent) {
        match event {
            FlashcpEvent::Plan { files, bytes } => {
                self.plan_files += files;
                self.plan_bytes += bytes;
            }
            FlashcpEvent::Progress { files_done, bytes_done } => {
                self.run_files = *files_done;
                self.run_bytes = *bytes_done;
            }
            FlashcpEvent::Done { files, bytes, secs } => {
                self.finished_files += files;
                self.finished_bytes += bytes;
                self.secs += secs;
                self.run_files = 0;
                self.run_bytes = 0;
            }
        }
    }

    pub fn files_done(&self) -> u64 {
        self.finished_files + self.run_files
    }

    pub fn bytes_done(&self) -> u64 {
        self.finished_bytes + self.run_bytes
    }

    /// Totals of completed items only (what the summary reports).
    pub fn finished_files(&self) -> u64 {
        self.finished_files
    }

    pub fn finished_bytes(&self) -> u64 {
        self.finished_bytes
    }

    pub fn secs(&self) -> f64 {
        self.secs
    }

    /// Builds a JobUpdate from the current totals, without a summary.
    pub fn update(&self, stage: Stage, op: Op, index: usize, total: usize, message: Option<String>) -> JobUpdate {
        JobUpdate {
            stage,
            op,
            index,
            total,
            files_done: self.files_done(),
            bytes_done: self.bytes_done(),
            plan_files: self.plan_files,
            plan_bytes: self.plan_bytes,
            conflict: stage == Stage::Error && message.as_deref().is_some_and(is_collision),
            message,
            summary: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn ok() -> ExitStatus {
        ExitStatus::from_raw(0)
    }

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    fn plan() -> &'static str {
        r#"{"event":"plan","op":"add","files":100,"dirs":5,"bytes":50000000,"write_bytes":52000000}"#
    }

    fn progress() -> &'static str {
        r#"{"event":"progress","files_done":10,"bytes_done":5000000}"#
    }

    fn done() -> &'static str {
        r#"{"event":"done","files":100,"bytes":50000000,"secs":0.5}"#
    }

    /// Feeds whole items (plan, progress, done) and returns the validator.
    fn feed_items(v: &mut RunValidator, items: usize) {
        for _ in 0..items {
            assert!(matches!(v.feed(plan()), Ok(Some(FlashcpEvent::Plan { .. }))));
            assert!(matches!(v.feed(progress()), Ok(Some(FlashcpEvent::Progress { .. }))));
            assert!(matches!(v.feed(done()), Ok(Some(FlashcpEvent::Done { .. }))));
        }
    }

    fn err(v: &mut RunValidator, line: &str) -> String {
        v.feed(line).expect_err("line must be rejected")
    }

    #[test]
    fn parses_plan_progress_done() {
        let mut v = RunValidator::new(1);
        assert_eq!(v.feed(plan()), Ok(Some(FlashcpEvent::Plan { files: 100, bytes: 50_000_000 })));
        assert_eq!(v.feed(progress()), Ok(Some(FlashcpEvent::Progress { files_done: 10, bytes_done: 5_000_000 })));
        assert_eq!(v.feed(done()), Ok(Some(FlashcpEvent::Done { files: 100, bytes: 50_000_000, secs: 0.5 })));
    }

    #[test]
    fn happy_path_one_run() {
        let mut v = RunValidator::new(1);
        feed_items(&mut v, 1);
        assert_eq!(v.dones(), 1);
        assert_eq!(v.finish(ok()), Ok(()));
    }

    #[test]
    fn happy_path_three_runs() {
        let mut v = RunValidator::new(3);
        feed_items(&mut v, 3);
        assert_eq!(v.dones(), 3);
        assert_eq!(v.finish(ok()), Ok(()));
    }

    #[test]
    fn empty_lines_are_ignored() {
        let mut v = RunValidator::new(1);
        assert_eq!(v.feed(""), Ok(None));
        assert_eq!(v.feed("   \t "), Ok(None));
        assert!(matches!(v.feed(plan()), Ok(Some(_))));
        assert_eq!(v.feed(""), Ok(None));
        assert!(matches!(v.feed(done()), Ok(Some(FlashcpEvent::Done { .. }))));
    }

    #[test]
    fn extra_fields_are_tolerated() {
        let mut v = RunValidator::new(1);
        let line = r#"{"event":"progress","files_done":1,"bytes_done":2,"speed":9.5,"eta":3}"#;
        assert_eq!(v.feed(plan()), Ok(Some(FlashcpEvent::Plan { files: 100, bytes: 50_000_000 })));
        assert_eq!(v.feed(line), Ok(Some(FlashcpEvent::Progress { files_done: 1, bytes_done: 2 })));
        let line = r#"{"event":"done","files":1,"bytes":2,"secs":1,"rate":4,"extra":{"a":1}}"#;
        assert_eq!(v.feed(line), Ok(Some(FlashcpEvent::Done { files: 1, bytes: 2, secs: 1.0 })));
        assert_eq!(v.finish(ok()), Ok(()));
    }

    #[test]
    fn non_json_line_is_unreadable() {
        let mut v = RunValidator::new(1);
        assert_eq!(err(&mut v, "Copying file 3 of 10..."), "flashcp sent unreadable output: Copying file 3 of 10...");
        assert_eq!(err(&mut v, "{not json"), "flashcp sent unreadable output: {not json");
    }

    #[test]
    fn unreadable_snippet_is_cut_at_120_chars() {
        let mut v = RunValidator::new(1);
        let long = "x".repeat(300);
        let message = err(&mut v, &long);
        assert_eq!(message, format!("flashcp sent unreadable output: {}", "x".repeat(120)));
    }

    #[test]
    fn json_without_event_is_unreadable() {
        let mut v = RunValidator::new(1);
        assert_eq!(
            err(&mut v, r#"{"files_done":3}"#),
            format!("flashcp sent unreadable output: {}", r#"{"files_done":3}"#)
        );
        assert!(err(&mut v, "[1,2,3]").starts_with("flashcp sent unreadable output"));
        assert!(err(&mut v, "42").starts_with("flashcp sent unreadable output"));
        assert!(err(&mut v, r#"{"event":7}"#).starts_with("flashcp sent unreadable output"));
    }

    #[test]
    fn malformed_event_fields_are_unreadable() {
        let mut v = RunValidator::new(1);
        assert!(err(&mut v, r#"{"event":"plan"}"#).starts_with("flashcp sent unreadable output"));
        assert_eq!(v.feed(plan()), Ok(Some(FlashcpEvent::Plan { files: 100, bytes: 50_000_000 })));
        assert!(err(&mut v, r#"{"event":"progress","files_done":"ten"}"#).starts_with("flashcp sent unreadable"));
        assert!(err(&mut v, r#"{"event":"done","files":1}"#).starts_with("flashcp sent unreadable"));
    }

    #[test]
    fn unknown_event_fails() {
        let mut v = RunValidator::new(1);
        assert_eq!(err(&mut v, r#"{"event":"mystery","x":1}"#), "flashcp sent an unknown event: mystery");
    }

    #[test]
    fn error_event_gives_its_message() {
        let mut v = RunValidator::new(1);
        assert_eq!(v.feed(plan()), Ok(Some(FlashcpEvent::Plan { files: 100, bytes: 50_000_000 })));
        assert_eq!(
            err(&mut v, r#"{"event":"error","message":"fake failure: device busy"}"#),
            "fake failure: device busy"
        );
    }

    #[test]
    fn error_event_without_message_still_fails() {
        let mut v = RunValidator::new(1);
        assert_eq!(err(&mut v, r#"{"event":"error","message":""}"#), "flashcp reported an error");
    }

    #[test]
    fn progress_before_plan_fails() {
        let mut v = RunValidator::new(1);
        assert_eq!(err(&mut v, progress()), "flashcp sent progress before a plan");
    }

    #[test]
    fn done_before_plan_fails() {
        let mut v = RunValidator::new(1);
        assert_eq!(err(&mut v, done()), "flashcp sent done before a plan");
        // and after a finished item, until the next plan
        feed_items(&mut v, 1);
        assert_eq!(err(&mut v, done()), "flashcp sent done before a plan");
    }

    #[test]
    fn second_plan_before_done_fails() {
        let mut v = RunValidator::new(2);
        assert!(matches!(v.feed(plan()), Ok(Some(_))));
        assert_eq!(err(&mut v, plan()), "flashcp started an item before the previous one was done");
    }

    #[test]
    fn more_dones_than_expected_fails() {
        let mut v = RunValidator::new(1);
        feed_items(&mut v, 1);
        assert!(matches!(v.feed(plan()), Ok(Some(_))));
        assert_eq!(err(&mut v, done()), "flashcp finished more items than expected (1)");
    }

    #[test]
    fn fewer_dones_at_eof_fails() {
        let mut v = RunValidator::new(3);
        feed_items(&mut v, 1);
        assert_eq!(v.finish(ok()), Err("flashcp stopped before finishing (1 of 3 items done)".to_string()));
    }

    #[test]
    fn stopping_inside_an_item_fails() {
        let mut v = RunValidator::new(1);
        assert!(matches!(v.feed(plan()), Ok(Some(_))));
        assert!(matches!(v.feed(progress()), Ok(Some(_))));
        assert_eq!(v.finish(ok()), Err("flashcp stopped before finishing (0 of 1 items done)".to_string()));
    }

    #[test]
    fn non_zero_exit_fails_even_with_all_dones() {
        let mut v = RunValidator::new(1);
        feed_items(&mut v, 1);
        assert_eq!(v.finish(exit(2)), Err("flashcp exited with status 2".to_string()));
    }

    #[test]
    fn signal_death_fails() {
        let mut v = RunValidator::new(1);
        feed_items(&mut v, 1);
        assert_eq!(v.finish(ExitStatus::from_raw(9)), Err("flashcp was killed by a signal".to_string()));
    }

    #[test]
    fn aggregates_across_runs() {
        let mut agg = JobAggregate::default();
        agg.start_run();
        agg.apply(&FlashcpEvent::Plan { files: 100, bytes: 50_000_000 });
        agg.apply(&FlashcpEvent::Progress { files_done: 40, bytes_done: 20_000_000 });
        assert_eq!(agg.files_done(), 40);
        assert_eq!(agg.bytes_done(), 20_000_000);

        agg.apply(&FlashcpEvent::Done { files: 100, bytes: 50_000_000, secs: 0.5 });
        agg.start_run();
        agg.apply(&FlashcpEvent::Plan { files: 100, bytes: 50_000_000 });
        agg.apply(&FlashcpEvent::Progress { files_done: 10, bytes_done: 5_000_000 });

        // completed run's totals plus the current run's progress
        assert_eq!(agg.files_done(), 110);
        assert_eq!(agg.bytes_done(), 55_000_000);
        assert_eq!(agg.plan_files, 200);
        assert_eq!(agg.plan_bytes, 100_000_000);

        agg.apply(&FlashcpEvent::Done { files: 100, bytes: 50_000_000, secs: 0.25 });
        assert_eq!(agg.finished_files(), 200);
        assert_eq!(agg.finished_bytes(), 100_000_000);
        assert_eq!(agg.secs(), 0.75);
        assert_eq!(agg.files_done(), 200);
    }

    #[test]
    fn update_carries_totals_without_summary() {
        let mut agg = JobAggregate::default();
        agg.start_run();
        agg.apply(&FlashcpEvent::Plan { files: 3, bytes: 30 });
        agg.apply(&FlashcpEvent::Progress { files_done: 1, bytes_done: 10 });
        let u = agg.update(Stage::Running, Op::Add, 0, 2, None);
        assert_eq!(u.files_done, 1);
        assert_eq!(u.bytes_done, 10);
        assert_eq!(u.plan_files, 3);
        assert_eq!(u.plan_bytes, 30);
        assert_eq!(u.total, 2);
        assert!(u.summary.is_none());
    }

    #[test]
    fn pull_plan_with_nested_strategy_is_accepted() {
        let mut v = RunValidator::new(1);
        let plan = r#"{"event":"plan","op":"pull","files":3,"dirs":1,"bytes":30,"strategy":{"transport":"uas","gap_kb":32,"inflight":32,"buffered":false}}"#;
        assert_eq!(v.feed(plan), Ok(Some(FlashcpEvent::Plan { files: 3, bytes: 30 })));
        assert_eq!(v.feed(done()), Ok(Some(FlashcpEvent::Done { files: 100, bytes: 50_000_000, secs: 0.5 })));
        assert_eq!(v.finish(ok()), Ok(()));
    }

    #[test]
    fn pull_with_two_dones_fails() {
        let mut v = RunValidator::new(1);
        let plan = r#"{"event":"plan","op":"pull","files":3,"dirs":1,"bytes":30,"strategy":{"transport":"uas"}}"#;
        assert!(matches!(v.feed(plan), Ok(Some(_))));
        assert!(matches!(v.feed(done()), Ok(Some(_))));
        assert!(matches!(v.feed(plan), Ok(Some(_))));
        assert_eq!(err(&mut v, done()), "flashcp finished more items than expected (1)");
    }

    #[test]
    fn only_collision_errors_set_the_conflict_flag() {
        let agg = JobAggregate::default();
        let collision = "2 file(s) already exist in the destination /d, e.g. a, b; pass --skip-existing or --overwrite";
        let u = agg.update(Stage::Error, Op::Pull, 0, 1, Some(collision.to_string()));
        assert!(u.conflict);
        assert_eq!(u.op, Op::Pull);
        assert!(!agg.update(Stage::Error, Op::Pull, 0, 1, Some("Unmount failed".into())).conflict);
        assert!(!agg.update(Stage::Running, Op::Pull, 0, 1, Some(collision.into())).conflict);
        assert!(!agg.update(Stage::Error, Op::Add, 0, 1, None).conflict);
    }

    #[test]
    fn pull_op_serializes_as_pull() {
        assert_eq!(serde_json::to_string(&Op::Pull).unwrap(), "\"pull\"");
    }
}
