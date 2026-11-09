//! flashcp's JSON-lines events, and the aggregated job state the GUI shows.

use serde::{Deserialize, Serialize};

/// One stdout line from `flashcp ... --json`. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum FlashcpEvent {
    Plan {
        files: u64,
        bytes: u64,
    },
    Progress {
        files_done: u64,
        bytes_done: u64,
    },
    Done {
        files: u64,
        bytes: u64,
        secs: f64,
    },
    Error {
        message: String,
    },
    /// A valid event this GUI does not use.
    #[serde(other)]
    Other,
}

/// Parses one stdout line. Non-JSON lines and malformed events give `None`.
pub fn parse_line(line: &str) -> Option<FlashcpEvent> {
    serde_json::from_str(line.trim()).ok()
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
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub files: u64,
    pub bytes: u64,
    pub secs: f64,
    pub wall_secs: f64,
    pub mount_point: Option<String>,
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
    pub summary: Option<JobSummary>,
}

/// Folds events from several sequential flashcp runs (one per source) into job totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobAggregate {
    finished_files: u64,
    finished_bytes: u64,
    secs: f64,
    run_files: u64,
    run_bytes: u64,
    plan_files: u64,
    plan_bytes: u64,
    last_error: Option<String>,
}

impl JobAggregate {
    /// Call before each run starts; resets the current run's progress.
    pub fn start_run(&mut self) {
        self.run_files = 0;
        self.run_bytes = 0;
        self.last_error = None;
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
            FlashcpEvent::Error { message } => self.last_error = Some(message.clone()),
            FlashcpEvent::Other => {}
        }
    }

    pub fn files_done(&self) -> u64 {
        self.finished_files + self.run_files
    }

    pub fn bytes_done(&self) -> u64 {
        self.finished_bytes + self.run_bytes
    }

    /// Totals of completed runs only (what the summary reports).
    pub fn finished_files(&self) -> u64 {
        self.finished_files
    }

    pub fn finished_bytes(&self) -> u64 {
        self.finished_bytes
    }

    pub fn secs(&self) -> f64 {
        self.secs
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
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
            message,
            summary: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan() {
        let ev =
            parse_line(r#"{"event":"plan","op":"add","files":100,"dirs":5,"bytes":50000000,"write_bytes":52000000}"#);
        assert_eq!(ev, Some(FlashcpEvent::Plan { files: 100, bytes: 50_000_000 }));
    }

    #[test]
    fn parses_progress() {
        let ev = parse_line(r#"{"event":"progress","files_done":10,"bytes_done":5000000}"#);
        assert_eq!(ev, Some(FlashcpEvent::Progress { files_done: 10, bytes_done: 5_000_000 }));
    }

    #[test]
    fn parses_done_with_float_secs() {
        let ev = parse_line(r#"{"event":"done","files":100,"bytes":50000000,"secs":0.5}"#);
        assert_eq!(ev, Some(FlashcpEvent::Done { files: 100, bytes: 50_000_000, secs: 0.5 }));
    }

    #[test]
    fn parses_error() {
        let ev = parse_line(r#"{"event":"error","message":"fake failure: device busy"}"#);
        assert_eq!(ev, Some(FlashcpEvent::Error { message: "fake failure: device busy".into() }));
    }

    #[test]
    fn garbage_lines_are_none() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("Copying file 3 of 10..."), None);
        assert_eq!(parse_line("{not json"), None);
        assert_eq!(parse_line("[1,2,3]"), None);
        assert_eq!(parse_line(r#"{"event":"plan"}"#), None);
        assert_eq!(parse_line(r#"{"files_done":3}"#), None);
    }

    #[test]
    fn unknown_event_is_other() {
        assert_eq!(parse_line(r#"{"event":"mystery","x":1}"#), Some(FlashcpEvent::Other));
    }

    #[test]
    fn extra_fields_are_ignored() {
        let ev = parse_line(r#"{"event":"progress","files_done":1,"bytes_done":2,"speed":9.5}"#);
        assert_eq!(ev, Some(FlashcpEvent::Progress { files_done: 1, bytes_done: 2 }));
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
    fn error_is_recorded_and_reset_per_run() {
        let mut agg = JobAggregate::default();
        agg.start_run();
        agg.apply(&FlashcpEvent::Error { message: "device busy".into() });
        assert_eq!(agg.last_error(), Some("device busy"));
        agg.start_run();
        assert_eq!(agg.last_error(), None);
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
}
