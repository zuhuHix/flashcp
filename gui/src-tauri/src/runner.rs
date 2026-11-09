//! Builds helper command lines and runs them, turning flashcp's JSON events into JobUpdates.

use crate::events::{parse_line, FlashcpEvent, JobAggregate, JobUpdate, Op, Stage};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const HELPER_PATH: &str = "/usr/libexec/flashcp-helper";
const DIRECT_ENV: &str = "FLASHCP_GUI_DIRECT";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// A source path picked by the user, with whether it is a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: String,
    pub is_dir: bool,
}

impl Source {
    pub fn probe(path: &str) -> Result<Source, String> {
        if !Path::new(path).is_absolute() {
            return Err(format!("Not an absolute path: {path}"));
        }
        let meta = std::fs::metadata(path).map_err(|e| format!("Cannot read {path}: {e}"))?;
        Ok(Source { path: path.to_string(), is_dir: meta.is_dir() })
    }
}

/// Destination inside the volume, always "/" or "/a/b" with no empty or doubled segments.
pub fn normalize_dest(dest: &str) -> String {
    let parts: Vec<&str> = dest.split('/').filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

/// Destination for a directory source: the dest dir joined with the directory's basename.
fn dir_dest(dest: &str, src: &str) -> String {
    let name = Path::new(src).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let base = normalize_dest(dest);
    if name.is_empty() {
        base
    } else if base == "/" {
        format!("/{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// One helper argument list per source. `add` copies into `dest_dir` (directories get their
/// basename appended); `build` erases `device` and is only meaningful with one source.
pub fn build_args(
    op: Op,
    sources: &[Source],
    device: &str,
    dest_dir: &str,
    skip_existing: bool,
    label: &str,
) -> Vec<Vec<String>> {
    sources
        .iter()
        .map(|src| match op {
            Op::Add => {
                let dest = if src.is_dir { dir_dest(dest_dir, &src.path) } else { normalize_dest(dest_dir) };
                let mut args = vec!["add".to_string(), src.path.clone(), device.to_string(), "--to".to_string(), dest];
                if skip_existing {
                    args.push("--skip-existing".to_string());
                }
                args
            }
            Op::Build => {
                let mut args =
                    vec!["build".to_string(), src.path.clone(), device.to_string(), "--yes-erase".to_string()];
                if !label.is_empty() {
                    args.push("--label".to_string());
                    args.push(label.to_string());
                }
                args
            }
        })
        .collect()
}

/// How to launch the helper: through pkexec in production, or directly (the helper appends
/// `--json` itself, so only the direct path adds it here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub base_args: Vec<String>,
    pub append_json: bool,
}

impl Invocation {
    pub fn from_env() -> Self {
        Self::from_direct(std::env::var(DIRECT_ENV).ok())
    }

    pub fn from_direct(direct: Option<String>) -> Self {
        match direct.filter(|s| !s.is_empty()) {
            Some(path) => Invocation { program: path, base_args: Vec::new(), append_json: true },
            None => Invocation {
                program: "pkexec".to_string(),
                base_args: vec![HELPER_PATH.to_string()],
                append_json: false,
            },
        }
    }

    pub fn full_args(&self, helper_args: &[String]) -> Vec<String> {
        let mut v = self.base_args.clone();
        v.extend(helper_args.iter().cloned());
        if self.append_json {
            v.push("--json".to_string());
        }
        v
    }
}

fn stderr_tail(s: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    t.chars().skip(n.saturating_sub(400)).collect()
}

/// The error text for a run that exited non-zero.
pub fn failure_message(program: &str, code: Option<i32>, last_error: Option<&str>, stderr: &str) -> String {
    if Path::new(program).file_name().and_then(|n| n.to_str()) == Some("pkexec") {
        match code {
            Some(126) => return "Authorization was cancelled".to_string(),
            Some(127) => return "Not authorized".to_string(),
            _ => {}
        }
    }
    if let Some(message) = last_error {
        return message.to_string();
    }
    let tail = stderr_tail(stderr);
    if !tail.is_empty() {
        return tail;
    }
    match code {
        Some(c) => format!("flashcp exited with status {c}"),
        None => "flashcp was killed by a signal".to_string(),
    }
}

/// Runs each argument list in turn (argument lists are complete: program arguments only, the
/// program itself is `program`). Calls `on_update` for stage changes, plan/done events and
/// throttled progress. Returns the aggregate of all runs, or the first failure's message.
pub fn run_job<F>(
    program: &str,
    arg_lists: &[Vec<String>],
    op: Op,
    env: &[(&str, &str)],
    mut on_update: F,
) -> Result<JobAggregate, String>
where
    F: FnMut(JobUpdate),
{
    let total = arg_lists.len();
    let mut agg = JobAggregate::default();
    for (index, args) in arg_lists.iter().enumerate() {
        let mut stage = if index == 0 { Stage::Authorizing } else { Stage::Running };
        on_update(agg.update(stage, op, index, total, None));
        agg.start_run();

        let mut child = Command::new(program)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot start {program}: {e}"))?;

        let stderr_reader = child.stderr.take().map(|mut s| {
            thread::spawn(move || {
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf);
                buf
            })
        });

        let mut last_emit: Option<Instant> = None;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Some(event) = parse_line(&line) else {
                    continue;
                };
                agg.apply(&event);
                let emit = match event {
                    FlashcpEvent::Plan { .. } | FlashcpEvent::Done { .. } => true,
                    FlashcpEvent::Progress { .. } => last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL),
                    FlashcpEvent::Error { .. } | FlashcpEvent::Other => false,
                };
                if emit {
                    if stage == Stage::Authorizing {
                        stage = Stage::Running;
                    }
                    on_update(agg.update(stage, op, index, total, None));
                    last_emit = Some(Instant::now());
                }
            }
        }

        let status = child.wait().map_err(|e| format!("Waiting for {program} failed: {e}"))?;
        let stderr_text = stderr_reader.and_then(|h| h.join().ok()).unwrap_or_default();
        if !status.success() {
            return Err(failure_message(program, status.code(), agg.last_error(), &stderr_text));
        }
    }
    Ok(agg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fake-flashcp");

    fn file(path: &str) -> Source {
        Source { path: path.into(), is_dir: false }
    }

    fn dir(path: &str) -> Source {
        Source { path: path.into(), is_dir: true }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn add_file_goes_into_dest_dir() {
        let args = build_args(Op::Add, &[file("/home/u/a.mkv")], "/dev/sdb1", "/Movies/", false, "");
        assert_eq!(args, vec![s(&["add", "/home/u/a.mkv", "/dev/sdb1", "--to", "/Movies"])]);
    }

    #[test]
    fn add_dir_appends_basename_and_skip_flag() {
        let args = build_args(Op::Add, &[dir("/home/u/Photos")], "/dev/sdb1", "/", true, "");
        assert_eq!(args, vec![s(&["add", "/home/u/Photos", "/dev/sdb1", "--to", "/Photos", "--skip-existing"])]);
    }

    #[test]
    fn dest_is_normalised() {
        let args = build_args(Op::Add, &[dir("/x/Trip")], "/dev/sdb1", "//Backups//2024/", false, "");
        assert_eq!(args[0][4], "/Backups/2024/Trip");
        assert_eq!(normalize_dest(""), "/");
        assert_eq!(normalize_dest("/"), "/");
        assert_eq!(normalize_dest("a/b"), "/a/b");
    }

    #[test]
    fn one_list_per_source() {
        let args = build_args(Op::Add, &[file("/a"), dir("/b/c")], "/dev/sdb1", "/", false, "");
        assert_eq!(args.len(), 2);
        assert_eq!(args[1][4], "/c");
    }

    #[test]
    fn build_with_label() {
        let args = build_args(Op::Build, &[dir("/home/u/Stuff")], "/dev/sdb1", "/", false, "STUFF");
        assert_eq!(args, vec![s(&["build", "/home/u/Stuff", "/dev/sdb1", "--yes-erase", "--label", "STUFF"])]);
    }

    #[test]
    fn build_without_label_omits_it() {
        let args = build_args(Op::Build, &[dir("/home/u/Stuff")], "/dev/sdb1", "/", false, "");
        assert_eq!(args, vec![s(&["build", "/home/u/Stuff", "/dev/sdb1", "--yes-erase"])]);
    }

    #[test]
    fn pkexec_invocation_by_default() {
        let inv = Invocation::from_direct(None);
        assert_eq!(inv.program, "pkexec");
        assert_eq!(inv.full_args(&s(&["info", "/dev/sdb1"])), s(&["/usr/libexec/flashcp-helper", "info", "/dev/sdb1"]));
        // empty override is ignored
        assert_eq!(Invocation::from_direct(Some(String::new())).program, "pkexec");
    }

    #[test]
    fn direct_invocation_appends_json() {
        let inv = Invocation::from_direct(Some("/usr/bin/flashcp".into()));
        assert_eq!(inv.program, "/usr/bin/flashcp");
        assert_eq!(inv.full_args(&s(&["add", "/a", "/dev/sdb1"])), s(&["add", "/a", "/dev/sdb1", "--json"]));
    }

    #[test]
    fn failure_messages() {
        assert_eq!(failure_message("pkexec", Some(126), None, ""), "Authorization was cancelled");
        assert_eq!(failure_message("pkexec", Some(127), None, ""), "Not authorized");
        assert_eq!(failure_message("/usr/bin/pkexec", Some(127), Some("ignored"), ""), "Not authorized");
        // 126 only means "cancelled" for pkexec itself
        assert_eq!(failure_message("/usr/bin/flashcp", Some(126), Some("boom"), ""), "boom");
        assert_eq!(failure_message("pkexec", Some(1), Some("device busy"), ""), "device busy");
        assert_eq!(failure_message("pkexec", Some(1), None, "  flashcp-helper: no\n"), "flashcp-helper: no");
        assert_eq!(failure_message("x", Some(3), None, "   "), "flashcp exited with status 3");
        assert_eq!(failure_message("x", None, None, ""), "flashcp was killed by a signal");
    }

    #[test]
    fn fake_flashcp_summary_over_two_runs() {
        let arg_lists = vec![s(&["add", "/a", "/dev/sdz1", "--json"]), s(&["add", "/b", "/dev/sdz1", "--json"])];
        let mut updates = Vec::new();
        let agg = run_job(FAKE, &arg_lists, Op::Add, &[("FAKE_FLASHCP_DELAY", "0.001")], |u| updates.push(u))
            .expect("fake run succeeds");
        assert_eq!(agg.finished_files(), 200);
        assert_eq!(agg.finished_bytes(), 100_000_000);
        assert_eq!(agg.update(Stage::Done, Op::Add, 0, 1, None).plan_files, 200);
        assert_eq!(agg.update(Stage::Done, Op::Add, 0, 1, None).plan_bytes, 100_000_000);
        assert!((agg.secs() - 1.0).abs() < 1e-9);
        assert_eq!(updates.first().map(|u| u.stage), Some(Stage::Authorizing));
        assert!(updates.iter().any(|u| u.stage == Stage::Running));
        assert!(updates.iter().all(|u| u.total == 2));
        assert!(updates.iter().any(|u| u.index == 1));
    }

    #[test]
    fn fake_flashcp_single_run_summary() {
        let arg_lists = vec![s(&["build", "/a", "/dev/sdz1", "--yes-erase"])];
        let agg = run_job(FAKE, &arg_lists, Op::Build, &[("FAKE_FLASHCP_DELAY", "0.001")], |_| {})
            .expect("fake run succeeds");
        assert_eq!(agg.finished_files(), 100);
        assert_eq!(agg.finished_bytes(), 50_000_000);
    }

    #[test]
    fn fake_flashcp_failure_is_reported() {
        let arg_lists = vec![s(&["add", "/a", "/dev/sdz1", "--json"])];
        let err =
            run_job(FAKE, &arg_lists, Op::Add, &[("FAKE_FLASHCP_FAIL", "1"), ("FAKE_FLASHCP_DELAY", "0.001")], |_| {})
                .expect_err("fake failure must be an error");
        assert_eq!(err, "fake failure: device busy");
    }

    #[test]
    fn missing_program_is_an_error() {
        let err =
            run_job("/nonexistent/flashcp-binary", &[s(&["info"])], Op::Add, &[], |_| {}).expect_err("cannot start");
        assert!(err.starts_with("Cannot start /nonexistent/flashcp-binary"));
    }
}
