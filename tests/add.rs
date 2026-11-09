//! Integration tests for `flashcp add` and `flashcp info`.
//!
//! Each volume is changed by the real binary, then checked with `fsck.exfat -n` and with the
//! independent parser in tests/common/exfat_reader.rs. Failure paths must leave the image
//! byte-for-byte unchanged. The crash-ordering tests use the FLASHCP_TEST_STOP_AFTER hook.
//!
//! The helpers in tests/common are shared with tests/build.rs; this binary uses only part of
//! them, so the module is marked allow(dead_code) here.

#[allow(dead_code)]
mod common;

use common::exfat_reader::{self, Volume};
use common::{
    MB, SrcNode, XorShift, assert_success, compare_trees, flashcp, fsck_clean, gen_dataset, make_image,
    make_tricky_tree, read_source_tree, run_build, test_dir, write_file,
};
use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Output};

const IMG: u64 = 128 * MB;

// ───────────────────────── helpers: running the tools ─────────────────────────

fn add_cmd(src: &Path, img: &Path, extra: &[&str]) -> Output {
    flashcp().arg("add").arg(src).arg(img).args(extra).output().expect("run flashcp add")
}

/// `flashcp add` that must succeed.
fn add_ok(src: &Path, img: &Path, extra: &[&str]) -> Output {
    let out = add_cmd(src, img, extra);
    assert_success(&out, "flashcp add");
    out
}

/// `flashcp add` that must fail, with every needle in stderr.
fn add_fails(src: &Path, img: &Path, extra: &[&str], needles: &[&str]) {
    let out = add_cmd(src, img, extra);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "flashcp add unexpectedly succeeded\nstdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    for n in needles {
        assert!(err.contains(n), "stderr does not contain {n:?}:\n{err}");
    }
}

fn mkfs(img: &Path, cluster: Option<&str>) {
    let mut cmd = Command::new("mkfs.exfat");
    if let Some(c) = cluster {
        cmd.arg("-c").arg(c);
    }
    let out = cmd.arg(img).output().expect("run mkfs.exfat (exfatprogs must be installed)");
    assert_success(&out, "mkfs.exfat");
}

fn info_json(img: &Path) -> String {
    let out = flashcp().arg("info").arg(img).arg("--json").output().expect("run flashcp info");
    assert_success(&out, "flashcp info --json");
    String::from_utf8(out.stdout).expect("utf-8 info output").trim().to_string()
}

/// Reads a volume with the strict reader and panics on error.
fn read_ok(img: &Path) -> Volume {
    exfat_reader::read_volume(img).unwrap_or_else(|e| panic!("read_volume({}) failed: {e}", img.display()))
}

// ───────────────────────── helpers: JSON lines (string matching only) ─────────────────────────

/// Value of a flat-object field, without quotes. Works for numbers and booleans.
fn json_field<'a>(obj: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{key}\":");
    let start = obj.find(&pat).unwrap_or_else(|| panic!("no field {key:?} in {obj}")) + pat.len();
    let rest = &obj[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim_matches('"')
}

fn json_u64(obj: &str, key: &str) -> u64 {
    json_field(obj, key).parse().unwrap_or_else(|e| panic!("field {key:?} of {obj} is not a number: {e}"))
}

fn stdout_lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect()
}

/// Every stdout line must be one JSON object on its own line.
fn assert_event_lines(lines: &[String]) {
    for l in lines {
        assert!(
            l.starts_with("{\"event\":\"") && l.ends_with('}') && l.matches('"').count() % 2 == 0,
            "stdout line is not one JSON event object: {l}"
        );
    }
}

// ───────────────────────── helpers: images and trees ─────────────────────────

/// Length and hash of the whole file, for "byte-for-byte unchanged" checks.
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

/// Overwrites one byte of the image at `off` with `f(old)`.
fn patch_byte(img: &Path, off: u64, f: impl FnOnce(u8) -> u8) {
    let mut file = fs::OpenOptions::new().read(true).write(true).open(img).expect("open image");
    file.seek(SeekFrom::Start(off)).expect("seek");
    let mut b = [0u8];
    file.read_exact(&mut b).expect("read byte");
    b[0] = f(b[0]);
    file.seek(SeekFrom::Start(off)).expect("seek");
    file.write_all(&b).expect("write byte");
    file.sync_all().expect("sync image");
}

/// A file of `bytes` pseudo-random bytes (deterministic for a given seed).
fn write_random(path: &Path, bytes: u64, seed: u64) {
    let mut buf = vec![0u8; bytes as usize];
    let mut rng = XorShift(seed | 1);
    rng.fill(&mut buf);
    write_file(path, &buf);
}

/// What the volume holds, in the same shape as a source tree (so compare_trees can check it).
fn as_src(vol: &Volume) -> BTreeMap<String, SrcNode> {
    vol.tree
        .iter()
        .map(|(k, n)| (k.clone(), SrcNode { is_dir: n.is_dir, bytes: n.bytes.clone(), mtime: n.mtime }))
        .collect()
}

/// A source tree re-keyed under `prefix` ("a/b" turns "x.txt" into "a/b/x.txt").
fn under(prefix: &str, tree: BTreeMap<String, SrcNode>) -> BTreeMap<String, SrcNode> {
    tree.into_iter().map(|(k, v)| (format!("{prefix}/{k}"), v)).collect()
}

/// Checks that the volume equals `want` (entries already there) plus `added` (new entries).
///
/// Directories in `ignore` (synthetic --to parents, or existing directories that gained
/// entries) must exist on the volume as directories; their mtimes are not part of the contract.
/// A directory present both in `want` and in `added` is a merge, and its mtime is ignored too.
fn expect_tree(vol: &Volume, mut want: BTreeMap<String, SrcNode>, added: BTreeMap<String, SrcNode>, ignore: &[&str]) {
    let mut skip: Vec<String> = ignore.iter().map(|s| s.to_string()).collect();
    for (k, v) in added {
        match want.get(&k).map(|old| old.is_dir) {
            Some(old_is_dir) => {
                assert!(
                    old_is_dir && v.is_dir,
                    "{k} exists on the volume and in the new tree, and is not a directory on both sides"
                );
                skip.push(k);
            }
            None => {
                want.insert(k, v);
            }
        }
    }
    for d in &skip {
        let node = vol.tree.get(d).unwrap_or_else(|| panic!("directory {d} is missing on the volume"));
        assert!(node.is_dir, "{d} is not a directory on the volume");
        let e = want.entry(d.clone()).or_insert(SrcNode { is_dir: true, bytes: Vec::new(), mtime: node.mtime });
        assert!(e.is_dir, "{d} is expected to be a directory");
        e.mtime = node.mtime;
    }
    let problems = compare_trees(&want, vol);
    assert!(
        problems.is_empty(),
        "volume differs from the expected tree ({} problems):\n{}",
        problems.len(),
        problems.iter().take(30).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// Small source tree: a root file and a few nested files, all with fixed content.
fn small_src(root: &Path, name: &str) {
    write_file(&root.join(name), format!("content of {name}\n").as_bytes());
}

// ───────────────────────── add onto a fresh volume ─────────────────────────

fn add_tricky_onto_mkfs(name: &str, cluster: Option<&str>) {
    let dir = test_dir(name);
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    mkfs(&img, cluster);
    add_ok(&src, &img, &[]);
    fsck_clean(&img);
    expect_tree(&read_ok(&img), BTreeMap::new(), read_source_tree(&src), &[]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn add_tricky_tree_onto_mkfs_volume_cluster_4k() {
    add_tricky_onto_mkfs("add_mkfs_4k", Some("4096"));
}

#[test]
fn add_tricky_tree_onto_mkfs_volume_default_cluster() {
    add_tricky_onto_mkfs("add_mkfs_default", None);
}

// ───────────────────────── add into a nested --to of a built volume ─────────────────────────

#[test]
fn add_to_nested_dir_keeps_existing_files() {
    let dir = test_dir("add_nested_built");
    let base_src = dir.join("base");
    gen_dataset("tiny")(&base_src);
    let add_src = dir.join("add");
    make_tricky_tree(&add_src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    assert_success(&run_build(&base_src, &img, &["--fat", "prefix"]), "flashcp build");

    expect_tree(&read_ok(&img), read_source_tree(&base_src), BTreeMap::new(), &[]);
    let base = read_source_tree(&base_src);

    add_ok(&add_src, &img, &["--to", "some/nested/dir"]);
    fsck_clean(&img);
    // "some", "some/nested" and "some/nested/dir" are created by this add: their mtimes are "now".
    expect_tree(
        &read_ok(&img),
        base,
        under("some/nested/dir", read_source_tree(&add_src)),
        &["some", "some/nested", "some/nested/dir"],
    );
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── two adds into the same --to ─────────────────────────

#[test]
fn second_add_merges_into_existing_directories() {
    let dir = test_dir("add_merge");
    let a = dir.join("a");
    write_file(&a.join("readme.txt"), b"A\n");
    write_file(&a.join("docs/a.txt"), b"doc a");
    write_file(&a.join("photos/2024/x.jpg"), b"x");
    write_file(&a.join("photos/2024/deep/y.bin"), b"y");
    let b = dir.join("b");
    write_file(&b.join("notes.txt"), b"B\n");
    write_file(&b.join("docs/b.txt"), b"doc b");
    write_file(&b.join("photos/2025/z.jpg"), b"z");
    write_file(&b.join("photos/2024/x2.jpg"), b"x2");
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    mkfs(&img, None);

    add_ok(&a, &img, &["--to", "Backups"]);
    fsck_clean(&img);
    expect_tree(&read_ok(&img), BTreeMap::new(), under("Backups", read_source_tree(&a)), &["Backups"]);

    add_ok(&b, &img, &["--to", "Backups"]);
    fsck_clean(&img);
    // docs, photos and photos/2024 exist after the first add: merged, so their mtimes may change.
    expect_tree(
        &read_ok(&img),
        under("Backups", read_source_tree(&a)),
        under("Backups", read_source_tree(&b)),
        &["Backups"],
    );
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── collisions ─────────────────────────

#[test]
fn same_tree_twice_is_refused_and_image_unchanged() {
    let dir = test_dir("add_collision_tree");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    mkfs(&img, None);
    add_ok(&src, &img, &[]);
    let before = digest(&img);
    add_fails(&src, &img, &[], &["already exist", "--skip-existing"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");
    fsck_clean(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn collisions_are_case_insensitive_ascii() {
    let dir = test_dir("add_case_ascii");
    let first = dir.join("first");
    write_file(&first.join("README.txt"), b"one\n");
    write_file(&first.join("Photos/a.txt"), b"a");
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    mkfs(&img, None);
    add_ok(&first, &img, &[]);
    let before = digest(&img);

    let second = dir.join("second");
    write_file(&second.join("readme.TXT"), b"two\n");
    add_fails(&second, &img, &[], &["already exist", "--skip-existing"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");

    // A directory whose name differs only by case merges with the existing one.
    let third = dir.join("third");
    write_file(&third.join("photos/b.txt"), b"b");
    add_ok(&third, &img, &[]);
    fsck_clean(&img);
    let vol = read_ok(&img);
    assert_eq!(vol.tree.get("Photos/b.txt").map(|n| n.bytes.as_slice()), Some(&b"b"[..]));
    assert!(!vol.tree.contains_key("photos"), "the merged directory was created a second time");
    assert_eq!(vol.tree.get("README.txt").map(|n| n.bytes.as_slice()), Some(&b"one\n"[..]));
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn collisions_are_case_insensitive_non_ascii() {
    let dir = test_dir("add_case_nonascii");
    let first = dir.join("first");
    // é.txt vs É.TXT: the exFAT up-case table maps U+00E9 to U+00C9.
    write_file(&first.join("\u{e9}.txt"), b"lower");
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    mkfs(&img, None);
    add_ok(&first, &img, &[]);
    let before = digest(&img);

    let second = dir.join("second");
    write_file(&second.join("\u{c9}.TXT"), b"upper");
    add_fails(&second, &img, &[], &["already exist", "--skip-existing"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");
    fsck_clean(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── --skip-existing ─────────────────────────

#[test]
fn skip_existing_adds_only_the_new_file() {
    let dir = test_dir("add_skip_existing");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    mkfs(&img, None);
    add_ok(&src, &img, &[]);
    let before = as_src(&read_ok(&img));

    // The same tree again, plus one new file in the root.
    let again = dir.join("again");
    make_tricky_tree(&again);
    write_file(&again.join("new.txt"), b"new\n");
    let out = add_ok(&again, &img, &["--skip-existing"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("skipped"), "summary does not mention skipped entries");
    fsck_clean(&img);

    let new_only: BTreeMap<String, SrcNode> =
        read_source_tree(&again).into_iter().filter(|(k, _)| k == "new.txt").collect();
    assert_eq!(new_only.len(), 1);
    // Everything that was there is unchanged, file contents and mtimes; only new.txt is added.
    expect_tree(&read_ok(&img), before, new_only, &[]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── nearly full volume ─────────────────────────

#[test]
fn nearly_full_volume_refuses_then_takes_what_fits() {
    let dir = test_dir("add_nearly_full");
    let base = dir.join("base");
    for i in 0..26u64 {
        write_random(&base.join(format!("chunk{i:02}.bin")), MB, 1000 + i);
    }
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    assert_success(&run_build(&base, &img, &[]), "flashcp build");
    // The optimizer picks an 8 KB cluster here, so about 2 MB stays free.
    let free = json_u64(&info_json(&img), "free");
    assert!((MB..4 * MB).contains(&free), "unexpected free space {free} on a 26 MB fill of a 32 MB volume");

    let too_big = dir.join("too_big");
    write_random(&too_big.join("big.bin"), 8 * MB, 77);
    let before = digest(&img);
    add_fails(&too_big, &img, &[], &["not enough free space"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");

    let fits = dir.join("fits");
    write_random(&fits.join("small.bin"), free - 256 * 1024, 78);
    add_ok(&fits, &img, &[]);
    fsck_clean(&img);
    expect_tree(&read_ok(&img), read_source_tree(&base), read_source_tree(&fits), &[]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── refused volumes ─────────────────────────

fn one_file_src(dir: &Path) -> std::path::PathBuf {
    let src = dir.join("src");
    small_src(&src, "hello.txt");
    src
}

#[test]
fn dirty_volume_is_refused() {
    let dir = test_dir("add_dirty");
    let src = one_file_src(&dir);
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    mkfs(&img, None);
    // VolumeFlags in the main boot sector, bit 1 (VolumeDirty). Not covered by the boot checksum.
    patch_byte(&img, 106, |b| b | 0x02);

    assert!(info_json(&img).contains("\"dirty\":true"), "info does not report the dirty flag");
    let before = digest(&img);
    add_fails(&src, &img, &[], &["dirty"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn two_fats_volume_is_refused() {
    let dir = test_dir("add_two_fats");
    let src = one_file_src(&dir);
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    mkfs(&img, None);
    patch_byte(&img, 110, |_| 2); // NumberOfFats
    let before = digest(&img);
    add_fails(&src, &img, &[], &["FATs"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn bad_boot_checksum_is_refused() {
    let dir = test_dir("add_bad_checksum");
    let src = one_file_src(&dir);
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    mkfs(&img, None);
    // Byte 100 is VolumeSerialNumber, covered by the boot checksum of sectors 0..10.
    patch_byte(&img, 100, |b| b ^ 0x5A);
    let before = digest(&img);
    add_fails(&src, &img, &[], &["checksum"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn non_exfat_image_is_refused() {
    let dir = test_dir("add_not_exfat");
    let src = one_file_src(&dir);
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    let before = digest(&img);
    add_fails(&src, &img, &[], &["exFAT"]);
    assert_eq!(digest(&img), before, "a refused add changed the image");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── crash ordering ─────────────────────────

/// The state a crash test starts from: a base volume and the add that will be interrupted.
struct CrashCase {
    dir: std::path::PathBuf,
    img: std::path::PathBuf,
    src: std::path::PathBuf,
    before: BTreeMap<String, SrcNode>,
    before_info: String,
}

/// Builds the base volume: a directory "existing" with a few files and a subdirectory. The add
/// puts 300 small files and one 1.5 MB file into "existing", so both the directory and the data
/// area must grow.
fn crash_setup(name: &str) -> CrashCase {
    let dir = test_dir(name);
    let base = dir.join("base");
    write_file(&base.join("root.txt"), b"root\n");
    write_file(&base.join("existing/keep-0.txt"), b"keep 0\n");
    write_file(&base.join("existing/keep-1.txt"), b"keep 1\n");
    write_random(&base.join("existing/sub/deep.bin"), 40_000, 5);
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    assert_success(&run_build(&base, &img, &["--cluster", "4096"]), "flashcp build");
    let before = as_src(&read_ok(&img));
    let before_info = info_json(&img);
    let src = dir.join("src");
    for i in 0..300 {
        write_file(&src.join(format!("n{i:03}.txt")), format!("file {i}\n").as_bytes());
    }
    write_random(&src.join("big.bin"), 1_500_000, 9);
    CrashCase { dir, img, src, before, before_info }
}

/// Runs the add with FLASHCP_TEST_STOP_AFTER=phase and asserts that it stopped there.
fn crash_at(c: &CrashCase, phase: &str) {
    let out = flashcp()
        .arg("add")
        .arg(&c.src)
        .arg(&c.img)
        .args(["--to", "existing"])
        .env("FLASHCP_TEST_STOP_AFTER", phase)
        .output()
        .expect("run flashcp add");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "stop after {phase} exited 0");
    assert!(
        err.contains(&format!("stopped after the {phase} phase")),
        "unexpected error after stop at {phase}:\n{err}"
    );
    fsck_clean(&c.img);
    let after_info = info_json(&c.img);
    assert_eq!(
        json_u64(&after_info, "files"),
        json_u64(&c.before_info, "files"),
        "files changed after stop at {phase}"
    );
    assert_eq!(json_u64(&after_info, "dirs"), json_u64(&c.before_info, "dirs"), "dirs changed after stop at {phase}");
}

/// Recovery as documented: fsck -y clears the dirty flag, then a normal add must work.
fn recover_and_add(c: &CrashCase) {
    let _ = Command::new("fsck.exfat").arg("-y").arg(&c.img).output().expect("run fsck.exfat -y");
    fsck_clean(&c.img);
    assert!(info_json(&c.img).contains("\"dirty\":false"), "fsck -y did not clear the dirty flag");
    add_ok(&c.src, &c.img, &["--to", "existing"]);
    fsck_clean(&c.img);
}

/// Stops the add after `phase` (simulated crash), checks the volume, then recovers and adds again.
fn crash_then_recover(phase: &str) {
    let c = crash_setup(&format!("crash_{phase}"));
    crash_at(&c, phase);
    match phase {
        "dirty" | "data" => {
            let problems = compare_trees(&c.before, &read_ok(&c.img));
            assert!(problems.is_empty(), "tree changed after stop at {phase}:\n{}", problems.join("\n"));
        }
        _ => {
            // Clusters are allocated but unreferenced: the strict reader must refuse, and say why.
            let e = exfat_reader::read_volume(&c.img).expect_err("reader accepted a volume with leaked clusters");
            assert!(
                e.contains("marked allocated but no file or directory uses it"),
                "unexpected reader error after stop at {phase}: {e}"
            );
            // Leaks are the ONLY problem: everything reachable is the old tree, byte for byte.
            let vol = exfat_reader::read_volume_allow_leaks(&c.img)
                .unwrap_or_else(|e| panic!("leak-tolerant read after stop at {phase}: {e}"));
            let problems = compare_trees(&c.before, &vol);
            assert!(problems.is_empty(), "tree changed after stop at {phase}:\n{}", problems.join("\n"));
        }
    }
    recover_and_add(&c);
    let after_info = info_json(&c.img);
    assert_eq!(json_u64(&after_info, "files"), json_u64(&c.before_info, "files") + 301, "file count after recovery");
    // Leaked clusters survive fsck -y and the later add, so after alloc/fat crashes the
    // recovered volume is read leak-tolerantly; the full old + new tree must match either way.
    let CrashCase { dir, img, src, before, .. } = c;
    let vol = match phase {
        "dirty" | "data" => read_ok(&img),
        _ => exfat_reader::read_volume_allow_leaks(&img).unwrap_or_else(|e| panic!("read after recovery: {e}")),
    };
    expect_tree(&vol, before, under("existing", read_source_tree(&src)), &["existing"]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn crash_after_dirty_phase_leaves_volume_intact() {
    crash_then_recover("dirty");
}

#[test]
fn crash_after_data_phase_leaves_volume_intact() {
    crash_then_recover("data");
}

#[test]
fn crash_after_alloc_phase_only_leaks_clusters() {
    crash_then_recover("alloc");
}

#[test]
fn crash_after_fat_phase_only_leaks_clusters() {
    crash_then_recover("fat");
}

// ───────────────────────── existing directory growth ─────────────────────────

#[test]
fn existing_nearly_full_directory_grows_for_500_files() {
    let dir = test_dir("add_dir_growth");
    let base = dir.join("base");
    write_file(&base.join("readme.txt"), b"base\n");
    // 40 entries of 3 x 32 bytes each: about 3840 of the 4096 bytes of the one cluster.
    for i in 0..40 {
        write_file(&base.join(format!("bucket/b{i:02}.txt")), format!("base {i}\n").as_bytes());
    }
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    assert_success(&run_build(&base, &img, &["--cluster", "4096"]), "flashcp build");
    expect_tree(&read_ok(&img), read_source_tree(&base), BTreeMap::new(), &[]);
    let before = as_src(&read_ok(&img));

    let add = dir.join("add");
    for i in 0..500u64 {
        write_random(&add.join(format!("n{i:03}.bin")), 1 + (i * 37) % 2000, i + 1);
    }
    add_ok(&add, &img, &["--to", "bucket"]);
    fsck_clean(&img);
    expect_tree(&read_ok(&img), before, under("bucket", read_source_tree(&add)), &["bucket"]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── JSON output ─────────────────────────

#[test]
fn add_json_stream_is_well_formed() {
    let dir = test_dir("add_json");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    mkfs(&img, None);

    let want = read_source_tree(&src);
    let want_files = want.values().filter(|n| !n.is_dir).count() as u64;
    let want_dirs = want.values().filter(|n| n.is_dir).count() as u64;
    let want_bytes: u64 = want.values().filter(|n| !n.is_dir).map(|n| n.bytes.len() as u64).sum();

    let out = add_ok(&src, &img, &["--json"]);
    let lines = stdout_lines(&out);
    assert_event_lines(&lines);
    assert!(lines.len() >= 2, "too few events: {lines:?}");

    let plan = &lines[0];
    assert!(plan.starts_with(r#"{"event":"plan","op":"add""#), "first event is not an add plan: {plan}");
    assert_eq!(json_u64(plan, "files"), want_files, "plan files");
    assert_eq!(json_u64(plan, "dirs"), want_dirs, "plan dirs");
    assert_eq!(json_u64(plan, "bytes"), want_bytes, "plan bytes");
    assert!(json_u64(plan, "write_bytes") >= want_bytes, "plan write_bytes is smaller than the data");

    let done = lines.last().expect("no events");
    assert!(done.starts_with(r#"{"event":"done""#), "last event is not done: {done}");
    assert_eq!(json_u64(done, "files"), want_files, "done files");
    assert_eq!(json_u64(done, "bytes"), want_bytes, "done bytes");
    json_field(done, "secs").parse::<f64>().expect("secs is a number");

    let (mut prev_files, mut prev_bytes) = (0u64, 0u64);
    for l in &lines[1..lines.len() - 1] {
        assert!(l.starts_with(r#"{"event":"progress""#), "unexpected event between plan and done: {l}");
        let (f, b) = (json_u64(l, "files_done"), json_u64(l, "bytes_done"));
        assert!(f >= prev_files && b >= prev_bytes, "progress went backwards: {l}");
        (prev_files, prev_bytes) = (f, b);
    }
    assert_eq!(lines.iter().filter(|l| l.contains("\"event\":\"plan\"")).count(), 1);
    assert_eq!(lines.iter().filter(|l| l.contains("\"event\":\"done\"")).count(), 1);

    fsck_clean(&img);
    expect_tree(&read_ok(&img), BTreeMap::new(), want, &[]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn add_json_error_is_one_event_line() {
    let dir = test_dir("add_json_error");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    mkfs(&img, None);
    add_ok(&src, &img, &[]);
    let before = digest(&img);

    let out = add_cmd(&src, &img, &["--json"]);
    assert!(!out.status.success(), "colliding add --json succeeded");
    let lines = stdout_lines(&out);
    assert_eq!(lines.len(), 1, "expected exactly one stdout line, got {lines:?}");
    assert!(lines[0].starts_with(r#"{"event":"error","message":""#), "not an error event: {}", lines[0]);
    assert!(lines[0].contains("already exist"), "error message lacks the reason: {}", lines[0]);
    assert_eq!(digest(&img), before, "a refused add changed the image");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn build_json_stream_is_well_formed() {
    let dir = test_dir("build_json");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);

    let out = flashcp()
        .arg("build")
        .arg(&src)
        .arg(&img)
        .arg("--yes-erase")
        .arg("--json")
        .output()
        .expect("run flashcp build --json");
    assert_success(&out, "flashcp build --json");
    let lines = stdout_lines(&out);
    assert_event_lines(&lines);

    let want = read_source_tree(&src);
    let want_files = want.values().filter(|n| !n.is_dir).count() as u64;
    let plan = &lines[0];
    assert!(plan.starts_with(r#"{"event":"plan","op":"build""#), "first event is not a build plan: {plan}");
    assert_eq!(json_u64(plan, "files"), want_files, "plan files");
    let done = lines.last().expect("no events");
    assert!(done.starts_with(r#"{"event":"done""#), "last event is not done: {done}");
    assert_eq!(json_u64(done, "files"), want_files, "done files");
    fsck_clean(&img);
    expect_tree(&read_ok(&img), BTreeMap::new(), want, &[]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn info_json_counts_match_known_tree() {
    let dir = test_dir("info_counts");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    assert_success(&run_build(&src, &img, &[]), "flashcp build");

    let want = read_source_tree(&src);
    let want_files = want.values().filter(|n| !n.is_dir).count() as u64;
    let want_dirs = want.values().filter(|n| n.is_dir).count() as u64;
    let info = info_json(&img);
    assert!(info.starts_with('{') && info.ends_with('}'), "not a JSON object: {info}");
    assert_eq!(json_u64(&info, "files"), want_files, "info files");
    assert_eq!(json_u64(&info, "dirs"), want_dirs, "info dirs (root excluded)");
    assert_eq!(json_field(&info, "label"), "FLASHCP");
    assert_eq!(json_field(&info, "dirty"), "false");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── dry run ─────────────────────────

#[test]
fn dry_run_leaves_image_untouched() {
    let dir = test_dir("add_dry_run");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    mkfs(&img, None);
    let before = digest(&img);
    let out = add_ok(&src, &img, &["--dry-run"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("plan:"), "dry run printed no plan line");
    assert_eq!(digest(&img), before, "dry run changed the image");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── boot sector bookkeeping ─────────────────────────

/// add updates VolumeFlags and PercentInUse in the main boot sector only (like the kernel); the
/// backup region may differ in exactly those volatile bytes, and the volume is clean afterwards.
#[test]
fn boot_sector_flags_clean_and_percent_updated_after_add() {
    let dir = test_dir("add_boot_flags");
    let src = dir.join("src");
    write_file(&src.join("big.bin"), &vec![0x5A; (8 * MB) as usize]);
    let img = dir.join("disk.img");
    make_image(&img, 32 * MB);
    mkfs(&img, None);
    add_ok(&src, &img, &[]);
    let head = fs::read(&img).expect("read image");
    assert_eq!(head[106] & 0x02, 0, "VolumeDirty left set");
    assert!(head[112] >= 20, "PercentInUse not updated: {}", head[112]);
    for i in 0..12 * 512 {
        if !matches!(i, 106 | 107 | 112) {
            assert_eq!(head[i], head[12 * 512 + i], "backup boot region differs at byte {i}");
        }
    }
    read_ok(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── inconsistent allocation is refused ─────────────────────────

/// A flashcp-built 64 MB volume, 4 KB clusters, root holding a.bin and b.bin (one cluster each).
/// flashcp's layout: bitmap at cluster 2, up-case table 3..=4, root dir 5, a.bin 6, b.bin 7.
fn two_file_volume(name: &str) -> (std::path::PathBuf, std::path::PathBuf, u64) {
    let dir = test_dir(name);
    let base = dir.join("base");
    write_file(&base.join("a.bin"), &[0xAA; 4096]);
    write_file(&base.join("b.bin"), &[0xBB; 4096]);
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    assert_success(&run_build(&base, &img, &["--cluster", "4096"]), "flashcp build");
    let vol = read_ok(&img);
    assert_eq!(vol.info.cluster_size, 4096);
    (dir, img, vol.info.heap_offset)
}

fn cluster_off(heap: u64, c: u64) -> u64 {
    heap + (c - 2) * 4096
}

fn assert_refused_unchanged(dir: &Path, img: &Path, needle: &str) {
    let src = dir.join("src");
    small_src(&src, "new.txt");
    let before = digest(img);
    let out = add_cmd(&src, img, &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "add onto an inconsistent volume succeeded");
    assert!(err.contains(needle), "stderr does not mention {needle:?}:\n{err}");
    assert_eq!(digest(img), before, "a refused add changed the image");
}

#[test]
fn bitmap_bit_clear_for_live_file_is_refused() {
    let (dir, img, heap) = two_file_volume("refuse_free_live_cluster");
    // a.bin's cluster 6 is bit 4 of the bitmap's first byte.
    let bm = cluster_off(heap, 2);
    patch_byte(&img, bm, |b| {
        assert_ne!(b & 0x10, 0, "cluster 6 should be allocated");
        b & !0x10
    });
    assert_refused_unchanged(&dir, &img, "marked free in the allocation bitmap");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn cross_linked_clusters_are_refused() {
    let (dir, img, heap) = two_file_volume("refuse_cross_link");
    // Root dir (cluster 5): label, bitmap, up-case entries, then a.bin's set (3 entries) and
    // b.bin's set at byte 192. Point b.bin's FirstCluster at a.bin's cluster 6.
    let set_off = cluster_off(heap, 5) + 192;
    let mut set = vec![0u8; 96];
    {
        let mut f = fs::File::open(&img).expect("open image");
        f.seek(SeekFrom::Start(set_off)).expect("seek");
        f.read_exact(&mut set).expect("read set");
    }
    assert_eq!(set[0], 0x85, "expected b.bin's file entry");
    assert_eq!(u32::from_le_bytes(set[52..56].try_into().unwrap()), 7, "b.bin should start at cluster 7");
    set[52..56].copy_from_slice(&6u32.to_le_bytes());
    let mut sum: u16 = 0;
    for (i, &b) in set.iter().enumerate() {
        if i != 2 && i != 3 {
            sum = sum.rotate_right(1).wrapping_add(b as u16);
        }
    }
    set[2..4].copy_from_slice(&sum.to_le_bytes());
    {
        let mut f = fs::OpenOptions::new().write(true).open(&img).expect("open image");
        f.seek(SeekFrom::Start(set_off)).expect("seek");
        f.write_all(&set).expect("write set");
    }
    assert_refused_unchanged(&dir, &img, "cross-linked");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// Image targets are flock'd exclusively for the whole run: a concurrent holder makes add and
/// build refuse, and nothing is written.
#[test]
fn image_locked_by_another_process_is_refused() {
    use std::os::unix::io::AsRawFd;
    let (dir, img, _) = two_file_volume("refuse_locked_image");
    let holder = fs::File::open(&img).expect("open image");
    assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0, "take the lock");
    assert_refused_unchanged(&dir, &img, "locked by another process");
    let out = run_build(&dir.join("src"), &img, &[]);
    assert!(!out.status.success(), "build onto a locked image succeeded");
    assert!(String::from_utf8_lossy(&out.stderr).contains("locked by another process"));
    drop(holder);
    add_ok(&dir.join("src"), &img, &[]);
    fsck_clean(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── gap policy ─────────────────────────

#[test]
fn image_add_uses_the_default_gap_policy() {
    let dir = test_dir("add_gap_policy_image");
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    mkfs(&img, None);
    let src = dir.join("src");
    small_src(&src, "a.txt");
    let out = add_ok(&src, &img, &["--dry-run"]);
    let plan = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(plan.contains("gaps: fill own slack ≤ 32.8 KB, skip otherwise; 8 in flight (image default)"), "{plan}");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// The hard safety rule of the gap policy: free space interleaved with live files, a policy
/// that would zero-fill gaps of any size (FLASHCP_TEST_GAP_FILL), and still not one byte of an
/// existing file may change, because every gap between the new runs is a used cluster.
#[test]
fn gap_fill_never_writes_over_existing_files() {
    let dir = test_dir("add_gap_fill_safety");
    let base = dir.join("base");
    for k in 0..40u64 {
        write_random(&base.join(format!("d/f{k:02}.bin")), 4096, 500 + k);
    }
    let img = dir.join("disk.img");
    // First with a one-cluster filler to learn the free space, then with a filler that leaves
    // a single free cluster at the end, so the add has to use the holes made below.
    write_file(&base.join("z/fill.bin"), b"x");
    make_image(&img, 32 * MB);
    assert_success(&run_build(&base, &img, &["--cluster", "4096"]), "flashcp build");
    let free = json_u64(&info_json(&img), "free");
    write_random(&base.join("z/fill.bin"), free, 99);
    assert_success(&run_build(&base, &img, &["--cluster", "4096"]), "flashcp build");
    assert_eq!(json_u64(&info_json(&img), "free"), 4096);
    let heap = read_ok(&img).info.heap_offset;
    // flashcp's layout: bitmap 2, up-case 3..=4, root 5, d/ 6, z/ 7; d/'s entry sets are 96
    // bytes each (file, stream, one name). Delete every other file: clear InUse on its three
    // entries and its bitmap bit.
    let mut kept = read_source_tree(&base);
    let d = cluster_off(heap, 6);
    for k in (0..40u64).step_by(2) {
        let set = d + 96 * k;
        let mut stream = [0u8; 32];
        let f = fs::File::open(&img).expect("open image");
        std::os::unix::fs::FileExt::read_exact_at(&f, &mut stream, set + 32).expect("read stream entry");
        assert_eq!(stream[0], 0xC0, "unexpected layout");
        let first = u32::from_le_bytes(stream[20..24].try_into().unwrap()) as u64;
        for e in 0..3 {
            patch_byte(&img, set + 32 * e, |b| b & 0x7F);
        }
        let bit = first - 2;
        patch_byte(&img, cluster_off(heap, 2) + bit / 8, |b| {
            assert_ne!(b & (1 << (bit % 8)), 0, "cluster {first} should be allocated");
            b & !(1 << (bit % 8))
        });
        kept.remove(&format!("d/f{k:02}.bin"));
    }
    fsck_clean(&img);
    let new = dir.join("new");
    for k in 0..15u64 {
        write_random(&new.join(format!("n/n{k:02}.bin")), 4000, 900 + k);
    }
    let out = flashcp()
        .arg("add")
        .arg(&new)
        .arg(&img)
        .env("FLASHCP_TEST_GAP_FILL", "1000000000")
        .env("FLASHCP_TEST_GAPS", "1")
        .output()
        .expect("run flashcp add");
    assert_success(&out, "flashcp add");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("(test override)"), "{stdout}");
    // 16 one-cluster objects in 16 holes: the first skip, then 15 gaps, each a live file.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("gaps: 0 bytes zero-filled, 16 skipped"), "a gap over live files was filled: {stderr}");
    fsck_clean(&img);
    expect_tree(&read_ok(&img), kept, read_source_tree(&new), &[]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── empty volume: add = build ─────────────────────────

fn mkfs_32k_labelled(img: &Path) {
    let out = Command::new("mkfs.exfat").args(["-c", "32768", "-L", "MYSTICK"]).arg(img).output().unwrap();
    assert_success(&out, "mkfs.exfat");
}

/// A fresh 32 KB-cluster volume is written as one build: smaller clusters, label kept,
/// every file there, fsck clean, and the JSON still says op "add".
#[test]
fn add_onto_empty_coarse_volume_is_a_build() {
    let dir = test_dir("add_empty_build");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("v.img");
    make_image(&img, IMG);
    mkfs_32k_labelled(&img);
    let out = add_ok(&src, &img, &["--json"]);
    let plan = String::from_utf8_lossy(&out.stdout).lines().next().unwrap().to_string();
    assert!(plan.contains(r#""op":"add""#), "{plan}");
    fsck_clean(&img);
    let v = read_ok(&img);
    assert!(v.info.cluster_size < 32768, "cluster {}", v.info.cluster_size);
    assert_eq!(v.label, "MYSTICK");
    let problems = compare_trees(&read_source_tree(&src), &v);
    assert!(problems.is_empty(), "{problems:?}");
    // --to puts the tree under the path, still as a build.
    make_image(&img, IMG);
    mkfs_32k_labelled(&img);
    add_ok(&src, &img, &["--to", "Backups/2026"]);
    fsck_clean(&img);
    let v = read_ok(&img);
    assert!(v.info.cluster_size < 32768);
    expect_tree(&v, BTreeMap::new(), under("Backups/2026", read_source_tree(&src)), &["Backups", "Backups/2026"]);
}

/// Anything on the volume (one file) means a normal, non-destructive add: cluster unchanged,
/// the old file still there. A dry run writes nothing either way.
#[test]
fn add_onto_volume_with_data_stays_an_add() {
    let dir = test_dir("add_nonempty_stays");
    let a = dir.join("a");
    write_file(&a.join("old.txt"), b"keep me");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("v.img");
    make_image(&img, IMG);
    mkfs_32k_labelled(&img);
    let before = digest(&img);
    add_ok(&src, &img, &["--dry-run"]);
    assert_eq!(digest(&img), before, "dry run wrote");
    let out = flashcp().arg("add").arg(&a).arg(&img).env("FLASHCP_TEST_NO_ADD_BUILD", "1").output().unwrap();
    assert_success(&out, "add a");
    add_ok(&src, &img, &[]);
    fsck_clean(&img);
    let v = read_ok(&img);
    assert_eq!(v.info.cluster_size, 32768);
    expect_tree(&v, read_source_tree(&a), read_source_tree(&src), &[]);
}
