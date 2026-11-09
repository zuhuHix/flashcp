//! Smoke tests for `flashcp sync` through the real binary on image files: every result is
//! checked with `fsck.exfat -n` and with the independent strict reader in tests/common.

#[allow(dead_code)]
mod common;

use common::exfat_reader::{self, Volume};
use common::{
    MB, SrcNode, XorShift, assert_success, compare_trees, flashcp, fsck_clean, make_image, make_tricky_tree,
    read_source_tree, run_build, set_mtime, test_dir, write_file,
};
use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output};

const T0: i64 = 1_700_000_000;

fn sync_cmd(src: &Path, img: &Path, extra: &[&str]) -> Output {
    flashcp()
        .arg("sync")
        .arg(src)
        .arg(img)
        .args(extra)
        .env("FLASHCP_TEST_NO_ADD_BUILD", "1")
        .output()
        .expect("run flashcp sync")
}

fn sync_ok(src: &Path, img: &Path, extra: &[&str]) -> Output {
    let out = sync_cmd(src, img, extra);
    assert_success(&out, "flashcp sync");
    out
}

fn read_ok(img: &Path) -> Volume {
    exfat_reader::read_volume(img).unwrap_or_else(|e| panic!("read_volume({}) failed: {e}", img.display()))
}

fn digest(path: &Path) -> u64 {
    let mut f = fs::File::open(path).expect("open image");
    let mut h = DefaultHasher::new();
    let mut buf = vec![0u8; MB as usize];
    loop {
        let n = f.read(&mut buf).expect("read image");
        if n == 0 {
            return h.finish();
        }
        h.write(&buf[..n]);
    }
}

/// The plan event of a `--json` run.
fn plan_line(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with(r#"{"event":"plan""#))
        .unwrap_or_else(|| panic!("no plan event in:\n{}", String::from_utf8_lossy(&out.stdout)))
        .to_string()
}

fn json_u64(obj: &str, key: &str) -> u64 {
    let pat = format!("\"{key}\":");
    let start = obj.find(&pat).unwrap_or_else(|| panic!("no field {key:?} in {obj}")) + pat.len();
    let rest = &obj[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].parse().unwrap_or_else(|e| panic!("field {key:?} of {obj}: {e}"))
}

fn random(path: &Path, bytes: usize, seed: u64, mtime: i64) {
    let mut buf = vec![0u8; bytes];
    XorShift(seed | 1).fill(&mut buf);
    write_file(path, &buf);
    set_mtime(path, mtime, 0);
}

fn text(path: &Path, s: &str, mtime: i64) {
    write_file(path, s.as_bytes());
    set_mtime(path, mtime, 0);
}

fn expect_same(src: &Path, vol: &Volume, what: &str) {
    let problems = compare_trees(&read_source_tree(src), vol);
    assert!(problems.is_empty(), "{what}: volume differs from the source:\n{}", problems.join("\n"));
}

/// A flashcp-built volume (4 KB clusters) of `base`.
fn built(dir: &Path, base: &Path) -> std::path::PathBuf {
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    assert_success(&run_build(base, &img, &["--cluster", "4096"]), "flashcp build");
    img
}

#[test]
fn second_sync_writes_nothing() {
    let dir = test_dir("sync_twice");
    let base = dir.join("base");
    text(&base.join("old.txt"), "old\n", T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    make_tricky_tree(&src);
    sync_ok(&src, &img, &["--delete"]);
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "first sync");
    let before = digest(&img);
    let out = sync_ok(&src, &img, &["--delete", "--json"]);
    let plan = plan_line(&out);
    assert_eq!(json_u64(&plan, "write_bytes"), 0, "{plan}");
    assert_eq!(json_u64(&plan, "new") + json_u64(&plan, "changed") + json_u64(&plan, "deleted"), 0, "{plan}");
    assert_eq!(digest(&img), before, "a sync with nothing to do changed the image");
    // --checksum agrees, and writes nothing either.
    let plan = plan_line(&sync_ok(&src, &img, &["--delete", "--checksum", "--json"]));
    assert_eq!(json_u64(&plan, "changed"), 0, "{plan}");
    assert_eq!(digest(&img), before, "a --checksum sync with nothing to do changed the image");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn changed_file_is_rewritten_into_new_clusters_and_the_old_ones_freed() {
    let dir = test_dir("sync_changed");
    let base = dir.join("base");
    random(&base.join("a/data.bin"), 100_000, 1, T0);
    text(&base.join("a/keep.txt"), "keep\n", T0);
    random(&base.join("shrinks.bin"), 9_000, 5, T0);
    text(&base.join("grows.txt"), "", T0);
    let img = built(&dir, &base);
    let free_before = json_u64(&info(&img), "free");
    let src = dir.join("src");
    random(&src.join("a/data.bin"), 100_000, 2, T0 + 60);
    text(&src.join("a/keep.txt"), "keep\n", T0);
    text(&src.join("shrinks.bin"), "", T0 + 1);
    random(&src.join("grows.txt"), 9_000, 6, T0 + 1);
    set_mtime(&src.join("a"), T0, 0);
    // A dry run reports and writes nothing.
    let before = digest(&img);
    let plan = plan_line(&sync_ok(&src, &img, &["--json", "--dry-run"]));
    assert_eq!(json_u64(&plan, "changed"), 3, "{plan}");
    assert_eq!(digest(&img), before, "--dry-run changed the image");
    let plan = plan_line(&sync_ok(&src, &img, &["--json"]));
    assert_eq!((json_u64(&plan, "changed"), json_u64(&plan, "unchanged")), (3, 1), "{plan}");
    fsck_clean(&img);
    // Strict read: no leaked clusters, so the old version was freed.
    expect_same(&src, &read_ok(&img), "after sync");
    assert_eq!(json_u64(&info(&img), "free"), free_before, "free space after replacing a file by one of the same size");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn quick_check_misses_what_checksum_catches() {
    let dir = test_dir("sync_checksum");
    let base = dir.join("base");
    random(&base.join("same.bin"), 50_000, 1, T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    random(&src.join("same.bin"), 50_000, 7, T0); // same size and time, other content
    let plan = plan_line(&sync_ok(&src, &img, &["--json"]));
    assert_eq!(json_u64(&plan, "unchanged"), 1, "{plan}");
    let plan = plan_line(&sync_ok(&src, &img, &["--checksum", "--json"]));
    assert_eq!(json_u64(&plan, "changed"), 1, "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after --checksum");
    // Same content, other time: only the entry is retimed.
    set_mtime(&src.join("same.bin"), T0 + 3600, 0);
    let plan = plan_line(&sync_ok(&src, &img, &["--checksum", "--json"]));
    assert_eq!((json_u64(&plan, "changed"), json_u64(&plan, "bytes")), (1, 0), "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the retime");
    fs::remove_dir_all(&dir).unwrap();
}

fn info(img: &Path) -> String {
    let out = flashcp().arg("info").arg(img).arg("--json").output().expect("run flashcp info");
    assert_success(&out, "flashcp info");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn delete_removes_extraneous_entries_and_without_it_they_stay() {
    let dir = test_dir("sync_delete");
    let base = dir.join("base");
    text(&base.join("keep.txt"), "keep\n", T0);
    text(&base.join("extra.txt"), "extra\n", T0);
    random(&base.join("old/deep/x.bin"), 20_000, 3, T0);
    text(&base.join("old/y.txt"), "y\n", T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    text(&src.join("keep.txt"), "keep\n", T0);
    let before = digest(&img);
    // Without --delete nothing is removed (and nothing is written: keep.txt is unchanged).
    sync_ok(&src, &img, &[]);
    assert_eq!(digest(&img), before);
    let out = sync_ok(&src, &img, &["--delete"]);
    let plan = String::from_utf8_lossy(&out.stdout);
    assert!(plan.contains("delete: /extra.txt") && plan.contains("delete: /old/ (2 files)"), "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after --delete");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn type_conflict_is_refused_without_delete() {
    let dir = test_dir("sync_conflict");
    let base = dir.join("base");
    text(&base.join("thing"), "a file\n", T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    text(&src.join("thing/inside.txt"), "now a dir\n", T0);
    let before = digest(&img);
    let out = sync_cmd(&src, &img, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pass --delete"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(digest(&img), before, "a refused sync changed the image");
    sync_ok(&src, &img, &["--delete"]);
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after replacing a file by a directory");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn renamed_folder_is_moved_not_copied() {
    let dir = test_dir("sync_rename");
    let base = dir.join("base");
    random(&base.join("Photos 2025/a.jpg"), 300_000, 1, T0);
    random(&base.join("Photos 2025/b.jpg"), 200_000, 2, T0 + 1);
    text(&base.join("Notes.TXT"), "notes\n", T0);
    text(&base.join("Docs/readme.txt"), "readme\n", T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    random(&src.join("Archive/Photos-2025/a.jpg"), 300_000, 1, T0);
    random(&src.join("Archive/Photos-2025/b.jpg"), 200_000, 2, T0 + 1);
    text(&src.join("notes.txt"), "notes\n", T0); // case-only rename
    text(&src.join("docs/readme.txt"), "readme\n", T0); // a directory's case-only rename
    let out = sync_ok(&src, &img, &["--delete", "--json"]);
    let plan = plan_line(&out);
    assert_eq!(json_u64(&plan, "moved"), 3, "{plan}");
    assert_eq!((json_u64(&plan, "new"), json_u64(&plan, "bytes")), (0, 0), "{plan}");
    assert!(plan.contains(r#"{"from":"/Photos 2025/a.jpg","to":"/Archive/Photos-2025/a.jpg"}"#), "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the rename");
    fs::remove_dir_all(&dir).unwrap();
}

// ───────────────────────── crash ordering ─────────────────────────

const PHASES: [&str; 7] = ["dirty", "data", "alloc", "fat", "unlink", "link", "free"];

/// Old tree → new tree with every kind of change: unchanged, changed, case rename, new files in
/// an existing directory that must grow, a deleted subtree, a renamed folder (moves into a new
/// directory), a type conflict and a plain extraneous file.
fn crash_case(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = dir.join("base");
    text(&base.join("keep.txt"), "keep\n", T0);
    random(&base.join("change.bin"), 70_000, 1, T0);
    text(&base.join("Case.txt"), "case\n", T0);
    text(&base.join("existing/one.txt"), "one\n", T0);
    random(&base.join("gone/sub/x.bin"), 30_000, 2, T0);
    text(&base.join("gone/y.txt"), "y\n", T0);
    random(&base.join("old name/big.bin"), 400_000, 3, T0);
    text(&base.join("conflict"), "file\n", T0);
    text(&base.join("extra.txt"), "extra\n", T0);
    let src = dir.join("src");
    text(&src.join("keep.txt"), "keep\n", T0);
    random(&src.join("change.bin"), 90_000, 4, T0 + 10);
    text(&src.join("case.TXT"), "case\n", T0);
    text(&src.join("existing/one.txt"), "one\n", T0);
    for i in 0..200 {
        text(&src.join(format!("existing/n{i:03}.txt")), &format!("n{i}\n"), T0);
    }
    random(&src.join("new name/big.bin"), 400_000, 3, T0);
    text(&src.join("conflict/inside.txt"), "dir\n", T0);
    (base, src)
}

fn as_src(vol: &Volume) -> BTreeMap<String, SrcNode> {
    vol.tree
        .iter()
        .map(|(k, n)| (k.clone(), SrcNode { is_dir: n.is_dir, bytes: n.bytes.clone(), mtime: n.mtime }))
        .collect()
}

#[test]
fn crash_at_every_phase_leaves_a_clean_volume_of_old_or_new_files() {
    let dir = test_dir("sync_crash");
    let (base, src) = crash_case(&dir);
    let pristine = built(&dir, &base);
    let old = as_src(&read_ok(&pristine));
    let new = read_source_tree(&src);
    for phase in PHASES {
        let img = dir.join(format!("{phase}.img"));
        fs::copy(&pristine, &img).unwrap();
        let out = flashcp()
            .arg("sync")
            .arg(&src)
            .arg(&img)
            .arg("--delete")
            .env("FLASHCP_TEST_NO_ADD_BUILD", "1")
            .env("FLASHCP_TEST_STOP_AFTER", phase)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains(&format!("stopped after the {phase} phase")), "{phase}: {err}");
        fsck_clean(&img);
        let vol = exfat_reader::read_volume_allow_leaks(&img).unwrap_or_else(|e| panic!("{phase}: {e}"));
        // Every file is its old or its new version.
        for (p, n) in &vol.tree {
            let ok = |t: &BTreeMap<String, SrcNode>| {
                t.get(p).is_some_and(|s| s.is_dir == n.is_dir && s.bytes == n.bytes && (n.is_dir || s.mtime == n.mtime))
            };
            assert!(ok(&old) || ok(&new), "{phase}: {p} is neither the old nor the new version");
        }
        let got = as_src(&vol);
        let names = |t: &BTreeMap<String, SrcNode>| t.keys().cloned().collect::<Vec<_>>();
        match phase {
            "dirty" | "data" | "alloc" | "fat" => assert_eq!(names(&got), names(&old), "{phase}: tree changed"),
            "unlink" => {
                assert!(got.keys().all(|k| old.contains_key(k)), "{phase}: something new is visible");
                assert_eq!(got["change.bin"].bytes, old["change.bin"].bytes, "{phase}: changed file is not old");
                assert!(!got.contains_key("gone") && !got.contains_key("extra.txt") && !got.contains_key("old name"));
            }
            _ => expect_same(&src, &vol, phase),
        }
        // Recovery: fsck -y clears the dirty flag, then the sync completes the job.
        let _ = Command::new("fsck.exfat").arg("-y").arg(&img).output().unwrap();
        sync_ok(&src, &img, &["--delete"]);
        fsck_clean(&img);
        let vol = exfat_reader::read_volume_allow_leaks(&img).unwrap();
        expect_same(&src, &vol, &format!("{phase}: after recovery"));
        let d = digest(&img);
        sync_ok(&src, &img, &["--delete"]);
        assert_eq!(digest(&img), d, "{phase}: a repeat sync after recovery wrote something");
    }
    // The uninterrupted run leaves no leaks at all.
    sync_ok(&src, &pristine, &["--delete"]);
    fsck_clean(&pristine);
    expect_same(&src, &read_ok(&pristine), "uninterrupted");
    let _ = new;
    fs::remove_dir_all(&dir).unwrap();
}
