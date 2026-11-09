//! Optimize ("Optimize this stick"): the plan the dialog shows before a run, and the small
//! filesystem checks around it. flashcp does the work; these rules only decide what the GUI shows.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::drives::{Drive, TrustIdentity};

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
    /// The temporary folder is on RAM (tmpfs/ramfs): the copy could be lost while the stick is
    /// erased. The plan is refused (flashcp refuses it too; this says so before any password).
    pub temp_volatile: bool,
    /// Single-use confirmation for `start_optimize`, bound to this drive (see [`Plans`]).
    /// Empty until the plan is registered.
    pub token: String,
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
    let temp_volatile = need_bytes > 0 && temp.as_deref().is_some_and(|t| is_volatile(Path::new(t)));
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
        temp_volatile,
        token: String::new(),
    }
}

const TMPFS_MAGIC: i128 = 0x0102_1994;
const RAMFS_MAGIC: i128 = 0x8584_58f6;

/// True when the nearest existing ancestor of `path` is on tmpfs or ramfs. Runs as the user.
pub fn is_volatile(path: &Path) -> bool {
    let Some(existing) = path.ancestors().find(|p| p.exists()) else { return false };
    let Ok(c_path) = CString::new(existing.as_os_str().as_bytes()) else { return false };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and st is a writable statfs.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let t = i128::from(st.f_type);
    t == TMPFS_MAGIC || t == RAMFS_MAGIC
}

/// How long a plan's confirmation stays valid.
pub const PLAN_TTL: Duration = Duration::from_secs(10 * 60);
pub const DRIVE_CHANGED: &str = "The drive changed, please plan again.";
pub const PLAN_UNKNOWN: &str = "This optimize plan is unknown or was already used, please plan again.";
pub const PLAN_EXPIRED: &str = "This optimize plan has expired, please plan again.";
pub const SPACE_CHANGED: &str =
    "There is no longer enough free space on this computer for the temporary copy, please plan again.";

/// What a confirmed plan was made for: the drive (by udisks id, device node and the full identity
/// tuple flashcp-trust checks) and the space it needs.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFor {
    pub drive_id: String,
    pub device: String,
    pub identity: TrustIdentity,
    pub temp: Option<String>,
    pub need_bytes: u64,
    pub resume: bool,
    pub issued: Instant,
}

/// Outstanding plan tokens. `start_optimize` redeems one: single use, expires after
/// [`PLAN_TTL`], valid only for the identical drive and while the space still suffices.
#[derive(Debug, Default)]
pub struct Plans(HashMap<String, PlannedFor>);

/// 128 random bits from the kernel, hex.
pub fn random_token() -> Result<String, String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("Cannot read /dev/urandom: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn same_drive(planned: &PlannedFor, now: &Drive) -> bool {
    planned.drive_id == now.id && planned.device == now.device && planned.identity == now.identity
}

impl Plans {
    /// Registers a plan for `drive` under a fresh token and returns it. Expired plans are dropped.
    pub fn issue(&mut self, drive: &Drive, plan: &OptimizePlan, now: Instant) -> Result<String, String> {
        self.0.retain(|_, p| now.duration_since(p.issued) < PLAN_TTL);
        let token = random_token()?;
        self.0.insert(
            token.clone(),
            PlannedFor {
                drive_id: drive.id.clone(),
                device: drive.device.clone(),
                identity: drive.identity.clone(),
                temp: plan.temp.clone(),
                need_bytes: plan.need_bytes,
                resume: plan.resume,
                issued: now,
            },
        );
        Ok(token)
    }

    /// Consumes `token` (whatever the outcome, so it can never be tried twice) and checks it
    /// against the drive as it is NOW and the free space now.
    pub fn redeem(
        &mut self,
        token: &str,
        drive_now: &Drive,
        now: Instant,
        free: impl Fn(&Path) -> Option<u64>,
    ) -> Result<PlannedFor, String> {
        let planned = self.0.remove(token).ok_or(PLAN_UNKNOWN)?;
        if now.duration_since(planned.issued) >= PLAN_TTL {
            return Err(PLAN_EXPIRED.into());
        }
        if !same_drive(&planned, drive_now) {
            return Err(DRIVE_CHANGED.into());
        }
        if planned.need_bytes > 0 {
            if let Some(t) = &planned.temp {
                if free(Path::new(t)).is_some_and(|f| f < planned.need_bytes) {
                    return Err(SPACE_CHANGED.into());
                }
            }
        }
        Ok(planned)
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

    fn drive() -> Drive {
        let mut d = crate::drives::fake_drives().remove(0);
        d.identity = TrustIdentity::from_udisks("SER123456", "-", "Stick", 32_000_000_000);
        d
    }

    fn issued(plans: &mut Plans, d: &Drive, need: u64, at: Instant) -> String {
        let s = strategy(json!("/c/t"), false, false);
        let mut p = plan(Some(&s), None);
        p.need_bytes = need;
        plans.issue(d, &p, at).unwrap()
    }

    #[test]
    fn tokens_are_random_and_single_use() {
        let (mut plans, d, t0) = (Plans::default(), drive(), Instant::now());
        let a = issued(&mut plans, &d, 10, t0);
        let b = issued(&mut plans, &d, 10, t0);
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(plans.redeem(&a, &d, t0, |_| Some(10)).is_ok());
        assert_eq!(plans.redeem(&a, &d, t0, |_| Some(10)).unwrap_err(), PLAN_UNKNOWN, "reuse");
        assert_eq!(plans.redeem("", &d, t0, |_| Some(10)).unwrap_err(), PLAN_UNKNOWN, "no token");
        assert_eq!(plans.redeem("deadbeef", &d, t0, |_| Some(10)).unwrap_err(), PLAN_UNKNOWN);
        assert!(plans.redeem(&b, &d, t0, |_| Some(10)).is_ok());
    }

    #[test]
    fn expired_tokens_are_refused() {
        let (mut plans, d, t0) = (Plans::default(), drive(), Instant::now());
        let a = issued(&mut plans, &d, 0, t0);
        assert_eq!(plans.redeem(&a, &d, t0 + PLAN_TTL, |_| None).unwrap_err(), PLAN_EXPIRED);
        let b = issued(&mut plans, &d, 0, t0);
        assert!(plans.redeem(&b, &d, t0 + PLAN_TTL - Duration::from_secs(1), |_| None).is_ok());
    }

    #[test]
    fn any_identity_difference_is_refused_and_burns_the_token() {
        let t0 = Instant::now();
        let d = drive();
        let changes: Vec<fn(&mut Drive)> = vec![
            |x| x.id = "/org/other".into(),
            |x| x.device = "/dev/sdq1".into(),
            |x| x.identity.key = "SER999999".into(),
            |x| x.identity.wwn = "0x5000".into(),
            |x| x.identity.model = "Other".into(),
            |x| x.identity.disk_size += 512,
        ];
        for change in changes {
            let mut plans = Plans::default();
            let tok = issued(&mut plans, &d, 0, t0);
            let mut now = d.clone();
            change(&mut now);
            assert_eq!(plans.redeem(&tok, &now, t0, |_| None).unwrap_err(), DRIVE_CHANGED);
            assert_eq!(plans.redeem(&tok, &d, t0, |_| None).unwrap_err(), PLAN_UNKNOWN);
        }
    }

    #[test]
    fn free_space_is_checked_again() {
        let (mut plans, d, t0) = (Plans::default(), drive(), Instant::now());
        let a = issued(&mut plans, &d, 1000, t0);
        assert_eq!(plans.redeem(&a, &d, t0, |_| Some(999)).unwrap_err(), SPACE_CHANGED);
        let b = issued(&mut plans, &d, 1000, t0);
        assert!(plans.redeem(&b, &d, t0, |_| None).is_ok(), "unknown free space is not a refusal");
    }

    #[test]
    fn ram_is_volatile_and_disk_is_not() {
        if Path::new("/dev/shm").exists() {
            assert!(is_volatile(Path::new("/dev/shm/flashcp/optimize/x")));
        }
    }

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
        // Same filesystem; other processes may write in between, so allow some drift.
        let deep_free = free_bytes(&deep).expect("measured at the ancestor");
        assert!(deep_free.abs_diff(from_ancestor) < 64 * 1024 * 1024, "{deep_free} vs {from_ancestor}");
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
