//! Progress events for `build`, `add` and `pull`, and their JSON-lines encoding (`--json`).
//!
//! The JSON contract, one object per line on stdout:
//!
//! ```text
//! {"event":"plan","op":"add"|"build"|"pull","files":N,"dirs":N,"bytes":N,"write_bytes":N}
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
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Build => "build",
            Op::Add => "add",
            Op::Pull => "pull",
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
            Event::Plan { op, files, dirs, bytes, write_bytes, .. } => format!(
                r#"{{"event":"plan","op":"{}","files":{files},"dirs":{dirs},"bytes":{bytes},"write_bytes":{write_bytes}}}"#,
                op.as_str()
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

pub fn error_json(message: &str) -> String {
    format!(r#"{{"event":"error","message":{}}}"#, json_str(message))
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
        let p = Event::Plan { op: Op::Add, files: 3, dirs: 1, bytes: 10, write_bytes: 4096, detail: "x".into() };
        assert_eq!(p.to_json(), r#"{"event":"plan","op":"add","files":3,"dirs":1,"bytes":10,"write_bytes":4096}"#);
        let g = Event::Progress { files_done: 1, bytes_done: 2 };
        assert_eq!(g.to_json(), r#"{"event":"progress","files_done":1,"bytes_done":2}"#);
        let d = Event::Done { files: 3, bytes: 10, secs: 1.5 };
        assert_eq!(d.to_json(), r#"{"event":"done","files":3,"bytes":10,"secs":1.500}"#);
        assert_eq!(error_json("a \"b\"\n\\"), r#"{"event":"error","message":"a \"b\"\n\\"}"#);
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
