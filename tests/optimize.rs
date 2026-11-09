//! Integration tests for `flashcp optimize` (image files only, no root).
//!
//! The "stick" is what a normal user ends up with: mkfs.exfat with 32 KB clusters, filled by
//! two separate `flashcp add` runs into the same directories, so files of one directory are
//! interleaved with the other batch and nothing is in the flashcp layout. After optimize the
//! volume must hold exactly the same tree (contents, empty dirs, mtimes, label), pass
//! `fsck.exfat -n`, use the optimizer's cluster size, and be "already optimized" for a second
//! run. The crash tests stop (with an error, or abort() as if killed) at every phase via the
//! FLASHCP_TEST_OPTIMIZE_STOP / FLASHCP_TEST_BUILD_ABORT_AT hooks, check that the data survives
//! (image untouched before the build; verified temp copy present after), then re-run optimize
//! without hooks and require the original content.
//!
//! Every run gets its own FLASHCP_TMPDIR / XDG_CACHE_HOME / TMPDIR, so nothing touches $HOME.

#[allow(dead_code)]
mod common;

use common::exfat_reader::{self, Volume};
use common::{MB, XorShift, assert_success, flashcp, fsck_clean, make_image, make_tricky_tree, test_dir, write_file};
use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LABEL: &str = "MY STICK";
const OLD_CLUSTER: u64 = 32768;

type Snapshot = BTreeMap<String, (bool, Vec<u8>, (i64, u32))>;

struct Stick {
    dir: PathBuf,
    img: PathBuf,
    /// FLASHCP_TMPDIR: where optimize puts its temp dir.
    base: PathBuf,
    orig: Snapshot,
}

fn snapshot(v: &Volume) -> Snapshot {
    v.tree.iter().map(|(k, n)| (k.clone(), (n.is_dir, n.bytes.clone(), n.mtime))).collect()
}

fn read_ok(img: &Path) -> Volume {
    exfat_reader::read_volume(img).unwrap_or_else(|e| panic!("read_volume({}) failed: {e}", img.display()))
}

fn write_random(path: &Path, bytes: u64, seed: u64) {
    let mut buf = vec![0u8; bytes as usize];
    XorShift(seed | 1).fill(&mut buf);
    write_file(path, &buf);
}

fn digest(path: &Path) -> (u64, u64) {
    let mut f = fs::File::open(path).expect("open image");
    let mut h = DefaultHasher::new();
    let mut buf = vec![0u8; MB as usize];
    let mut len = 0u64;
    loop {
        let n = f.read(&mut buf).expect("read image");
        if n == 0 {
            break;
        }
        h.write(&buf[..n]);
        len += n as u64;
    }
    (len, h.finish())
}

/// Gives the owner write permission on everything under `p` (optimize leaves its temp copy
/// read-only), then removes it.
fn remove_all(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fn walk(p: &Path) {
        let Ok(md) = fs::symlink_metadata(p) else { return };
        if md.is_dir() {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path());
            }
        }
    }
    walk(p);
    fs::remove_dir_all(p).unwrap_or_else(|e| panic!("remove {}: {e}", p.display()));
}

/// Flips (xor 0x5A) the first byte of a file of the read-only temp copy, keeping its mode and
/// mtime: what the FLASHCP_TEST_OPTIMIZE_TAMPER hook does, so calling it again restores it.
fn flip_first_byte(p: &Path) {
    use std::os::unix::fs::{FileExt, PermissionsExt};
    let md = fs::metadata(p).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o600)).unwrap();
    let f = fs::OpenOptions::new().read(true).write(true).open(p).unwrap();
    let mut b = [0u8];
    f.read_exact_at(&mut b, 0).unwrap();
    f.write_all_at(&[b[0] ^ 0x5A], 0).unwrap();
    f.set_modified(md.modified().unwrap()).unwrap();
    drop(f);
    fs::set_permissions(p, md.permissions()).unwrap();
}

fn run_ok(cmd: &mut Command, what: &str) {
    assert_success(&cmd.output().unwrap_or_else(|e| panic!("run {what}: {e}")), what);
}

/// A fragmented, mkfs-formatted stick (see the module docs).
fn stick(name: &str) -> Stick {
    let dir = test_dir(name);
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    run_ok(Command::new("mkfs.exfat").args(["-c", "32768", "-L", LABEL]).arg(&img), "mkfs.exfat");
    let a = dir.join("batch-a");
    make_tricky_tree(&a);
    // A kernel-style 32 KB volume filled file by file: add must not turn into a build here.
    run_ok(
        flashcp().arg("add").arg(&a).arg(&img).args(["--to", "docs"]).env("FLASHCP_TEST_NO_ADD_BUILD", "1"),
        "flashcp add (batch a)",
    );
    let b = dir.join("batch-b");
    for i in 0..40u64 {
        write_random(&b.join(format!("bulk/d{}/late-{i:02}.bin", i % 4)), 3000 + i * 517, i + 7);
    }
    write_random(&b.join("bulk/d1/large-late.bin"), 700_000, 99);
    write_file(&b.join("notes/empty-late.txt"), b"");
    run_ok(flashcp().arg("add").arg(&b).arg(&img).args(["--to", "docs"]), "flashcp add (batch b)");
    fsck_clean(&img);
    let v = read_ok(&img);
    assert_eq!(v.info.cluster_size, OLD_CLUSTER);
    assert_eq!(v.label, LABEL);
    let base = dir.join("tmpbase");
    fs::create_dir_all(&base).unwrap();
    for d in ["cache", "tmp"] {
        fs::create_dir_all(dir.join(d)).unwrap();
    }
    Stick { orig: snapshot(&v), dir, img, base }
}

fn optimize(s: &Stick, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = flashcp();
    c.arg("optimize")
        .arg(&s.img)
        .args(args)
        .env("FLASHCP_TMPDIR", &s.base)
        .env("XDG_CACHE_HOME", s.dir.join("cache"))
        .env("TMPDIR", s.dir.join("tmp"));
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("run flashcp optimize")
}

fn optimize_ok(s: &Stick, args: &[&str]) -> Output {
    let out = optimize(s, args, &[]);
    assert_success(&out, "flashcp optimize");
    out
}

/// Must fail; returns stderr (+ stdout, for JSON errors).
fn optimize_fails(s: &Stick, args: &[&str], env: &[(&str, &str)]) -> String {
    let out = optimize(s, args, env);
    assert!(!out.status.success(), "flashcp optimize unexpectedly succeeded\n{}", String::from_utf8_lossy(&out.stdout));
    format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout))
}

fn temp_dirs(s: &Stick) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(&s.base)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("flashcp-optimize-"))
        .collect();
    v.sort();
    v
}

fn the_temp_dir(s: &Stick) -> PathBuf {
    let t = temp_dirs(s);
    assert_eq!(t.len(), 1, "expected exactly one temp dir, found {t:?}");
    t.into_iter().next().unwrap()
}

fn state_of(tmp: &Path) -> String {
    let s = fs::read_to_string(tmp.join("state")).expect("read state file");
    s.lines().find_map(|l| l.strip_prefix("state=")).expect("state line").to_string()
}

fn same_tree(s: &Stick, v: &Volume, what: &str) {
    let now = snapshot(v);
    let mut problems = Vec::new();
    for (k, a) in &s.orig {
        match now.get(k) {
            None => problems.push(format!("missing: {k}")),
            Some(b) if b != a => problems.push(format!(
                "differs: {k} (dir {} / {}, {} / {} bytes, mtime {:?} / {:?})",
                a.0,
                b.0,
                a.1.len(),
                b.1.len(),
                a.2,
                b.2
            )),
            _ => {}
        }
    }
    problems.extend(now.keys().filter(|k| !s.orig.contains_key(*k)).map(|k| format!("extra: {k}")));
    assert!(problems.is_empty(), "{what}: tree differs from the original:\n{}", problems.join("\n"));
}

/// The full post-condition of a successful optimize.
fn assert_optimized(s: &Stick) {
    fsck_clean(&s.img);
    let v = read_ok(&s.img);
    same_tree(s, &v, "after optimize");
    assert_eq!(v.label, LABEL, "label not preserved");
    assert_ne!(v.info.cluster_size, OLD_CLUSTER, "cluster size did not change");
    for (k, n) in &v.tree {
        assert!(n.is_dir || n.bytes.is_empty() || n.no_fat_chain, "{k} is not contiguous");
    }
    assert!(temp_dirs(s).is_empty(), "temp dir left behind: {:?}", temp_dirs(s));
    // The optimizer's choice: a second run finds nothing to do.
    let out = optimize_ok(s, &["--dry-run", "--json"]);
    let plan = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_string();
    assert!(plan.contains(r#""write_bytes":0"#) && plan.contains(r#""already_optimized":true"#), "not optimal: {plan}");
}

// ───────────────────────── the happy path ─────────────────────────

#[test]
fn round_trip_keeps_everything_and_changes_the_layout() {
    let s = stick("opt_round_trip");
    let out = optimize_ok(&s, &["--yes"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("[1/5] pull") && text.contains("[5/5] cleanup"), "phases not printed:\n{text}");
    assert!(text.contains("optimized:"), "no summary:\n{text}");
    assert_optimized(&s);
    let before = digest(&s.img);
    let again = optimize_ok(&s, &["--yes"]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("already optimized"), "second run rebuilt");
    assert_eq!(digest(&s.img), before, "an already optimized volume was written");
    // --force rebuilds anyway, and the result is still right.
    optimize_ok(&s, &["--yes", "--force"]);
    assert_optimized(&s);
    remove_all(&s.dir);
}

#[test]
fn json_event_sequence() {
    let s = stick("opt_json");
    let out = optimize_ok(&s, &["--yes", "--json"]);
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    assert!(lines.iter().all(|l| l.starts_with(r#"{"event":""#) && l.ends_with('}')), "{lines:?}");
    let plan = &lines[0];
    assert!(plan.starts_with(r#"{"event":"plan","op":"optimize""#), "{plan}");
    let files = s.orig.values().filter(|n| !n.0).count();
    let bytes: usize = s.orig.values().map(|n| n.1.len()).sum();
    assert!(plan.contains(&format!(r#""files":{files},"#)) && plan.contains(&format!(r#""bytes":{bytes},"#)), "{plan}");
    assert!(plan.contains(r#""strategy":{"temp":"#), "{plan}");
    let phases: Vec<&String> = lines.iter().filter(|l| l.contains(r#""event":"phase""#)).collect();
    let names = ["pull", "verify", "build", "check", "cleanup"];
    assert_eq!(phases.len(), 5, "{phases:?}");
    for (i, (p, n)) in phases.iter().zip(names).enumerate() {
        let want = format!(r#"{{"event":"phase","op":"optimize","phase":"{n}","step":{},"steps":5}}"#, i + 1);
        assert_eq!(p.as_str(), want);
    }
    // Only phases and progress between plan and done; the inner pull/build plan and done are swallowed.
    for l in &lines[1..lines.len() - 1] {
        assert!(l.contains(r#""event":"phase""#) || l.contains(r#""event":"progress""#), "unexpected: {l}");
    }
    let done = lines.last().unwrap();
    assert!(done.starts_with(&format!(r#"{{"event":"done","files":{files},"bytes":{bytes},"#)), "{done}");
    assert_eq!(lines.iter().filter(|l| l.contains(r#""event":"plan""#)).count(), 1);
    assert_optimized(&s);
    remove_all(&s.dir);
}

// ───────────────────────── refusals: nothing written ─────────────────────────

#[test]
fn dry_run_and_missing_yes_write_nothing() {
    let s = stick("opt_dry_run");
    let before = digest(&s.img);
    let out = optimize_ok(&s, &["--dry-run"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("optimize:") && text.contains("temporary copy in"), "{text}");
    assert!(text.contains("Windows") && text.contains("FORMAT") && text.contains("re-run `flashcp optimize"), "{text}");
    assert_eq!(digest(&s.img), before, "--dry-run changed the image");
    assert!(temp_dirs(&s).is_empty(), "--dry-run created a temp dir");
    let e = optimize_fails(&s, &[], &[]);
    assert!(e.contains("--yes") && e.contains("nothing was changed") && e.contains("FORMAT"), "{e}");
    assert_eq!(digest(&s.img), before);
    remove_all(&s.dir);
}

#[test]
fn refuses_without_temp_space() {
    let s = stick("opt_no_space");
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_FAKE_FREE", "1000")]);
    assert!(e.contains("no place for the temporary copy") && e.contains("has only"), "{e}");
    assert!(e.contains("nothing was changed"), "{e}");
    assert_eq!(digest(&s.img), before, "image changed");
    assert!(temp_dirs(&s).is_empty());
    remove_all(&s.dir);
}

#[test]
fn refuses_tmpfs_unless_given_with_temp() {
    let shm = Path::new("/dev/shm");
    let is_tmpfs =
        Command::new("stat").args(["-f", "-c", "%T"]).arg(shm).output().is_ok_and(|o| o.stdout.starts_with(b"tmpfs"));
    if !is_tmpfs {
        eprintln!("SKIP: /dev/shm is not a tmpfs here");
        return;
    }
    let s = stick("opt_tmpfs");
    let ram = shm.join(format!("flashcp-opt-test-{}", std::process::id()));
    fs::create_dir_all(&ram).unwrap();
    let before = digest(&s.img);
    // tmpfs first in line; every other candidate "full", so nothing qualifies.
    let mut c = flashcp();
    c.arg("optimize").arg(&s.img).arg("--yes").env("FLASHCP_TMPDIR", &ram).env("FLASHCP_TEST_FAKE_FREE", "0");
    c.env("XDG_CACHE_HOME", s.dir.join("cache")).env("TMPDIR", s.dir.join("tmp"));
    let out = c.output().unwrap();
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "accepted tmpfs");
    assert!(e.contains(&format!("{} is in RAM (tmpfs)", ram.display())), "{e}");
    assert_eq!(digest(&s.img), before);
    assert!(e.contains("--temp-volatile-i-accept-data-loss"), "{e}");
    // Explicitly given, a dry run shows the plan (with a warning that a real run refuses it).
    let r = ram.to_str().unwrap();
    let out = optimize(&s, &["--dry-run", "--temp", r], &[]);
    assert_success(&out, "optimize --temp tmpfs --dry-run");
    let warn = String::from_utf8_lossy(&out.stderr);
    assert!(warn.contains("warning") && warn.contains("refuses it without"), "no tmpfs warning: {warn}");
    assert!(String::from_utf8_lossy(&out.stdout).contains(&ram.display().to_string()));
    // A real run with an explicit tmpfs --temp is refused: a crash would lose the data.
    let e = optimize_fails(&s, &["--yes", "--temp", r], &[]);
    assert!(e.contains("is in RAM (tmpfs)") && e.contains("lose this copy"), "{e}");
    assert!(e.contains("--temp-volatile-i-accept-data-loss") && e.contains("nothing was changed"), "{e}");
    assert_eq!(digest(&s.img), before);
    // The flag needs --temp.
    let e = optimize_fails(&s, &["--yes", "--temp-volatile-i-accept-data-loss"], &[]);
    assert!(e.contains("--temp"), "{e}");
    assert_eq!(digest(&s.img), before);
    // With the flag it is used, with the warning.
    let out = optimize(&s, &["--yes", "--temp", r, "--temp-volatile-i-accept-data-loss"], &[]);
    assert_success(&out, "optimize --temp tmpfs --temp-volatile-i-accept-data-loss");
    assert!(String::from_utf8_lossy(&out.stderr).contains("are lost"), "no tmpfs warning");
    assert!(fs::read_dir(&ram).unwrap().next().is_none(), "temp copy left in {}", ram.display());
    fsck_clean(&s.img);
    same_tree(&s, &read_ok(&s.img), "after optimize via tmpfs");
    fs::remove_dir_all(&ram).unwrap();
    remove_all(&s.dir);
}

#[test]
fn corrupted_temp_copy_is_caught_by_verify() {
    let s = stick("opt_corrupt");
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_CORRUPT", "docs/random-5mb.bin")]);
    assert!(e.contains("does not match the drive") && e.contains("docs/random-5mb.bin"), "{e}");
    assert!(e.contains("untouched"), "{e}");
    assert_eq!(digest(&s.img), before, "image changed");
    assert!(temp_dirs(&s).is_empty(), "temp copy kept");
    remove_all(&s.dir);
}

// ───────────────────────── crashes at every phase ─────────────────────────

/// Stops at `point` (error, or abort() like kill -9), checks what survives, re-runs optimize
/// without hooks and requires the original content.
fn crash_then_resume(name: &str, env: &[(&str, &str)], point: &str, aborted: bool) {
    let s = stick(name);
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], env);
    let image_untouched = digest(&s.img) == before;
    match point {
        "pull" | "verify" => {
            assert!(image_untouched, "{point}: image changed before the build");
            if aborted {
                let t = the_temp_dir(&s);
                assert_eq!(state_of(&t), if point == "pull" { "pulled" } else { "verified" });
            } else {
                assert!(e.contains("untouched"), "{e}");
                assert!(temp_dirs(&s).is_empty(), "{point}: temp dir kept after a clean failure");
            }
        }
        "building" | "build" | "mid-build" => {
            if point == "building" {
                assert!(image_untouched, "image changed before the build started");
            } else {
                assert!(!image_untouched, "{point}: the build did not write");
            }
            let t = the_temp_dir(&s);
            assert_eq!(state_of(&t), "building");
            assert!(t.join("manifest").is_file() && t.join("tree/docs/random-5mb.bin").is_file(), "copy missing");
            if !aborted {
                assert!(e.contains("all your files are safe in") && e.contains("--temp"), "{e}");
            }
        }
        "cleanup" => {
            assert!(!image_untouched);
            assert_eq!(state_of(&the_temp_dir(&s)), "done");
            fsck_clean(&s.img);
            same_tree(&s, &read_ok(&s.img), "after a crash in cleanup");
        }
        _ => unreachable!(),
    }
    let out = optimize_ok(&s, &["--yes"]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if matches!(point, "building" | "build" | "mid-build") {
        assert!(text.contains("interrupted optimize"), "resume not reported:\n{text}");
    }
    assert_optimized(&s);
    remove_all(&s.dir);
}

macro_rules! crash_tests {
    ($($name:ident: $point:literal, $abort:literal;)*) => {$(
        #[test]
        fn $name() {
            let mut env = vec![("FLASHCP_TEST_OPTIMIZE_STOP", $point)];
            if $abort {
                env.push(("FLASHCP_TEST_OPTIMIZE_ABORT", "1"));
            }
            crash_then_resume(stringify!($name), &env, $point, $abort);
        }
    )*};
}

crash_tests! {
    opt_fail_after_pull: "pull", false;
    opt_abort_after_pull: "pull", true;
    opt_fail_after_verify: "verify", false;
    opt_abort_after_verify: "verify", true;
    opt_fail_after_state_building: "building", false;
    opt_abort_after_state_building: "building", true;
    opt_fail_after_build: "build", false;
    opt_abort_after_build: "build", true;
    opt_abort_during_cleanup: "cleanup", true;
    opt_fail_during_cleanup: "cleanup", false;
}

#[test]
fn opt_abort_mid_build() {
    crash_then_resume("opt_abort_mid_build", &[("FLASHCP_TEST_BUILD_ABORT_AT", "3000000")], "mid-build", true);
}

// ───────────────────────── resume refusals ─────────────────────────

/// Crashed with state=building but the drive was changed since: never erased automatically.
#[test]
fn resume_refuses_a_drive_written_since() {
    let s = stick("opt_resume_foreign");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "building"), ("FLASHCP_TEST_OPTIMIZE_ABORT", "1")]);
    let t = the_temp_dir(&s);
    // The user adds a file to the (still original, clean) volume.
    let extra = s.dir.join("extra");
    write_file(&extra.join("new.txt"), b"added after the crash\n");
    run_ok(flashcp().arg("add").arg(&extra).arg(&s.img), "flashcp add");
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("will not erase it") && e.contains(&t.display().to_string()), "{e}");
    // The drive holds the newer data: keep it; never suggest writing the stale copy over it.
    assert!(e.contains("newer data") && e.contains("keep the drive") && e.contains("stale"), "{e}");
    assert!(!e.contains("flashcp build") && !e.contains("otherwise delete"), "{e}");
    assert_eq!(digest(&s.img), before, "a foreign volume was written");
    assert_eq!(the_temp_dir(&s), t, "temp copy removed");
    remove_all(&s.dir);
}

/// Sets VolumeDirty in the main boot sector (as a camera or Windows does while mounted, and
/// leaves when the stick is pulled out).
fn set_dirty(img: &Path) {
    let mut b = fs::read(img).unwrap();
    b[106] |= 2;
    fs::write(img, b).unwrap();
}

fn serial_in(img: &Path) -> u32 {
    let b = fs::read(img).unwrap();
    le32(&b, 100)
}

fn state_field(tmp: &Path, key: &str) -> String {
    let s = fs::read_to_string(tmp.join("state")).unwrap();
    s.lines().find_map(|l| l.strip_prefix(&format!("{key}="))).unwrap_or_else(|| panic!("no {key} line")).to_string()
}

/// Replaces the `key=` line of the state file.
fn set_state_field(tmp: &Path, key: &str, value: &str) {
    let s = fs::read_to_string(tmp.join("state")).unwrap();
    let out: Vec<String> = s
        .lines()
        .map(|l| if l.starts_with(&format!("{key}=")) { format!("{key}={value}") } else { l.to_string() })
        .collect();
    fs::write(tmp.join("state"), out.join("\n") + "\n").unwrap();
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// An aborted mid-build: the drive holds our half-written volume (main boot region zeros, the
/// backup with our serial, job id and geometry).
fn half_written(name: &str) -> (Stick, PathBuf) {
    let s = stick(name);
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_BUILD_ABORT_AT", "3000000")]);
    let t = the_temp_dir(&s);
    assert_eq!(state_of(&t), "building");
    (s, t)
}

/// A refusal of a half-written drive: nothing written, the copy kept as it was.
fn refused(s: &Stick, t: &Path, want: &[&str]) -> String {
    let before = digest(&s.img);
    let e = optimize_fails(s, &["--yes"], &[]);
    for w in want {
        assert!(e.contains(w), "missing {w:?} in: {e}");
    }
    assert!(e.contains(&t.display().to_string()), "{e}");
    assert_eq!(digest(&s.img), before, "a drive that is not this optimize's was written");
    assert_eq!(the_temp_dir(s), *t, "temp copy removed");
    assert_eq!(state_of(t), "building");
    assert!(t.join("tree/docs/random-5mb.bin").is_file(), "copy removed");
    e
}

/// Our serial in the backup boot sector, but another job id (a reused identity and a foreign
/// volume with that serial): refused; with the right id it resumes.
#[test]
fn resume_refuses_our_serial_with_another_job_id() {
    let (s, t) = half_written("opt_resume_other_job");
    let job = state_field(&t, "job");
    set_state_field(&t, "job", &"5a".repeat(16));
    refused(&s, &t, &["serial number", "another optimize job", "will not write"]);
    set_state_field(&t, "job", &job);
    optimize_ok(&s, &["--yes"]);
    assert_optimized(&s);
    remove_all(&s.dir);
}

/// Our serial and job id, but the backup's geometry is not what this optimize planned.
#[test]
fn resume_refuses_our_job_id_with_another_geometry() {
    let (s, t) = half_written("opt_resume_other_geometry");
    let mut b = fs::read(&s.img).unwrap();
    let fat_len = le32(&b, 12 * 512 + 84);
    b[12 * 512 + 84..12 * 512 + 88].copy_from_slice(&(fat_len + 1).to_le_bytes());
    fs::write(&s.img, &b).unwrap();
    let e = refused(&s, &t, &["geometry differs", "FatLength", "will not write"]);
    assert!(!e.contains("another optimize job"), "{e}");
    remove_all(&s.dir);
}

/// The temp dir found by the drive's identity, but the state was saved for another drive
/// (another identity, or another size): refused before anything is read as ours.
#[test]
fn resume_refuses_another_device_identity_or_size() {
    let (s, t) = half_written("opt_resume_other_device");
    let dev = state_field(&t, "device");
    set_state_field(&t, "device", &hex(b"img-1-2"));
    refused(&s, &t, &["not the drive it was made from", "img-1-2", "Nothing was changed"]);
    set_state_field(&t, "device", &dev);
    // The same image file (same inode), one MB larger.
    let f = fs::OpenOptions::new().write(true).open(&s.img).unwrap();
    f.set_len(fs::metadata(&s.img).unwrap().len() + MB).unwrap();
    drop(f);
    refused(&s, &t, &["not the drive it was made from", "size"]);
    remove_all(&s.dir);
}

/// After the drive may have been written, the JSON error carries "recovery" with the FORMAT
/// warning; before that, it does not.
#[test]
fn json_error_carries_recovery_once_the_drive_was_written() {
    let s = stick("opt_json_recovery");
    let out = optimize(&s, &["--yes", "--json"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "pull")]);
    let err = String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().to_string();
    assert!(err.starts_with(r#"{"event":"error","message":"#) && !err.contains(r#""recovery""#), "{err}");
    let out = optimize(&s, &["--yes", "--json"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "build")]);
    let err = String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().to_string();
    assert!(err.starts_with(r#"{"event":"error","message":"#) && err.contains(r#","recovery":""#), "{err}");
    let rec = &err[err.find(r#""recovery""#).unwrap()..];
    assert!(rec.contains("Windows") && rec.contains("FORMAT") && rec.contains("flashcp optimize"), "{rec}");
    // The human error says it too (our volume, made dirty: this run rebuilds it and stops
    // after the build again).
    set_dirty(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "build")]);
    assert!(e.contains("all your files are safe in") && e.contains("Windows") && e.contains("FORMAT"), "{e}");
    optimize_ok(&s, &["--yes"]);
    assert_optimized(&s);
    remove_all(&s.dir);
}

fn saved_serial(tmp: &Path) -> u32 {
    let s = fs::read_to_string(tmp.join("state")).unwrap();
    u32::from_str_radix(s.lines().find_map(|l| l.strip_prefix("serial=")).expect("serial line"), 16).unwrap()
}

/// The build never started (state=building), then the original volume was mounted, written
/// and pulled out (dirty): the drive may hold newer files than the copy, so it is not rebuilt.
#[test]
fn resume_refuses_the_original_volume_left_dirty() {
    let s = stick("opt_resume_orig_dirty");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "building")]);
    let t = the_temp_dir(&s);
    set_dirty(&s.img);
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("original volume") && e.contains("not clean") && e.contains("newer data"), "{e}");
    assert!(!e.contains("flashcp build"), "{e}");
    assert_eq!(digest(&s.img), before, "a dirty original volume was overwritten");
    assert_eq!(the_temp_dir(&s), t, "temp copy removed");
    remove_all(&s.dir);
}

/// state=building, then the drive was reformatted (another exFAT serial) or overwritten with
/// something else (no exFAT at all): neither is ours, neither is erased.
#[test]
fn resume_refuses_a_reformatted_or_foreign_drive() {
    let s = stick("opt_resume_reformatted");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "building"), ("FLASHCP_TEST_OPTIMIZE_ABORT", "1")]);
    let t = the_temp_dir(&s);
    run_ok(Command::new("mkfs.exfat").args(["-L", "OTHER"]).arg(&s.img), "mkfs.exfat");
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("neither the original volume") && e.contains(&t.display().to_string()), "{e}");
    assert_eq!(digest(&s.img), before, "a reformatted drive was written");
    // Not a filesystem flashcp knows (e.g. reformatted as FAT32 / NTFS, here: zeros).
    let mut b = fs::read(&s.img).unwrap();
    b[..MB as usize].fill(0);
    b[3..11].copy_from_slice(b"NTFS    ");
    fs::write(&s.img, &b).unwrap();
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("neither the original volume"), "{e}");
    assert_eq!(digest(&s.img), before, "a foreign drive was written");
    assert_eq!(the_temp_dir(&s), t, "temp copy removed");
    assert_eq!(state_of(&t), "building");
    remove_all(&s.dir);
}

/// The build finished (clean, our serial) but the check never ran; then files were added. Our
/// volume, but with other content: refused, not rebuilt.
#[test]
fn resume_refuses_our_finished_volume_written_since() {
    let s = stick("opt_resume_ours_written");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "build"), ("FLASHCP_TEST_OPTIMIZE_ABORT", "1")]);
    let t = the_temp_dir(&s);
    assert_eq!(serial_in(&s.img), saved_serial(&t), "the build did not write the serial saved in the state");
    let extra = s.dir.join("extra");
    write_file(&extra.join("new.txt"), b"added after the crash\n");
    run_ok(flashcp().arg("add").arg(&extra).arg(&s.img), "flashcp add");
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("the interrupted optimize built") && e.contains("will not erase it"), "{e}");
    assert_eq!(digest(&s.img), before, "a volume written since was overwritten");
    assert_eq!(the_temp_dir(&s), t);
    remove_all(&s.dir);
}

/// Our serial and dirty (a half-written build, or ours left dirty): rebuilt from the copy.
#[test]
fn resume_rebuilds_our_dirty_volume() {
    let s = stick("opt_resume_ours_dirty");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "build")]);
    let t = the_temp_dir(&s);
    assert_eq!(serial_in(&s.img), saved_serial(&t));
    set_dirty(&s.img);
    let out = optimize_ok(&s, &["--yes"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unfinished rebuild"), "not rebuilt");
    assert_optimized(&s);
    remove_all(&s.dir);
}

/// A temp dir locked by another run is neither resumed nor removed.
#[test]
fn resume_refuses_a_temp_dir_locked_by_another_run() {
    let s = stick("opt_resume_locked");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "building")]);
    let t = the_temp_dir(&s);
    let lock = fs::File::open(t.join("lock")).expect("lock file");
    lock.try_lock().expect("take the lock");
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("in use by another") && e.contains("nothing was changed"), "{e}");
    assert_eq!(digest(&s.img), before);
    assert_eq!(state_of(&t), "building");
    drop(lock);
    optimize_ok(&s, &["--yes"]);
    assert_optimized(&s);
    remove_all(&s.dir);
}

/// An image attached to a loop device between phases (losetup ignores our flock): stopped
/// before the build and before the check, with the copy kept and named.
#[test]
fn loop_device_attached_between_phases_stops_the_run() {
    for at in ["build", "check"] {
        let s = stick(&format!("opt_loop_{at}"));
        let before = digest(&s.img);
        let e = optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_LOOP", at)]);
        assert!(e.contains("loop device") && e.contains("all your files are safe in"), "{at}: {e}");
        assert_eq!(digest(&s.img) == before, at == "build", "{at}: wrong write state");
        assert_eq!(state_of(&the_temp_dir(&s)), "building");
        optimize_ok(&s, &["--yes"]);
        assert_optimized(&s);
        remove_all(&s.dir);
    }
}

#[test]
fn resume_refuses_a_missing_manifest() {
    let s = stick("opt_resume_no_manifest");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_BUILD_ABORT_AT", "3000000")]);
    let t = the_temp_dir(&s);
    fs::remove_file(t.join("manifest")).unwrap();
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("manifest") && e.contains("missing or damaged") && e.contains("flashcp build"), "{e}");
    assert_eq!(digest(&s.img), before);
    assert!(t.join("tree").is_dir(), "copy removed");
    remove_all(&s.dir);
}

#[test]
fn resume_refuses_a_temp_copy_damaged_after_the_crash() {
    let s = stick("opt_resume_damaged");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_BUILD_ABORT_AT", "3000000")]);
    let t = the_temp_dir(&s);
    flip_first_byte(&t.join("tree/docs/café.txt"));
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("no longer matches its manifest") && e.contains("docs/café.txt"), "{e}");
    assert_eq!(digest(&s.img), before, "a damaged copy was written");
    assert!(t.join("tree").is_dir());
    remove_all(&s.dir);
}

// ───────────────────────── chains out of physical order, ValidDataLength ─────────────────────────

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// The entry set whose name is `name`: edited by `f`, then its SetChecksum recomputed.
fn patch_set(img: &mut [u8], name: &str, f: impl FnOnce(&mut [u8])) {
    let needle: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let hits: Vec<usize> =
        img.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect();
    assert_eq!(hits.len(), 1, "expected one occurrence of {name}");
    let set = hits[0] - 66;
    assert!(img[set] == 0x85 && img[set + 32] == 0xC0, "{name}: not an entry set");
    let len = 32 * (1 + img[set + 1] as usize);
    f(&mut img[set..set + len]);
    let mut c: u16 = 0;
    for (i, &b) in img[set..set + len].iter().enumerate() {
        if i != 2 && i != 3 {
            c = c.rotate_right(1).wrapping_add(b as u16);
        }
    }
    img[set + 2..set + 4].copy_from_slice(&c.to_le_bytes());
}

/// A kernel-style image: a file whose FAT chain visits its clusters out of physical order
/// (c, c+2, c+1), so its logical blocks lie on the device in another order than in the file,
/// a file whose ValidDataLength is half its DataLength (the rest must read as zeros, whatever
/// the disk holds), and empty files.
fn crafted_stick(name: &str) -> Stick {
    let dir = test_dir(name);
    let src = dir.join("src");
    write_random(&src.join("frag.bin"), 3 * OLD_CLUSTER, 11);
    write_random(&src.join("vdl.bin"), 10_000, 12);
    write_random(&src.join("after.bin"), 50_000, 13);
    write_file(&src.join("empty.bin"), b"");
    write_file(&src.join("sub/empty-too.bin"), b"");
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    assert_success(&common::run_build(&src, &img, &["--cluster", "32768", "--label", LABEL]), "flashcp build");
    let mut b = fs::read(&img).unwrap();
    let bps = 1usize << b[108];
    let fat = le32(&b, 80) as usize * bps;
    let mut first = 0;
    patch_set(&mut b, "frag.bin", |s| {
        s[33] &= !0x02; // NoFatChain off
        first = le32(s, 52) as usize;
    });
    let entry = |c: usize| fat + 4 * c;
    b[entry(first)..entry(first) + 4].copy_from_slice(&((first + 2) as u32).to_le_bytes());
    b[entry(first + 2)..entry(first + 2) + 4].copy_from_slice(&((first + 1) as u32).to_le_bytes());
    b[entry(first + 1)..entry(first + 1) + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&img, &b).unwrap();
    // (The strict reader insists on ValidDataLength == DataLength, so snapshot first.)
    let mut orig = snapshot(&read_ok(&img));
    orig.get_mut("vdl.bin").unwrap().1[5000..].fill(0);
    patch_set(&mut b, "vdl.bin", |s| s[40..48].copy_from_slice(&5000u64.to_le_bytes()));
    fs::write(&img, &b).unwrap();
    fsck_clean(&img);
    assert_ne!(orig["frag.bin"].1, fs::read(src.join("frag.bin")).unwrap(), "the chain patch did not reorder the file");
    for d in ["tmpbase", "cache", "tmp"] {
        fs::create_dir_all(dir.join(d)).unwrap();
    }
    Stick { base: dir.join("tmpbase"), dir, img, orig }
}

#[test]
fn chained_out_of_order_and_valid_data_length_survive() {
    let s = crafted_stick("opt_crafted");
    optimize_ok(&s, &["--yes"]);
    assert_optimized(&s);
    remove_all(&s.dir);
}

// ───────────────────────── the device is read once ─────────────────────────

/// Device data bytes read per optimize phase, from the FLASHCP_TEST_TRACE_READS trace.
fn reads_per_phase(stderr: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let mut phase = "(before)".to_string();
    for l in stderr.lines() {
        if let Some(p) = l.strip_prefix("flashcp-phase ") {
            phase = p.to_string();
            out.entry(phase.clone()).or_insert(0);
        } else if let Some(r) = l.strip_prefix("flashcp-read ") {
            let len: u64 = r.split(' ').nth(1).unwrap().parse().unwrap();
            *out.entry(phase.clone()).or_insert(0) += len;
        }
    }
    out
}

/// Optimizes `s` with the verify fallback forced (the drive's data hashed again, with pull's
/// planner and strategy) and requires that independent manifest to equal the one pull computed
/// while reading (optimize fails otherwise).
fn pull_hash_equals_device_hash(s: &Stick, args: &[&str]) {
    let mut a = vec!["--yes"];
    a.extend_from_slice(args);
    let out = optimize(s, &a, &[("FLASHCP_TEST_OPTIMIZE_VERIFY_DEVICE", "1"), ("FLASHCP_TEST_TRACE_READS", "1")]);
    assert_success(&out, "flashcp optimize (device re-verified)");
    let r = reads_per_phase(&String::from_utf8_lossy(&out.stderr));
    assert!(r["verify"] > 0, "the forced fallback did not read the drive: {r:?}");
    assert_optimized(s);
}

#[test]
fn pull_stream_manifest_equals_device_manifest_on_a_built_image() {
    let dir = test_dir("opt_hash_built");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    assert_success(&common::run_build(&src, &img, &["--cluster", "32768", "--label", LABEL]), "flashcp build");
    for d in ["tmpbase", "cache", "tmp"] {
        fs::create_dir_all(dir.join(d)).unwrap();
    }
    let s = Stick { orig: snapshot(&read_ok(&img)), base: dir.join("tmpbase"), dir, img };
    pull_hash_equals_device_hash(&s, &[]);
    remove_all(&s.dir);
}

#[test]
fn pull_stream_manifest_equals_device_manifest_on_a_fragmented_image() {
    let s = crafted_stick("opt_hash_crafted");
    pull_hash_equals_device_hash(&s, &[]);
    remove_all(&s.dir);
    let s = stick("opt_hash_stick");
    pull_hash_equals_device_hash(&s, &[]);
    remove_all(&s.dir);
}

#[test]
fn verify_does_not_read_the_drive_again() {
    let s = stick("opt_read_once");
    let out = optimize(&s, &["--yes"], &[("FLASHCP_TEST_TRACE_READS", "1")]);
    assert_success(&out, "flashcp optimize");
    let r = reads_per_phase(&String::from_utf8_lossy(&out.stderr));
    eprintln!("device data bytes read per phase: {r:?}");
    assert_eq!(r.get("(before)"), None, "data read before the pull: {r:?}");
    assert!(r["pull"] > 0 && r["check"] > 0, "{r:?}");
    assert_eq!(r["verify"], 0, "verify read the drive again: {r:?}");
    assert_optimized(&s);
    remove_all(&s.dir);
}

// ───────────────────────── the copy changes after verify ─────────────────────────

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

/// After verify the copy is read-only (files 0400, dirs 0500, the temp dir 0700), it is on disk
/// (syncfs) before the device is first written, and the cleanup still removes it.
#[test]
fn verified_copy_is_read_only_and_synced_before_the_device_is_written() {
    let s = stick("opt_sealed");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "verify")]);
    assert!(temp_dirs(&s).is_empty(), "temp copy kept after a failure before the build");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_STOP", "building")]);
    let t = the_temp_dir(&s);
    assert_eq!(mode(&t), 0o700, "temp dir");
    let (mut files, mut dirs) = (0, 0);
    let mut stack = vec![t.join("tree")];
    while let Some(d) = stack.pop() {
        assert_eq!(mode(&d), 0o500, "{}", d.display());
        dirs += 1;
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p);
            } else {
                assert_eq!(mode(&p), 0o400, "{}", p.display());
                files += 1;
            }
        }
    }
    assert!(files > 100 && dirs > 5, "{files} files, {dirs} dirs");
    // Resume: the temp filesystem is synced before the first device write; cleanup removes the
    // read-only copy.
    let out = optimize(&s, &["--yes"], &[("FLASHCP_TEST_TRACE_READS", "1")]);
    assert_success(&out, "flashcp optimize (resume)");
    let err = String::from_utf8_lossy(&out.stderr);
    let synced = err.find("flashcp-temp-synced").expect("temp fs not synced");
    let written = err.find("flashcp-device-write").expect("no device write traced");
    assert!(synced < written, "device written before the temp copy was synced");
    assert_optimized(&s);
    remove_all(&s.dir);
}

/// The image's main boot region is zeros (an unmountable volume) and the backup carries the
/// serial the interrupted optimize saved, so a re-run knows the drive as its own.
fn assert_unfinished_rebuild(s: &Stick, t: &Path) {
    let b = fs::read(&s.img).unwrap();
    assert!(b[..12 * 512].iter().all(|&x| x == 0), "main boot region written before the end");
    assert_eq!(&b[12 * 512 + 3..12 * 512 + 11], b"EXFAT   ", "no backup boot sector");
    assert_eq!(le32(&b, 12 * 512 + 100), saved_serial(t), "backup boot sector without our serial");
    assert_eq!(&b[12 * 512 + 120..12 * 512 + 131], b"flashcp-job", "backup boot sector without our job tag");
    assert_eq!(hex(&b[12 * 512 + 131..12 * 512 + 147]), state_field(t, "job"), "backup boot sector without our job id");
    assert!(exfat_reader::read_volume(&s.img).is_err(), "an aborted build reads as a volume");
    if let Ok(o) = Command::new("fsck.exfat").arg("-n").arg(&s.img).output() {
        assert!(!o.status.success(), "fsck.exfat accepts an aborted build");
    }
}

/// `tamper` changes a file of the verified copy after verify (contents only: same size, mode
/// and mtime). The build must notice, stop, keep the copy and say how to recover; the drive is
/// left unmountable; a re-run refuses the changed copy; once the file is restored, it finishes.
fn changed_copy_is_caught(name: &str, s: &Stick, tamper: &str, rel: &str) {
    let before = digest(&s.img);
    let e = optimize_fails(s, &["--yes"], &[("FLASHCP_TEST_OPTIMIZE_TAMPER", tamper)]);
    assert!(e.contains("changed after it was verified") && e.contains(rel), "{name}: {e}");
    assert!(e.contains("restore it") && e.contains("re-run `flashcp optimize`"), "{name}: {e}");
    assert!(e.contains("flashcp add") && e.contains("/tree") && e.contains("after formatting"), "{name}: {e}");
    assert_ne!(digest(&s.img), before, "{name}: the build never started");
    let t = the_temp_dir(s);
    assert_eq!(state_of(&t), "building");
    assert!(t.join("manifest").is_file(), "{name}: manifest removed");
    assert_unfinished_rebuild(s, &t);
    // Not restored: refused, nothing written.
    let now = digest(&s.img);
    let e = optimize_fails(s, &["--yes"], &[]);
    assert!(e.contains("no longer matches its manifest") && e.contains(rel), "{name}: {e}");
    assert_eq!(digest(&s.img), now, "{name}: a changed copy was written");
    // Restored: the re-run recognizes its unfinished rebuild and completes it.
    flip_first_byte(&t.join("tree").join(rel));
    let out = optimize_ok(s, &["--yes"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unfinished rebuild"), "{name}: not resumed");
    assert_optimized(s);
}

/// Changed between verify and the build: a small file, read into memory for the build.
#[test]
fn copy_changed_between_verify_and_build_stops_the_build() {
    let s = stick("opt_changed_before_build");
    changed_copy_is_caught("before the build", &s, "docs/café.txt", "docs/café.txt");
    remove_all(&s.dir);
}

/// Changed while the build runs (after the first file went to the device): a file big enough
/// to be streamed from the copy straight into the device buffers.
#[test]
fn copy_changed_during_the_build_stops_the_build() {
    let mut s = stick("opt_changed_mid_build");
    let big = s.dir.join("big");
    write_random(&big.join("zz/zz/streamed.bin"), 20 * MB, 77);
    run_ok(flashcp().arg("add").arg(&big).arg(&s.img), "flashcp add (big)");
    s.orig = snapshot(&read_ok(&s.img));
    changed_copy_is_caught("mid-build", &s, "1:zz/zz/streamed.bin", "zz/zz/streamed.bin");
    remove_all(&s.dir);
}

/// Plain `flashcp build` keeps its behaviour: an interrupted build leaves the main boot sector
/// written, saying VolumeDirty (not zeros), and a finished one is clean.
#[test]
fn plain_build_still_marks_dirty_until_done() {
    let dir = test_dir("opt_plain_build");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    let out = flashcp()
        .arg("build")
        .arg(&src)
        .arg(&img)
        .arg("--yes-erase")
        .env("FLASHCP_TEST_BUILD_ABORT_AT", "3000000")
        .output()
        .unwrap();
    assert!(!out.status.success(), "the abort hook did not fire");
    let b = fs::read(&img).unwrap();
    assert_eq!(&b[3..11], b"EXFAT   ", "main boot sector not written first");
    assert_eq!(b[106] & 2, 2, "an interrupted build is not marked dirty");
    assert_success(&common::run_build(&src, &img, &[]), "flashcp build");
    let b = fs::read(&img).unwrap();
    assert_eq!(b[106] & 2, 0, "a finished build is dirty");
    fsck_clean(&img);
    remove_all(&dir);
}
