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
    run_ok(flashcp().arg("add").arg(&a).arg(&img).args(["--to", "docs"]), "flashcp add (batch a)");
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
    fs::remove_dir_all(&s.dir).unwrap();
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
    fs::remove_dir_all(&s.dir).unwrap();
}

// ───────────────────────── refusals: nothing written ─────────────────────────

#[test]
fn dry_run_and_missing_yes_write_nothing() {
    let s = stick("opt_dry_run");
    let before = digest(&s.img);
    let out = optimize_ok(&s, &["--dry-run"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("optimize:") && text.contains("temporary copy in"), "{text}");
    assert_eq!(digest(&s.img), before, "--dry-run changed the image");
    assert!(temp_dirs(&s).is_empty(), "--dry-run created a temp dir");
    let e = optimize_fails(&s, &[], &[]);
    assert!(e.contains("--yes") && e.contains("nothing was changed"), "{e}");
    assert_eq!(digest(&s.img), before);
    fs::remove_dir_all(&s.dir).unwrap();
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
    fs::remove_dir_all(&s.dir).unwrap();
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
    // Explicitly given, it is used (with a warning).
    let out = optimize(&s, &["--dry-run", "--temp", ram.to_str().unwrap()], &[]);
    assert_success(&out, "optimize --temp tmpfs --dry-run");
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning"), "no tmpfs warning");
    assert!(String::from_utf8_lossy(&out.stdout).contains(&ram.display().to_string()));
    fs::remove_dir_all(&ram).unwrap();
    fs::remove_dir_all(&s.dir).unwrap();
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
    fs::remove_dir_all(&s.dir).unwrap();
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
    fs::remove_dir_all(&s.dir).unwrap();
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
    assert_eq!(digest(&s.img), before, "a foreign volume was written");
    assert_eq!(the_temp_dir(&s), t, "temp copy removed");
    fs::remove_dir_all(&s.dir).unwrap();
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
    fs::remove_dir_all(&s.dir).unwrap();
}

#[test]
fn resume_refuses_a_temp_copy_damaged_after_the_crash() {
    let s = stick("opt_resume_damaged");
    optimize_fails(&s, &["--yes"], &[("FLASHCP_TEST_BUILD_ABORT_AT", "3000000")]);
    let t = the_temp_dir(&s);
    let f = t.join("tree/docs/café.txt");
    let mut b = fs::read(&f).unwrap();
    b[0] ^= 1;
    fs::write(&f, b).unwrap();
    let before = digest(&s.img);
    let e = optimize_fails(&s, &["--yes"], &[]);
    assert!(e.contains("no longer matches its manifest") && e.contains("docs/café.txt"), "{e}");
    assert_eq!(digest(&s.img), before, "a damaged copy was written");
    assert!(t.join("tree").is_dir());
    fs::remove_dir_all(&s.dir).unwrap();
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

/// A file whose FAT chain visits its clusters out of physical order (c, c+2, c+1), so its
/// logical blocks lie on the device in another order than in the file, and a file whose
/// ValidDataLength is half its DataLength (the rest must read as zeros, whatever the disk holds).
#[test]
fn chained_out_of_order_and_valid_data_length_survive() {
    let dir = test_dir("opt_crafted");
    let src = dir.join("src");
    write_random(&src.join("frag.bin"), 3 * OLD_CLUSTER, 11);
    write_random(&src.join("vdl.bin"), 10_000, 12);
    write_random(&src.join("after.bin"), 50_000, 13);
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
    let s = Stick { base: dir.join("tmpbase"), dir, img, orig };
    optimize_ok(&s, &["--yes"]);
    assert_optimized(&s);
    fs::remove_dir_all(&s.dir).unwrap();
}
