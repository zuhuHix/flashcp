//! Plans the copy items, stages loose files for the helper, builds the helper or flashcp command
//! lines, runs them, and turns flashcp's JSON events into JobUpdates.

use crate::drives::Drive;
use crate::drives::NOT_TRUSTED_MESSAGE;
use crate::events::{FlashcpEvent, JobAggregate, JobUpdate, Op, RunValidator, Stage};
use crate::pull::Existing;
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const HELPER_PATH: &str = "/usr/libexec/flashcp-helper";
pub const TRUST_HELPER_PATH: &str = "/usr/libexec/flashcp-trust";
const DIRECT_ENV: &str = "FLASHCP_GUI_DIRECT";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
/// The helper takes at most this many --item pairs in one run.
pub const MAX_ITEMS: usize = 256;
/// Descriptor flashcp reads the device from (the helper dup2s the opened device there).
const TARGET_FD: &str = "3";
const HELPER_NOT_TRUSTED: &str = "flashcp-helper: drive not trusted";
static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

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
        let meta = fs::metadata(path).map_err(|e| format!("Cannot read {path}: {e}"))?;
        Ok(Source { path: path.to_string(), is_dir: meta.is_dir() })
    }
}

/// Destination inside the volume, always "/" or "/a/b" with no empty segments.
pub fn normalize_dest(dest: &str) -> String {
    let parts: Vec<&str> = dest.split('/').filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

/// Like normalize_dest, but refuses "." and ".." segments and control characters.
pub fn checked_dest(dest: &str) -> Result<String, String> {
    if dest.chars().any(char::is_control) || dest.split('/').any(|p| p == "." || p == "..") {
        return Err(format!("Destination folder is not allowed: {dest}"));
    }
    Ok(normalize_dest(dest))
}

fn base_name(path: &str) -> Result<String, String> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && !n.chars().any(char::is_control))
        .map(str::to_string)
        .ok_or_else(|| format!("Cannot copy {path}: its name is not usable"))
}

fn join_dest(dest: &str, name: &str) -> String {
    if dest == "/" {
        format!("/{name}")
    } else {
        format!("{dest}/{name}")
    }
}

/// One copy: `src` goes to `to` inside the volume. `sources` is how many picked sources it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub src: String,
    pub to: String,
    pub sources: usize,
}

/// The planned copy. `staged` pairs each loose file with its hard link inside `stage_dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub items: Vec<Item>,
    pub stage_dir: PathBuf,
    pub staged: Vec<(PathBuf, PathBuf)>,
}

/// Plans an add job. A directory source becomes `<dest>/<basename>`. Loose files are linked into
/// `stage_dir` and become one item `stage_dir -> dest`, placed where the first loose file was.
/// Two sources with the same name are refused, since they would land on the same path.
pub fn plan_items(sources: &[Source], dest_dir: &str, stage_dir: &Path) -> Result<Plan, String> {
    let dest = checked_dest(dest_dir)?;
    let mut items: Vec<Item> = Vec::new();
    let mut staged = Vec::new();
    let mut names = HashSet::new();
    let mut staging_item: Option<usize> = None;
    for src in sources {
        let name = base_name(&src.path)?;
        if !names.insert(name.clone()) {
            return Err(format!("Two items are named “{name}”. Rename one of them before copying."));
        }
        if src.is_dir {
            items.push(Item { src: src.path.clone(), to: join_dest(&dest, &name), sources: 1 });
        } else {
            let index = match staging_item {
                Some(index) => index,
                None => {
                    items.push(Item { src: stage_dir.to_string_lossy().into_owned(), to: dest.clone(), sources: 0 });
                    staging_item = Some(items.len() - 1);
                    items.len() - 1
                }
            };
            items[index].sources += 1;
            staged.push((PathBuf::from(&src.path), stage_dir.join(&name)));
        }
    }
    if items.len() > MAX_ITEMS {
        return Err(format!("Too many items at once (the limit is {MAX_ITEMS}). Copy them in smaller batches."));
    }
    Ok(Plan { items, stage_dir: stage_dir.to_path_buf(), staged })
}

fn cache_root(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, String> {
    if let Some(dir) = xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Ok(dir);
    }
    home.map(|h| PathBuf::from(h).join(".cache")).ok_or_else(|| "neither XDG_CACHE_HOME nor HOME is set".to_string())
}

/// A fresh staging folder for this process: $XDG_CACHE_HOME/flashcp-gui/stage-<pid>-<n>.
/// Only the path is computed here; StageDir::create makes it.
pub fn new_stage_dir() -> Result<PathBuf, String> {
    let root = cache_root(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))?;
    let n = STAGE_SEQ.fetch_add(1, Ordering::SeqCst);
    Ok(root.join("flashcp-gui").join(format!("stage-{}-{n}", std::process::id())))
}

/// The staged loose files. The folder is removed on drop, so it goes after success, failure or a panic.
pub struct StageDir(PathBuf);

impl StageDir {
    /// Creates the folder (mode 0700) and hard-links each file into it, copying when a link fails.
    /// Returns None when nothing is staged.
    pub fn create(dir: &Path, staged: &[(PathBuf, PathBuf)]) -> Result<Option<StageDir>, String> {
        if staged.is_empty() {
            return Ok(None);
        }
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
        }
        fs::DirBuilder::new().mode(0o700).create(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        let guard = StageDir(dir.to_path_buf());
        for (src, link) in staged {
            if fs::hard_link(src, link).is_err() {
                fs::copy(src, link).map_err(|e| format!("Cannot stage {}: {e}", src.display()))?;
            }
        }
        Ok(Some(guard))
    }
}

impl Drop for StageDir {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.0) {
            eprintln!("flashcp-gui: cannot remove staging folder {}: {e}", self.0.display());
        }
    }
}

/// What a job copies or erases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Add {
        items: Vec<Item>,
        skip_existing: bool,
    },
    /// Erase the drive, then copy the folder `src` onto it. Empty `label` omits --label.
    Build {
        src: String,
        confirm: String,
        label: String,
    },
    /// Copy files off the drive into the local folder `dest`. Empty `from` means the whole drive.
    Pull {
        from: String,
        existing: Existing,
        dest: String,
    },
}

/// One process to run. `covers` has one entry per `done` the process must emit: the number of
/// picked sources that done completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub args: Vec<String>,
    pub covers: Vec<usize>,
}

/// How the job reaches flashcp: through pkexec and the helper, or directly (dev dry runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Helper,
    Direct(String),
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

impl Invocation {
    pub fn from_env() -> Self {
        Self::from_direct(std::env::var(DIRECT_ENV).ok())
    }

    pub fn from_direct(direct: Option<String>) -> Self {
        match direct.filter(|s| !s.is_empty()) {
            Some(path) => Invocation::Direct(path),
            None => Invocation::Helper,
        }
    }

    /// The program to execute: pkexec for the helper, else the direct binary.
    pub fn program(&self) -> &str {
        match self {
            Invocation::Helper => "pkexec",
            Invocation::Direct(path) => path,
        }
    }

    /// Runs for this action. The helper gets one run for the whole job (it does not add --json;
    /// it runs flashcp with --json itself). Direct mode runs flashcp once per item, with --json.
    pub fn runs(&self, device: &str, expect: &str, action: &Action) -> Vec<Run> {
        match (self, action) {
            (Invocation::Helper, Action::Add { items, skip_existing }) => {
                let mut args = strings(&[HELPER_PATH, "add", device, "--expect", expect]);
                if *skip_existing {
                    args.push("--skip-existing".to_string());
                }
                for item in items {
                    args.push("--item".to_string());
                    args.push(item.src.clone());
                    args.push(item.to.clone());
                }
                vec![Run { args, covers: items.iter().map(|i| i.sources).collect() }]
            }
            (Invocation::Helper, Action::Build { src, confirm, label }) => {
                let mut args = strings(&[HELPER_PATH, "build", device, "--expect", expect, "--confirm", confirm]);
                if !label.is_empty() {
                    args.push("--label".to_string());
                    args.push(label.clone());
                }
                args.push(src.clone());
                vec![Run { args, covers: vec![1] }]
            }
            (Invocation::Helper, Action::Pull { from, existing, dest }) => {
                let mut args = strings(&[HELPER_PATH, "pull", device, "--expect", expect]);
                if !from.is_empty() {
                    args.extend(strings(&["--from", from]));
                }
                if let Some(flag) = existing.flag() {
                    args.push(flag.to_string());
                }
                args.push(dest.clone());
                vec![Run { args, covers: vec![1] }]
            }
            (Invocation::Direct(_), Action::Add { items, skip_existing }) => items
                .iter()
                .map(|item| {
                    let mut args = strings(&["add", &item.src, "--target-fd", TARGET_FD, "--to", &item.to]);
                    if *skip_existing {
                        args.push("--skip-existing".to_string());
                    }
                    args.push("--json".to_string());
                    Run { args, covers: vec![item.sources] }
                })
                .collect(),
            (Invocation::Direct(_), Action::Build { src, label, .. }) => {
                let mut args = strings(&["build", src, "--target-fd", TARGET_FD, "--yes-erase"]);
                if !label.is_empty() {
                    args.push("--label".to_string());
                    args.push(label.clone());
                }
                args.push("--json".to_string());
                vec![Run { args, covers: vec![1] }]
            }
            (Invocation::Direct(_), Action::Pull { from, existing, dest }) => {
                let mut args = strings(&["pull", "--target-fd", TARGET_FD, dest]);
                if !from.is_empty() {
                    args.extend(strings(&["--from", from]));
                }
                if let Some(flag) = existing.flag() {
                    args.push(flag.to_string());
                }
                args.push("--json".to_string());
                vec![Run { args, covers: vec![1] }]
            }
        }
    }
}

fn stderr_tail(s: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    t.chars().skip(n.saturating_sub(400)).collect()
}

/// The message for pkexec's own exit codes, when they mean something to the user.
fn pkexec_code_message(code: Option<i32>) -> Option<String> {
    match code {
        Some(126) => Some("Authorization was cancelled".to_string()),
        Some(127) => Some("Not authorized".to_string()),
        _ => None,
    }
}

/// The error text for a process that exited non-zero, when its stream did not already explain it.
pub fn failure_message(program: &str, code: Option<i32>, stderr: &str) -> String {
    let is_pkexec = Path::new(program).file_name().and_then(|n| n.to_str()) == Some("pkexec");
    if is_pkexec {
        if let Some(message) = pkexec_code_message(code) {
            return message;
        }
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

/// Turns internal messages into the text the user sees.
pub fn user_message(message: String) -> String {
    if message.starts_with(HELPER_NOT_TRUSTED) {
        NOT_TRUSTED_MESSAGE.to_string()
    } else {
        message
    }
}

fn index_of(covered: usize, total: usize) -> usize {
    covered.min(total.saturating_sub(1))
}

/// Runs each process in turn and folds its validated events into one aggregate. `on_update`
/// gets stage changes, plan and done events, and throttled progress. The first problem (a
/// stream violation, an error event, or a failed exit) stops the job: the process is drained and
/// waited for (never killed mid-write), and the first message is returned.
pub fn run_job<F>(
    program: &str,
    runs: &[Run],
    op: Op,
    env: &[(&str, &str)],
    mut on_update: F,
) -> Result<JobAggregate, String>
where
    F: FnMut(JobUpdate),
{
    let total: usize = runs.iter().flat_map(|r| r.covers.iter()).sum();
    let mut agg = JobAggregate::default();
    let mut covered = 0usize;
    for (run_index, run) in runs.iter().enumerate() {
        let mut stage = if run_index == 0 { Stage::Authorizing } else { Stage::Running };
        on_update(agg.update(stage, op, index_of(covered, total), total, None));
        agg.start_run();
        let mut validator = RunValidator::new(run.covers.len());

        let mut child = Command::new(program)
            .args(&run.args)
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

        let mut failure: Option<String> = None;
        let mut last_emit: Option<Instant> = None;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // After a protocol violation keep draining instead of killing: the child may be
                // flashcp mid-write, and cutting it off could leave the volume half-updated.
                if failure.is_some() {
                    continue;
                }
                let event = match validator.feed(&line) {
                    Ok(Some(event)) => event,
                    Ok(None) => continue,
                    Err(message) => {
                        failure = Some(message);
                        continue;
                    }
                };
                agg.apply(&event);
                let emit = match event {
                    FlashcpEvent::Done { .. } => {
                        let item = validator.dones().saturating_sub(1);
                        covered += run.covers.get(item).copied().unwrap_or(0);
                        true
                    }
                    FlashcpEvent::Plan { .. } => true,
                    FlashcpEvent::Progress { .. } => last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL),
                };
                if emit {
                    if stage == Stage::Authorizing {
                        stage = Stage::Running;
                    }
                    on_update(agg.update(stage, op, index_of(covered, total), total, None));
                    last_emit = Some(Instant::now());
                }
            }
        }

        if let Some(message) = failure {
            let _ = child.wait();
            return Err(user_message(message));
        }
        let status: ExitStatus = child.wait().map_err(|e| format!("Waiting for {program} failed: {e}"))?;
        let stderr_text = stderr_reader.and_then(|h| h.join().ok()).unwrap_or_default();
        match validator.finish(status) {
            Ok(()) => {}
            Err(_) if !status.success() => {
                return Err(user_message(failure_message(program, status.code(), &stderr_text)));
            }
            Err(message) => return Err(user_message(message)),
        }
    }
    Ok(agg)
}

fn error_message(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("event")?.as_str()? != "error" {
        return None;
    }
    value.get("message")?.as_str().map(str::to_string)
}

/// Runs flashcp-trust through pkexec for the drive with this identity key. Blocks until it exits.
/// The exact argv for flashcp-trust: the full identity the user was shown, so the helper can
/// refuse if the device it opens differs in any field.
pub fn trust_args(drive: &Drive) -> Vec<String> {
    let id = &drive.identity;
    vec![
        TRUST_HELPER_PATH.to_string(),
        drive.device.clone(),
        "--expect".into(),
        id.key.clone(),
        "--wwn".into(),
        id.wwn.clone(),
        "--model".into(),
        id.model.clone(),
        "--size".into(),
        id.disk_size.to_string(),
    ]
}

pub fn trust_device(drive: &Drive) -> Result<(), String> {
    let output = Command::new("pkexec")
        .args(trust_args(drive))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Cannot start pkexec: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message =
        stdout.lines().find_map(error_message).or_else(|| pkexec_code_message(output.status.code())).unwrap_or_else(
            || {
                let tail = stderr_tail(&stderr);
                if tail.is_empty() {
                    "Could not trust the drive".to_string()
                } else {
                    tail
                }
            },
        );
    Err(user_message(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_args_equal_the_identity_the_dialog_shows() {
        let mut d = crate::drives::fake_drives().remove(0);
        d.identity = crate::drives::TrustIdentity::from_udisks(
            "500014a000000001",
            "0x500014a000000001",
            "DATAMAG 20Gbps",
            1_000_204_886_016,
        );
        // The dialog renders the serialized `identity` object (ui/logic.js trustDetails); the
        // argv must carry exactly those values.
        let json = serde_json::to_value(&d).unwrap();
        let shown = &json["identity"];
        assert_eq!(shown["keyKind"], "serial");
        let expected: Vec<String> = vec![
            TRUST_HELPER_PATH.into(),
            "/dev/sdz1".into(),
            "--expect".into(),
            shown["key"].as_str().unwrap().into(),
            "--wwn".into(),
            shown["wwn"].as_str().unwrap().into(),
            "--model".into(),
            shown["model"].as_str().unwrap().into(),
            "--size".into(),
            shown["diskSize"].as_u64().unwrap().to_string(),
        ];
        assert_eq!(trust_args(&d), expected);
        assert_eq!(
            &expected[3..],
            ["500014a000000001", "--wwn", "0x500014a000000001", "--model", "DATAMAG 20Gbps", "--size", "1000204886016"]
        );
    }

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fake-flashcp");
    const FAST: (&str, &str) = ("FAKE_FLASHCP_DELAY", "0.001");

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn file(path: &str) -> Source {
        Source { path: path.into(), is_dir: false }
    }

    fn dir(path: &str) -> Source {
        Source { path: path.into(), is_dir: true }
    }

    fn item(src: &str, to: &str, sources: usize) -> Item {
        Item { src: src.into(), to: to.into(), sources }
    }

    fn run(args: &[&str], covers: &[usize]) -> Run {
        Run { args: s(args), covers: covers.to_vec() }
    }

    fn fail(runs: &[Run], env: &[(&str, &str)]) -> String {
        run_job(FAKE, runs, Op::Add, env, |_| {}).expect_err("the job must fail")
    }

    // ---- planning ----

    #[test]
    fn directories_land_under_dest_by_basename() {
        let plan = plan_items(&[dir("/x/Photos"), dir("/x/Docs")], "/Backups/", Path::new("/c/stage")).unwrap();
        assert_eq!(plan.items, vec![item("/x/Photos", "/Backups/Photos", 1), item("/x/Docs", "/Backups/Docs", 1)]);
        assert!(plan.staged.is_empty());
    }

    #[test]
    fn root_dest_joins_without_double_slash() {
        let plan = plan_items(&[dir("/x/Photos")], "/", Path::new("/c/stage")).unwrap();
        assert_eq!(plan.items[0].to, "/Photos");
        let plan = plan_items(&[dir("/x/Photos")], "", Path::new("/c/stage")).unwrap();
        assert_eq!(plan.items[0].to, "/Photos");
    }

    #[test]
    fn loose_files_share_one_staged_item() {
        let sources = [file("/a/one.txt"), dir("/b/Photos"), file("/a/two.txt")];
        let plan = plan_items(&sources, "/Docs", Path::new("/c/stage-1-0")).unwrap();
        assert_eq!(plan.items, vec![item("/c/stage-1-0", "/Docs", 2), item("/b/Photos", "/Docs/Photos", 1)]);
        assert_eq!(
            plan.staged,
            vec![
                (PathBuf::from("/a/one.txt"), PathBuf::from("/c/stage-1-0/one.txt")),
                (PathBuf::from("/a/two.txt"), PathBuf::from("/c/stage-1-0/two.txt")),
            ]
        );
    }

    #[test]
    fn duplicate_basenames_are_refused() {
        let err = plan_items(&[file("/a/x.txt"), file("/b/x.txt")], "/", Path::new("/c/s")).unwrap_err();
        assert!(err.contains("“x.txt”"), "{err}");
    }

    #[test]
    fn folder_and_file_with_the_same_name_are_refused() {
        assert!(plan_items(&[file("/a/Photos"), dir("/b/Photos")], "/", Path::new("/c/s")).is_err());
    }

    #[test]
    fn dest_with_dotdot_or_control_chars_is_refused() {
        assert!(plan_items(&[dir("/x/A")], "/a/../b", Path::new("/c/s")).is_err());
        assert!(plan_items(&[dir("/x/A")], "/a/./b", Path::new("/c/s")).is_err());
        assert!(plan_items(&[dir("/x/A")], "/a\nb", Path::new("/c/s")).is_err());
        assert!(plan_items(&[dir("/x/A")], "/a/..b", Path::new("/c/s")).is_ok());
    }

    #[test]
    fn too_many_items_are_refused() {
        let many: Vec<Source> = (0..=MAX_ITEMS).map(|i| dir(&format!("/x/d{i}"))).collect();
        assert!(plan_items(&many, "/", Path::new("/c/s")).is_err());
        assert!(plan_items(&many[..MAX_ITEMS], "/", Path::new("/c/s")).is_ok());
    }

    #[test]
    fn cache_root_prefers_absolute_xdg() {
        assert_eq!(cache_root(Some("/xc".into()), Some("/h".into())).unwrap(), PathBuf::from("/xc"));
        assert_eq!(cache_root(Some("rel".into()), Some("/h".into())).unwrap(), PathBuf::from("/h/.cache"));
        assert_eq!(cache_root(None, Some("/h".into())).unwrap(), PathBuf::from("/h/.cache"));
        assert!(cache_root(None, None).is_err());
    }

    #[test]
    fn staging_links_real_files_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("Photos");
        fs::create_dir(&photos).unwrap();
        let one = dir.path().join("one.txt");
        fs::write(&one, "hello").unwrap();
        let stage = dir.path().join("cache/flashcp-gui/stage-test");

        let sources = [Source { path: one.to_string_lossy().into(), is_dir: false }, dir_src(&photos)];
        let plan = plan_items(&sources, "/", &stage).unwrap();
        assert_eq!(plan.items[0].src, stage.to_string_lossy());
        assert!(!stage.exists(), "planning must not touch the disk");

        let guard = StageDir::create(&plan.stage_dir, &plan.staged).unwrap().expect("staged");
        assert_eq!(fs::read_to_string(stage.join("one.txt")).unwrap(), "hello");
        assert!(stage.join("one.txt").exists());
        let mode = fs::metadata(&stage).unwrap().permissions();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o700);
        drop(guard);
        assert!(!stage.exists(), "staging folder is removed on drop");
        assert!(one.exists(), "the user's own file is never removed");
    }

    fn dir_src(p: &Path) -> Source {
        Source { path: p.to_string_lossy().into(), is_dir: true }
    }

    #[test]
    fn nothing_staged_means_no_folder() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        assert!(StageDir::create(&stage, &[]).unwrap().is_none());
        assert!(!stage.exists());
    }

    // ---- command lines ----

    #[test]
    fn helper_add_has_one_item_pair_per_item() {
        let action =
            Action::Add { items: vec![item("/h/Photos", "/Photos", 1), item("/c/stage", "/", 2)], skip_existing: true };
        let runs = Invocation::Helper.runs("/dev/sdz1", "FAKE0001", &action);
        assert_eq!(runs.len(), 1, "one helper run per job");
        assert_eq!(
            runs[0].args,
            s(&[
                HELPER_PATH,
                "add",
                "/dev/sdz1",
                "--expect",
                "FAKE0001",
                "--skip-existing",
                "--item",
                "/h/Photos",
                "/Photos",
                "--item",
                "/c/stage",
                "/",
            ])
        );
        assert_eq!(runs[0].covers, vec![1, 2]);
    }

    #[test]
    fn helper_build_passes_confirm_and_label_before_the_folder() {
        let action = Action::Build { src: "/h/Stuff".into(), confirm: "FAKEUSB".into(), label: "FAKEUSB".into() };
        let runs = Invocation::Helper.runs("/dev/sdz1", "FAKE0001", &action);
        assert_eq!(
            runs[0].args,
            s(&[
                HELPER_PATH,
                "build",
                "/dev/sdz1",
                "--expect",
                "FAKE0001",
                "--confirm",
                "FAKEUSB",
                "--label",
                "FAKEUSB",
                "/h/Stuff",
            ])
        );
        assert_eq!(runs[0].covers, vec![1]);
        let action = Action::Build { src: "/h/Stuff".into(), confirm: "FAKE0001".into(), label: String::new() };
        let args = &Invocation::Helper.runs("/dev/sdz1", "FAKE0001", &action)[0].args;
        assert!(!args.iter().any(|a| a == "--label"));
    }

    #[test]
    fn direct_mode_runs_one_process_per_item() {
        let action = Action::Add {
            items: vec![item("/h/Photos", "/Photos", 1), item("/c/stage", "/", 2)],
            skip_existing: false,
        };
        let runs = Invocation::Direct("/fake".into()).runs("/dev/sdz1", "FAKE0001", &action);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].args, s(&["add", "/h/Photos", "--target-fd", "3", "--to", "/Photos", "--json"]));
        assert_eq!(runs[0].covers, vec![1]);
        assert_eq!(runs[1].args, s(&["add", "/c/stage", "--target-fd", "3", "--to", "/", "--json"]));
        assert_eq!(runs[1].covers, vec![2]);
    }

    #[test]
    fn direct_build_mirrors_flashcp_arguments() {
        let action = Action::Build { src: "/h/Stuff".into(), confirm: "X".into(), label: "STUFF".into() };
        let runs = Invocation::Direct("/fake".into()).runs("/dev/sdz1", "X", &action);
        assert_eq!(
            runs[0].args,
            s(&["build", "/h/Stuff", "--target-fd", "3", "--yes-erase", "--label", "STUFF", "--json"])
        );
    }

    fn pull(from: &str, existing: Existing) -> Action {
        Action::Pull { from: from.into(), existing, dest: "/home/u/Downloads/USB".into() }
    }

    #[test]
    fn helper_pull_argv_with_and_without_from_and_each_existing_mode() {
        let dev = "/dev/sdz1";
        let whole = Invocation::Helper.runs(dev, "FAKE0001", &pull("", Existing::Fail));
        assert_eq!(whole.len(), 1, "one helper run per job");
        assert_eq!(whole[0].args, s(&[HELPER_PATH, "pull", dev, "--expect", "FAKE0001", "/home/u/Downloads/USB"]));
        assert_eq!(whole[0].covers, vec![1]);

        let folder = Invocation::Helper.runs(dev, "FAKE0001", &pull("Photos/2026", Existing::Skip));
        assert_eq!(
            folder[0].args,
            s(&[
                HELPER_PATH,
                "pull",
                dev,
                "--expect",
                "FAKE0001",
                "--from",
                "Photos/2026",
                "--skip-existing",
                "/home/u/Downloads/USB",
            ])
        );

        let over = Invocation::Helper.runs(dev, "FAKE0001", &pull("Photos", Existing::Overwrite));
        assert_eq!(
            over[0].args,
            s(&[
                HELPER_PATH,
                "pull",
                dev,
                "--expect",
                "FAKE0001",
                "--from",
                "Photos",
                "--overwrite",
                "/home/u/Downloads/USB"
            ])
        );
    }

    #[test]
    fn direct_pull_argv_with_and_without_from_and_each_existing_mode() {
        let direct = Invocation::Direct("/fake".into());
        let whole = direct.runs("/dev/sdz1", "FAKE0001", &pull("", Existing::Fail));
        assert_eq!(whole[0].args, s(&["pull", "--target-fd", "3", "/home/u/Downloads/USB", "--json"]));
        assert_eq!(whole[0].covers, vec![1]);

        let skip = direct.runs("/dev/sdz1", "FAKE0001", &pull("Photos/2026", Existing::Skip));
        assert_eq!(
            skip[0].args,
            s(&[
                "pull",
                "--target-fd",
                "3",
                "/home/u/Downloads/USB",
                "--from",
                "Photos/2026",
                "--skip-existing",
                "--json",
            ])
        );

        let over = direct.runs("/dev/sdz1", "FAKE0001", &pull("Photos", Existing::Overwrite));
        assert_eq!(
            over[0].args,
            s(&["pull", "--target-fd", "3", "/home/u/Downloads/USB", "--from", "Photos", "--overwrite", "--json"])
        );
    }

    #[test]
    fn invocation_selects_helper_unless_direct_is_set() {
        assert_eq!(Invocation::from_direct(None), Invocation::Helper);
        assert_eq!(Invocation::from_direct(Some(String::new())), Invocation::Helper);
        assert_eq!(Invocation::Helper.program(), "pkexec");
        assert_eq!(Invocation::from_direct(Some("/usr/bin/flashcp".into())).program(), "/usr/bin/flashcp");
    }

    #[test]
    fn failure_messages() {
        assert_eq!(failure_message("pkexec", Some(126), ""), "Authorization was cancelled");
        assert_eq!(failure_message("pkexec", Some(127), ""), "Not authorized");
        assert_eq!(failure_message("/usr/bin/pkexec", Some(127), "ignored"), "Not authorized");
        // 126 only means "cancelled" for pkexec itself
        assert_eq!(failure_message("/usr/bin/flashcp", Some(126), "boom"), "boom");
        assert_eq!(failure_message("pkexec", Some(1), "  flashcp-helper: no\n"), "flashcp-helper: no");
        assert_eq!(failure_message("x", Some(3), "   "), "flashcp exited with status 3");
        assert_eq!(failure_message("x", None, ""), "flashcp was killed by a signal");
    }

    #[test]
    fn untrusted_helper_message_is_user_facing() {
        assert_eq!(
            user_message("flashcp-helper: drive not trusted: FAKE0001".to_string()),
            "This drive isn't trusted yet. Use “Trust this drive…” first."
        );
        assert_eq!(user_message("Unmount failed".to_string()), "Unmount failed");
    }

    // ---- running the fake ----

    #[test]
    fn normal_stream_succeeds() {
        let mut updates = Vec::new();
        let runs = [run(&["add", "/a", "--target-fd", "3", "--json"], &[1])];
        let agg = run_job(FAKE, &runs, Op::Add, &[FAST], |u| updates.push(u)).expect("fake succeeds");
        assert_eq!(agg.finished_files(), 100);
        assert_eq!(agg.finished_bytes(), 50_000_000);
        assert_eq!(updates.first().map(|u| u.stage), Some(Stage::Authorizing));
        assert!(updates.iter().any(|u| u.stage == Stage::Running));
        assert!(updates.iter().all(|u| u.total == 1 && u.index == 0));
    }

    #[test]
    fn build_stream_succeeds() {
        let runs = [run(&["build", "/a", "--target-fd", "3", "--yes-erase", "--json"], &[1])];
        let agg = run_job(FAKE, &runs, Op::Build, &[FAST], |_| {}).expect("fake succeeds");
        assert_eq!(agg.finished_files(), 100);
    }

    #[test]
    fn two_items_in_one_process_succeed_when_both_expected() {
        let mut updates = Vec::new();
        let runs = [run(&["add", "--json"], &[1, 1])];
        let agg = run_job(FAKE, &runs, Op::Add, &[("FAKE_FLASHCP_MODE", "twodone"), FAST], |u| updates.push(u))
            .expect("two dones for two items");
        assert_eq!(agg.finished_files(), 200);
        assert_eq!(agg.finished_bytes(), 100_000_000);
        assert!(updates.iter().any(|u| u.index == 1 && u.total == 2));
    }

    #[test]
    fn extra_done_fails_the_job() {
        let runs = [run(&["add", "--json"], &[1])];
        let msg = fail(&runs, &[("FAKE_FLASHCP_MODE", "twodone"), FAST]);
        assert_eq!(msg, "flashcp finished more items than expected (1)");
    }

    #[test]
    fn garbage_line_fails_the_job() {
        let runs = [run(&["add", "--json"], &[1])];
        assert_eq!(
            fail(&runs, &[("FAKE_FLASHCP_MODE", "garbage"), FAST]),
            "flashcp sent unreadable output: this is not json"
        );
    }

    #[test]
    fn stream_without_done_fails_the_job() {
        let runs = [run(&["add", "--json"], &[1])];
        assert_eq!(
            fail(&runs, &[("FAKE_FLASHCP_MODE", "nodone"), FAST]),
            "flashcp stopped before finishing (0 of 1 items done)"
        );
    }

    #[test]
    fn error_event_fails_with_its_message() {
        let runs = [run(&["add", "--json"], &[1])];
        assert_eq!(fail(&runs, &[("FAKE_FLASHCP_MODE", "error"), FAST]), "fake failure: device busy");
    }

    #[test]
    fn fail_flag_is_an_alias_of_the_error_mode() {
        let runs = [run(&["add", "--json"], &[1])];
        assert_eq!(fail(&runs, &[("FAKE_FLASHCP_FAIL", "1"), FAST]), "fake failure: device busy");
    }

    #[test]
    fn helper_run_with_missing_items_fails() {
        // the helper promised two items but the stream finishes only one
        let runs = [run(&["add", "--json"], &[1, 1])];
        assert_eq!(fail(&runs, &[FAST]), "flashcp stopped before finishing (1 of 2 items done)");
    }

    #[test]
    fn exit_code_without_error_event_fails_the_job() {
        let runs = [run(&["-c", "exit 3"], &[1])];
        assert_eq!(run_job("/bin/sh", &runs, Op::Add, &[], |_| {}).unwrap_err(), "flashcp exited with status 3");
    }

    #[test]
    fn helper_refusal_becomes_the_trust_message() {
        let script = r#"echo '{"event":"error","message":"flashcp-helper: drive not trusted: FAKE0001"}'; exit 3"#;
        let runs = [run(&["-c", script], &[1])];
        assert_eq!(
            run_job("/bin/sh", &runs, Op::Add, &[], |_| {}).unwrap_err(),
            "This drive isn't trusted yet. Use “Trust this drive…” first."
        );
    }

    #[test]
    fn direct_mode_passes_the_expected_argv() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv.log");
        let action =
            Action::Add { items: vec![item("/h/Photos", "/Photos", 1), item("/c/stage", "/", 2)], skip_existing: true };
        let runs = Invocation::Direct(FAKE.into()).runs("/dev/sdz1", "FAKE0001", &action);
        let log_s = log.to_string_lossy().into_owned();
        let env = [("FAKE_FLASHCP_LOG", log_s.as_str()), FAST];
        let agg = run_job(FAKE, &runs, Op::Add, &env, |_| {}).expect("each item finishes once");
        assert_eq!(agg.finished_files(), 200);
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "add\n/h/Photos\n--target-fd\n3\n--to\n/Photos\n--skip-existing\n--json\n--\n\
             add\n/c/stage\n--target-fd\n3\n--to\n/\n--skip-existing\n--json\n--\n"
        );
    }

    fn pull_run(args: &[&str]) -> Vec<Run> {
        let mut argv = vec!["pull", "--target-fd", "3", "/home/u/out"];
        argv.extend_from_slice(args);
        argv.push("--json");
        vec![run(&argv, &[1])]
    }

    fn pull_job(runs: &[Run], env: &[(&str, &str)]) -> Result<JobAggregate, String> {
        run_job(FAKE, runs, Op::Pull, env, |_| {})
    }

    #[test]
    fn pull_normal_stream_succeeds() {
        let mut updates = Vec::new();
        let runs = pull_run(&[]);
        let agg = run_job(FAKE, &runs, Op::Pull, &[FAST], |u| updates.push(u)).expect("fake pull succeeds");
        assert_eq!(agg.finished_files(), 100);
        assert_eq!(updates.first().map(|u| u.stage), Some(Stage::Authorizing));
        assert!(updates.iter().all(|u| u.op == Op::Pull && u.total == 1 && !u.conflict));
    }

    #[test]
    fn pull_collision_without_flags_fails_with_the_collision_message() {
        let runs = pull_run(&[]);
        let msg = pull_job(&runs, &[("FAKE_FLASHCP_MODE", "conflict"), FAST]).expect_err("must refuse");
        assert!(msg.contains("already exist"), "{msg}");
        assert!(crate::pull::is_collision(&msg));
    }

    #[test]
    fn pull_collision_with_skip_or_overwrite_succeeds() {
        for flag in ["--skip-existing", "--overwrite"] {
            let runs = pull_run(&[flag]);
            let agg = pull_job(&runs, &[("FAKE_FLASHCP_MODE", "conflict"), FAST]).expect("retry with a flag succeeds");
            assert_eq!(agg.finished_files(), 100, "{flag}");
        }
    }

    #[test]
    fn pull_nodone_and_twodone_fail() {
        let runs = pull_run(&[]);
        assert_eq!(
            pull_job(&runs, &[("FAKE_FLASHCP_MODE", "nodone"), FAST]).unwrap_err(),
            "flashcp stopped before finishing (0 of 1 items done)"
        );
        assert_eq!(
            pull_job(&runs, &[("FAKE_FLASHCP_MODE", "twodone"), FAST]).unwrap_err(),
            "flashcp finished more items than expected (1)"
        );
    }

    #[test]
    fn pull_direct_argv_reaches_flashcp() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv.log");
        let log_s = log.to_string_lossy().into_owned();
        let action = pull("Photos/2026", Existing::Skip);
        let runs = Invocation::Direct(FAKE.into()).runs("/dev/sdz1", "FAKE0001", &action);
        let env = [("FAKE_FLASHCP_LOG", log_s.as_str()), FAST];
        pull_job(&runs, &env).expect("fake pull succeeds");
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "pull\n--target-fd\n3\n/home/u/Downloads/USB\n--from\nPhotos/2026\n--skip-existing\n--json\n--\n"
        );
    }

    #[test]
    fn missing_program_is_an_error() {
        let err = run_job("/nonexistent/flashcp-binary", &[run(&["info"], &[1])], Op::Add, &[], |_| {})
            .expect_err("cannot start");
        assert!(err.starts_with("Cannot start /nonexistent/flashcp-binary"));
    }
}
