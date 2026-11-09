//! Optimize ("Optimize this stick"): the plan the dialog shows before a run, and the small
//! filesystem checks around it. flashcp does the work; these rules only decide what the GUI shows.

use serde::Serialize;
use serde_json::Value;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// flashcp's space need per entry (files and folders), and its fixed margin.
const PER_ENTRY_BYTES: u64 = 4096;
const MARGIN_BYTES: u64 = 64 * 1024 * 1024;
/// Prefix of the working folder flashcp creates for an optimize run.
const JOB_DIR_PREFIX: &str = "flashcp-optimize-";
/// The exFAT superblock magic (EXFAT_SUPER_MAGIC in linux/magic.h).
const EXFAT_SUPER_MAGIC: i128 = 0x2011_BAB0;

/// What the optimize dialog shows: flashcp's dry-run plan plus what the GUI checked.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizePlan {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub write_bytes: u64,
    /// The temporary copy flashcp will use (None when nothing is copied).
    pub temp: Option<String>,
    pub old_cluster: Option<u64>,
    pub new_cluster: Option<u64>,
    pub label: String,
    /// An interrupted job is finished from its verified copy.
    pub resume: bool,
    /// The stick is already in the optimal layout; nothing to do.
    pub already_optimized: bool,
    /// Free space flashcp needs in the temporary folder (0 when none is needed).
    pub need_bytes: u64,
    /// Free space in the temporary folder's filesystem, when it could be read.
    pub temp_free: Option<u64>,
    /// False only when the known free space is below the need. Unknown free space is not a refusal.
    pub enough_space: bool,
}

fn str_field(strategy: Option<&Value>, key: &str) -> Option<String> {
    strategy?.get(key)?.as_str().map(str::to_string)
}

fn u64_field(strategy: Option<&Value>, key: &str) -> Option<u64> {
    strategy?.get(key)?.as_u64()
}

fn bool_field(strategy: Option<&Value>, key: &str) -> bool {
    strategy.and_then(|s| s.get(key)).and_then(Value::as_bool).unwrap_or(false)
}

/// Builds the dialog plan from flashcp's plan event. `free` reports the free bytes of a path
/// (see `free_bytes`); it is a parameter so the rules can be tested without a filesystem.
pub fn plan_from(
    files: u64,
    dirs: u64,
    bytes: u64,
    write_bytes: u64,
    strategy: Option<&Value>,
    free: impl Fn(&Path) -> Option<u64>,
) -> OptimizePlan {
    let resume = bool_field(strategy, "resume");
    let already_optimized = bool_field(strategy, "already_optimized");
    // A resume works from the copy that already exists, and an optimized stick needs nothing.
    let need_bytes = if resume || already_optimized {
        0
    } else {
        bytes.saturating_add(files.saturating_add(dirs).saturating_mul(PER_ENTRY_BYTES)).saturating_add(MARGIN_BYTES)
    };
    let temp = str_field(strategy, "temp");
    let temp_free = temp.as_deref().and_then(|t| free(Path::new(t)));
    let enough_space = need_bytes == 0 || temp_free.is_none_or(|f| f >= need_bytes);
    OptimizePlan {
        files,
        dirs,
        bytes,
        write_bytes,
        temp,
        old_cluster: u64_field(strategy, "cluster"),
        new_cluster: u64_field(strategy, "new_cluster"),
        label: str_field(strategy, "label").unwrap_or_default(),
        resume,
        already_optimized,
        need_bytes,
        temp_free,
        enough_space,
    }
}

/// Free bytes available to unprivileged users on the filesystem holding `path`. A path that does
/// not exist yet is measured at its nearest existing ancestor. None when that cannot be read.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    let c_path = CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and st is a writable statvfs.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // Both fields are u64 on the 64-bit Linux targets this app is built for.
    Some(st.f_bavail.saturating_mul(st.f_frsize))
}

/// True when `temp_dir` holds a working folder left by an optimize run that did not finish.
pub fn pending_job(temp_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(temp_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_type().is_ok_and(|t| t.is_dir()) && entry.file_name().to_string_lossy().starts_with(JOB_DIR_PREFIX)
    })
}

/// The cluster size of the exFAT volume mounted at `mount_point` (the Linux exfat driver reports
/// it as the block size). None for any other filesystem, or when it cannot be read.
pub fn exfat_cluster(mount_point: &Path) -> Option<u64> {
    let c_path = CString::new(mount_point.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and st is a writable statfs.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    if i128::from(st.f_type) != EXFAT_SUPER_MAGIC {
        return None;
    }
    u64::try_from(st.f_bsize).ok().filter(|b| *b > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MIB: u64 = 1024 * 1024;

    fn strategy(temp: Value, resume: bool, already: bool) -> Value {
        json!({"temp": temp, "cluster": 32768, "new_cluster": 4096, "label": "FAKEUSB",
               "already_optimized": already, "resume": resume})
    }

    fn plan(strategy: Option<&Value>, free: Option<u64>) -> OptimizePlan {
        plan_from(100, 5, 50_000_000, 52_000_000, strategy, |_| free)
    }

    #[test]
    fn need_is_bytes_plus_entries_plus_margin() {
        let s = strategy(json!("/c/flashcp-optimize-X"), false, false);
        let need = 50_000_000 + 105 * 4096 + 64 * MIB;
        let p = plan(Some(&s), Some(need));
        assert_eq!(p.need_bytes, need);
        assert_eq!(p.temp.as_deref(), Some("/c/flashcp-optimize-X"));
        assert_eq!((p.old_cluster, p.new_cluster, p.label.as_str()), (Some(32768), Some(4096), "FAKEUSB"));
        assert!(p.enough_space);
        assert!(!p.resume && !p.already_optimized);
    }

    #[test]
    fn free_below_need_is_not_enough_and_exact_need_is() {
        let s = strategy(json!("/c/t"), false, false);
        let need = plan(Some(&s), None).need_bytes;
        assert!(!plan(Some(&s), Some(need - 1)).enough_space);
        assert!(plan(Some(&s), Some(need)).enough_space);
        assert_eq!(plan(Some(&s), Some(need - 1)).temp_free, Some(need - 1));
    }

    #[test]
    fn unknown_free_space_is_not_a_refusal() {
        let s = strategy(json!("/c/t"), false, false);
        let p = plan(Some(&s), None);
        assert!(p.enough_space);
        assert_eq!(p.temp_free, None);
    }

    #[test]
    fn resume_and_already_optimized_need_nothing() {
        let resume = strategy(json!("/c/t"), true, false);
        let p = plan(Some(&resume), Some(0));
        assert_eq!(p.need_bytes, 0);
        assert!(p.enough_space);
        assert!(p.resume);

        let done = strategy(Value::Null, false, true);
        let p = plan(Some(&done), Some(0));
        assert_eq!((p.need_bytes, p.enough_space), (0, true));
        assert!(p.already_optimized);
        assert_eq!(p.temp, None);
        assert_eq!(p.temp_free, None, "no temp folder, nothing to measure");
    }

    #[test]
    fn missing_strategy_gives_empty_defaults() {
        let p = plan_from(0, 0, 0, 0, None, |_| Some(1));
        assert_eq!(p.need_bytes, MIB * 64);
        assert_eq!(p.label, "");
        assert_eq!((p.old_cluster, p.new_cluster, p.temp), (None, None, None));
        assert_eq!(p.temp_free, None);
    }

    #[test]
    fn plan_serializes_camel_case() {
        let s = strategy(json!("/c/t"), false, false);
        let v = serde_json::to_value(plan(Some(&s), Some(1 << 40))).unwrap();
        assert_eq!(v["writeBytes"], 52_000_000);
        assert_eq!(v["needBytes"], 50_000_000 + 105 * 4096 + 64 * MIB);
        assert_eq!(v["enoughSpace"], true);
        assert_eq!(v["alreadyOptimized"], false);
        assert_eq!(v["newCluster"], 4096);
    }

    #[test]
    fn free_bytes_of_a_missing_subpath_is_the_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        let from_ancestor = free_bytes(dir.path()).expect("tempdir is measurable");
        assert!(from_ancestor > 0);
        assert_eq!(free_bytes(&deep), Some(from_ancestor));
        assert_eq!(free_bytes(Path::new("")), None);
    }

    #[test]
    fn pending_job_detects_an_unfinished_run_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!pending_job(dir.path()), "empty folder");
        assert!(!pending_job(&dir.path().join("missing")), "missing folder");
        fs::write(dir.path().join("flashcp-optimize-file"), "x").unwrap();
        assert!(!pending_job(dir.path()), "a file with the prefix is not a run");
        fs::create_dir(dir.path().join("other")).unwrap();
        assert!(!pending_job(dir.path()), "other folders are ignored");
        fs::create_dir(dir.path().join("flashcp-optimize-20260101-abc")).unwrap();
        assert!(pending_job(dir.path()));
    }

    #[test]
    fn exfat_cluster_is_none_on_a_non_exfat_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(exfat_cluster(dir.path()), None);
        assert_eq!(exfat_cluster(&dir.path().join("missing")), None);
    }
}
