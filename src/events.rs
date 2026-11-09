//! Progress events for `build`, `add` and `pull`, and their JSON-lines encoding (`--json`).
//!
//! The JSON contract, one object per line on stdout:
//!
//! ```text
//! {"event":"plan","op":"add"|"build"|"pull"|"optimize","files":N,"dirs":N,"bytes":N,"write_bytes":N}
//!     pull adds "strategy":{"transport":"uas"|"usb-storage"|..,"gap_kb":N,"inflight":N,
//!     "direct":B,"overhead_ms":F|null,"mbps":F|null,"chunk_kb":N,"tail":N}
//! {"event":"phase","op":"optimize","phase":"pull"|..,"step":N,"steps":N}
//!     optimize only: a new phase starts; progress events that follow count within it
//! {"event":"progress","files_done":N,"bytes_done":N}   (at most ~10 per second)
//! {"event":"done","files":N,"bytes":N,"secs":F}
//! {"event":"error","message":"..."}                     (and a non-zero exit code)
//! ```

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Build,
    Add,
    Pull,
    Optimize,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Build => "build",
            Op::Add => "add",
            Op::Pull => "pull",
            Op::Optimize => "optimize",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Emitted once, before anything is written. `files`/`dirs` count what will be created,
    /// `bytes` is file data, `write_bytes` is what will physically be written to the target
    /// (for `pull`: read from the volume, gaps and alignment included).
    /// `detail` is a human-readable summary (not part of the JSON).
    Plan {
        op: Op,
        files: u64,
        dirs: u64,
        bytes: u64,
        write_bytes: u64,
        detail: String,
        /// `pull` only: how the volume will be read, as a JSON object (transport, gap, depth).
        strategy: Option<String>,
    },
    /// A multi-phase operation (`optimize`) enters phase `step` of `steps` (1-based).
    Phase {
        op: Op,
        phase: &'static str,
        step: u32,
        steps: u32,
    },
    /// Throttled to at most ~10 per second, plus one final event at 100 %.
    Progress {
        files_done: u64,
        bytes_done: u64,
    },
    Done {
        files: u64,
        bytes: u64,
        secs: f64,
    },
}

/// JSON string literal (with quotes) for `s`.
pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_f64(v: f64) -> String {
    if v.is_finite() { format!("{v:.3}") } else { "0".into() }
}

impl Event {
    pub fn to_json(&self) -> String {
        match self {
            Event::Plan { op, files, dirs, bytes, write_bytes, strategy, .. } => format!(
                r#"{{"event":"plan","op":"{}","files":{files},"dirs":{dirs},"bytes":{bytes},"write_bytes":{write_bytes}{}}}"#,
                op.as_str(),
                strategy.as_ref().map(|s| format!(r#","strategy":{s}"#)).unwrap_or_default()
            ),
            Event::Phase { op, phase, step, steps } => format!(
                r#"{{"event":"phase","op":"{}","phase":{},"step":{step},"steps":{steps}}}"#,
                op.as_str(),
                json_str(phase)
            ),
            Event::Progress { files_done, bytes_done } => {
                format!(r#"{{"event":"progress","files_done":{files_done},"bytes_done":{bytes_done}}}"#)
            }
            Event::Done { files, bytes, secs } => {
                format!(r#"{{"event":"done","files":{files},"bytes":{bytes},"secs":{}}}"#, json_f64(*secs))
            }
        }
    }
}

/// The JSON error event. `recovery` (only `optimize` sets it, once the drive may be
/// half-written) says where the data is and how to finish.
pub fn error_json(message: &str, recovery: Option<&str>) -> String {
    match recovery {
        Some(r) => format!(r#"{{"event":"error","message":{},"recovery":{}}}"#, json_str(message), json_str(r)),
        None => format!(r#"{{"event":"error","message":{}}}"#, json_str(message)),
    }
}

/// Callback type the library calls with each event.
pub type Sink<'a> = &'a mut dyn FnMut(&Event);

/// Rate-limits progress events to one per `every`.
pub(crate) struct Ticker<'a> {
    sink: Sink<'a>,
    last: Option<Instant>,
    every: Duration,
}

impl<'a> Ticker<'a> {
    pub(crate) fn new(sink: Sink<'a>) -> Self {
        Ticker { sink, last: None, every: Duration::from_millis(100) }
    }
    /// The underlying sink, for handing the whole operation to another one (add → build).
    pub(crate) fn sink(&mut self) -> Sink<'_> {
        &mut *self.sink
    }
    pub(crate) fn emit(&mut self, e: Event) {
        (self.sink)(&e);
    }
    pub(crate) fn progress(&mut self, files_done: u64, bytes_done: u64, force: bool) {
        let now = Instant::now();
        if force || self.last.is_none_or(|t| now.duration_since(t) >= self.every) {
            self.last = Some(now);
            (self.sink)(&Event::Progress { files_done, bytes_done });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_match_contract() {
        let p = Event::Plan {
            op: Op::Add,
            files: 3,
            dirs: 1,
            bytes: 10,
            write_bytes: 4096,
            detail: "x".into(),
            strategy: None,
        };
        assert_eq!(p.to_json(), r#"{"event":"plan","op":"add","files":3,"dirs":1,"bytes":10,"write_bytes":4096}"#);
        let g = Event::Progress { files_done: 1, bytes_done: 2 };
        assert_eq!(g.to_json(), r#"{"event":"progress","files_done":1,"bytes_done":2}"#);
        let ph = Event::Phase { op: Op::Optimize, phase: "verify", step: 2, steps: 5 };
        assert_eq!(ph.to_json(), r#"{"event":"phase","op":"optimize","phase":"verify","step":2,"steps":5}"#);
        let d = Event::Done { files: 3, bytes: 10, secs: 1.5 };
        assert_eq!(d.to_json(), r#"{"event":"done","files":3,"bytes":10,"secs":1.500}"#);
        assert_eq!(error_json("a \"b\"\n\\", None), r#"{"event":"error","message":"a \"b\"\n\\"}"#);
        assert_eq!(
            error_json("failed", Some("re-run \"it\"")),
            r#"{"event":"error","message":"failed","recovery":"re-run \"it\""}"#
        );
        assert_eq!(json_str("\u{1}é"), "\"\\u0001é\"");
    }

    #[test]
    fn ticker_throttles_but_forces() {
        let mut n = 0;
        let mut sink = |_: &Event| n += 1;
        let mut t = Ticker::new(&mut sink);
        for i in 0..1000 {
            t.progress(i, i, false);
        }
        t.progress(1000, 1000, true);
        let _ = t;
        assert_eq!(n, 2);
    }
}
