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

// ───────────────────────── more coverage: trees, mtimes, names, moves, mixed runs ─────────────────────────

/// Whether `line` is one complete JSON object (balanced brackets outside strings).
fn json_object_ok(line: &str) -> bool {
    if !(line.starts_with('{') && line.ends_with('}')) {
        return false;
    }
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for c in line.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else {
            match c {
                '"' => in_str = true,
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    depth == 0 && !in_str
}

/// The JSON array stored under `"key":[` in `obj` (its elements' text, without brackets).
fn json_array<'a>(obj: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{key}\":[");
    let start = obj.find(&pat).unwrap_or_else(|| panic!("no array {key:?} in {obj}")) + pat.len();
    let end = obj[start..].find(']').unwrap_or_else(|| panic!("unterminated {key:?} in {obj}")) + start;
    &obj[start..end]
}

/// Used clusters of a volume, from `flashcp info --json` (size and free are in bytes).
fn used_clusters(img: &Path) -> u64 {
    let s = info(img);
    (json_u64(&s, "size") - json_u64(&s, "free")) / json_u64(&s, "cluster")
}

/// Clusters the files of `src` occupy (each file rounds up to whole clusters).
fn file_clusters(src: &Path, cs: u64) -> u64 {
    read_source_tree(src).values().filter(|n| !n.is_dir).map(|n| (n.bytes.len() as u64).div_ceil(cs)).sum()
}

#[test]
fn nested_directories_changes_new_deep_dirs_and_deep_subtree_deletes() {
    let dir = test_dir("sync_nested");
    let base = dir.join("base");
    random(&base.join("a/b/c/d/deep.bin"), 30_000, 1, T0);
    text(&base.join("a/b/c/d/keep.txt"), "keep\n", T0);
    random(&base.join("a/b/c/gonedir/e/f/g.bin"), 40_000, 2, T0);
    text(&base.join("a/b/c/gonedir/e/h.txt"), "h\n", T0);
    text(&base.join("a/top.txt"), "top\n", T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    random(&src.join("a/b/c/d/deep.bin"), 30_000, 9, T0 + 10); // deep change
    text(&src.join("a/b/c/d/keep.txt"), "keep\n", T0);
    random(&src.join("a/b/n1/n2/n3/new.bin"), 12_000, 3, T0 + 20); // new deep directories
    text(&src.join("a/top.txt"), "top\n", T0);
    sync_ok(&src, &img, &["--delete"]);
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "nested tree after sync");
    // A second run is a no-op.
    let before = digest(&img);
    sync_ok(&src, &img, &["--delete"]);
    assert_eq!(digest(&img), before, "a repeat sync wrote something");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn only_mtime_changes_are_rewritten_by_default_and_retimed_by_checksum() {
    let dir = test_dir("sync_mtime_only");
    let base = dir.join("base");
    random(&base.join("a.bin"), 50_000, 1, T0);
    random(&base.join("b.bin"), 50_000, 2, T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    random(&src.join("a.bin"), 50_000, 1, T0 + 60); // same bytes, new time only
    random(&src.join("b.bin"), 50_000, 2, T0);
    let plan = plan_line(&sync_ok(&src, &img, &["--json"]));
    assert_eq!((json_u64(&plan, "changed"), json_u64(&plan, "unchanged")), (1, 1), "{plan}");
    assert_eq!(json_u64(&plan, "bytes"), 50_000, "the quick check rewrites the file: {plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the default sync of a retimed file");
    // Now only b.bin is retimed; --checksum sees the same content and only fixes the entry.
    set_mtime(&src.join("b.bin"), T0 + 120, 0);
    let plan = plan_line(&sync_ok(&src, &img, &["--checksum", "--json"]));
    assert_eq!((json_u64(&plan, "changed"), json_u64(&plan, "bytes")), (1, 0), "{plan}");
    assert_eq!(json_u64(&plan, "unchanged"), 1, "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the checksum retime");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn case_only_renames_are_moves_and_a_case_change_with_new_content_repoints() {
    let dir = test_dir("sync_case");
    let base = dir.join("base");
    text(&base.join("Readme.TXT"), "readme\n", T0);
    random(&base.join("Photos/A.JPG"), 100_000, 7, T0);
    text(&base.join("A.TXT"), "old\n", T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    text(&src.join("README.txt"), "readme\n", T0);
    random(&src.join("photos/a.jpg"), 100_000, 7, T0);
    text(&src.join("A.TXT"), "old\n", T0);
    let plan = plan_line(&sync_ok(&src, &img, &["--json"]));
    assert_eq!(json_u64(&plan, "moved"), 2, "{plan}");
    assert_eq!((json_u64(&plan, "bytes"), json_u64(&plan, "new")), (0, 0), "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the case renames");
    // The source now has a.txt with other content where the volume has A.TXT.
    fs::remove_file(src.join("A.TXT")).unwrap();
    text(&src.join("a.txt"), "new content\n", T0 + 5);
    let plan = plan_line(&sync_ok(&src, &img, &["--json"]));
    assert_eq!((json_u64(&plan, "changed"), json_u64(&plan, "bytes")), (1, 12), "{plan}");
    fsck_clean(&img);
    let vol = read_ok(&img);
    assert!(vol.tree.contains_key("a.txt") && !vol.tree.contains_key("A.TXT"), "the name case must follow the source");
    expect_same(&src, &vol, "after the case change with new content");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_moved_between_existing_directories_is_relinked_only_with_delete() {
    let dir = test_dir("sync_move");
    let base = dir.join("base");
    random(&base.join("one/movie.bin"), 300_000, 1, T0);
    text(&base.join("one/keep.txt"), "one\n", T0);
    text(&base.join("two/other.txt"), "two\n", T0);
    let src = dir.join("src");
    text(&src.join("one/keep.txt"), "one\n", T0);
    text(&src.join("two/other.txt"), "two\n", T0);
    random(&src.join("two/movie.bin"), 300_000, 1, T0);

    // With --delete: the file is re-linked from its old clusters.
    let img = built(&dir, &base);
    let plan = plan_line(&sync_ok(&src, &img, &["--delete", "--json"]));
    assert_eq!(json_u64(&plan, "moved"), 1, "{plan}");
    assert_eq!((json_u64(&plan, "bytes"), json_u64(&plan, "new")), (0, 0), "{plan}");
    assert!(plan.contains(r#"{"from":"/one/movie.bin","to":"/two/movie.bin"}"#), "{plan}");
    fsck_clean(&img);
    let vol = read_ok(&img);
    assert!(!vol.tree.contains_key("one/movie.bin"), "the old path is still there");
    expect_same(&src, &vol, "after the move with --delete");
    fs::remove_dir_all(&dir).unwrap();

    // Without --delete the moved file is copied and the old path stays.
    let dir = test_dir("sync_move_nodelete");
    let base = dir.join("base");
    random(&base.join("one/movie.bin"), 300_000, 1, T0);
    text(&base.join("one/keep.txt"), "one\n", T0);
    text(&base.join("two/other.txt"), "two\n", T0);
    let src = dir.join("src");
    text(&src.join("one/keep.txt"), "one\n", T0);
    text(&src.join("two/other.txt"), "two\n", T0);
    random(&src.join("two/movie.bin"), 300_000, 1, T0);
    let img = built(&dir, &base);
    let plan = plan_line(&sync_ok(&src, &img, &["--json"]));
    assert_eq!((json_u64(&plan, "new"), json_u64(&plan, "moved")), (1, 0), "{plan}");
    fsck_clean(&img);
    let vol = read_ok(&img);
    assert!(vol.tree.contains_key("one/movie.bin") && vol.tree.contains_key("two/movie.bin"));
    assert_eq!(vol.tree["one/movie.bin"].bytes, vol.tree["two/movie.bin"].bytes);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn swapped_names_with_equal_size_and_time_keep_their_own_contents() {
    let dir = test_dir("sync_swap");
    let base = dir.join("base");
    random(&base.join("a.bin"), 80_000, 1, T0);
    random(&base.join("b.bin"), 80_000, 2, T0);
    // In place: a.bin and b.bin trade contents. The quick check cannot see that, --checksum does.
    let img = built(&dir, &base);
    let src = dir.join("src");
    random(&src.join("a.bin"), 80_000, 2, T0);
    random(&src.join("b.bin"), 80_000, 1, T0);
    let plan = plan_line(&sync_ok(&src, &img, &["--delete", "--checksum", "--json"]));
    assert_eq!((json_u64(&plan, "changed"), json_u64(&plan, "moved")), (2, 0), "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the in-place swap");
    fs::remove_dir_all(&dir).unwrap();

    // Renamed: the two files are gone and c.bin / d.bin take their contents. Each new file is a
    // move candidate against two same-size, same-time files; the contents must pick the right one.
    let dir = test_dir("sync_swap_renamed");
    let base = dir.join("base");
    random(&base.join("a.bin"), 80_000, 1, T0);
    random(&base.join("b.bin"), 80_000, 2, T0);
    let img = built(&dir, &base);
    let src = dir.join("src");
    random(&src.join("c.bin"), 80_000, 2, T0);
    random(&src.join("d.bin"), 80_000, 1, T0);
    let plan = plan_line(&sync_ok(&src, &img, &["--delete", "--json"]));
    assert_eq!((json_u64(&plan, "moved"), json_u64(&plan, "bytes")), (2, 0), "{plan}");
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the rename swap");
    fs::remove_dir_all(&dir).unwrap();
}

/// Old tree → new tree with every operation at once. Returns (base, src).
///
/// Changed: bigger, smaller, from empty, to empty. Unchanged: keep.txt, photos/a.jpg (a folder
/// case rename). Moved: Readme.TXT → README.txt (case), movefrom/m.bin → dest/m.bin (a file
/// leaving a directory that is deleted). Deleted: extra.txt, the gonedir subtree. New: new/…
/// (two new directories, two files). Type conflict: thing (file) → thing/ (directory).
fn mixed(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = dir.join("base");
    text(&base.join("keep.txt"), "keep\n", T0);
    random(&base.join("grow.bin"), 10_000, 1, T0);
    random(&base.join("shrink.bin"), 200_000, 3, T0);
    text(&base.join("from_empty.txt"), "", T0);
    random(&base.join("to_empty.bin"), 9_000, 6, T0);
    text(&base.join("Readme.TXT"), "readme\n", T0);
    random(&base.join("Photos/a.jpg"), 100_000, 7, T0);
    random(&base.join("gonedir/e/f/g.bin"), 30_000, 8, T0);
    text(&base.join("gonedir/y.txt"), "y\n", T0);
    text(&base.join("extra.txt"), "extra\n", T0);
    random(&base.join("movefrom/m.bin"), 60_000, 9, T0);
    text(&base.join("dest/keep.txt"), "d\n", T0);
    text(&base.join("thing"), "a file\n", T0);
    let src = dir.join("src");
    text(&src.join("keep.txt"), "keep\n", T0);
    random(&src.join("grow.bin"), 200_000, 2, T0 + 10);
    random(&src.join("shrink.bin"), 1_000, 4, T0 + 10);
    random(&src.join("from_empty.txt"), 7_000, 5, T0 + 10);
    text(&src.join("to_empty.bin"), "", T0 + 10);
    text(&src.join("README.txt"), "readme\n", T0);
    random(&src.join("photos/a.jpg"), 100_000, 7, T0);
    text(&src.join("dest/keep.txt"), "d\n", T0);
    random(&src.join("dest/m.bin"), 60_000, 9, T0);
    text(&src.join("thing/inside.txt"), "dir\n", T0 + 20);
    text(&src.join("new/deep/n.txt"), "new\n", T0 + 20);
    random(&src.join("new/big.bin"), 300_000, 10, T0 + 20);
    (base, src)
}

#[test]
fn mixed_run_with_delete_matches_the_source_and_frees_what_it_should() {
    let dir = test_dir("sync_mixed");
    let (base, src) = mixed(&dir);
    let img = built(&dir, &base);
    let plan = plan_line(&sync_ok(&src, &img, &["--delete", "--json"]));
    assert_eq!((json_u64(&plan, "moved"), json_u64(&plan, "changed")), (2, 4), "{plan}");
    fsck_clean(&img);
    // Strict read: no leaked clusters and every cluster owned.
    expect_same(&src, &read_ok(&img), "after the mixed run");
    let after = digest(&img);
    sync_ok(&src, &img, &["--delete"]);
    assert_eq!(digest(&img), after, "a second sync after the mixed run wrote something");

    // Space: the same source built onto an identical fresh image. Each directory holds at least one
    // cluster, so used = file clusters + directory clusters (the root included). The tighter bound
    // (used after <= used after build + the volume's directory clusters) needs directory cluster
    // counts the reader does not expose; the strict read above is the exact no-leak check.
    let fresh = dir.join("fresh.img");
    make_image(&fresh, 64 * MB);
    assert_success(&run_build(&src, &fresh, &["--cluster", "4096"]), "flashcp build (fresh)");
    fsck_clean(&fresh);
    let cs = json_u64(&info(&img), "cluster");
    let files = file_clusters(&src, cs);
    let dirs = read_source_tree(&src).values().filter(|n| n.is_dir).count() as u64 + 1;
    let (used_sync, used_build) = (used_clusters(&img), used_clusters(&fresh));
    assert!(used_sync >= files + dirs, "used {used_sync} < files {files} + dirs {dirs}");
    assert!(used_build >= files + dirs, "build used {used_build} < files {files} + dirs {dirs}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn json_plan_lists_deletes_and_moves_and_every_line_is_an_object() {
    let dir = test_dir("sync_json");
    let (base, src) = mixed(&dir);
    let img = built(&dir, &base);
    let out = sync_ok(&src, &img, &["--delete", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!stdout.trim().is_empty());
    for line in stdout.lines() {
        assert!(json_object_ok(line), "not one JSON object: {line}");
    }
    let plan = plan_line(&out);
    assert!(plan.contains(r#""op":"sync""#), "{plan}");
    let sync_obj = &plan[plan.find(r#""sync":{"#).expect("sync object") + "\"sync\":".len()..];
    let deletes = json_array(sync_obj, "delete");
    for want in [r#""/extra.txt""#, r#""/gonedir/""#, r#""/movefrom/""#, r#""/thing""#] {
        assert!(deletes.contains(want), "delete list lacks {want}: {deletes}");
    }
    assert!(!deletes.contains("m.bin"), "a moved file is listed as deleted: {deletes}");
    let moves = json_array(sync_obj, "moves");
    assert_eq!(moves, r#"{"from":"/movefrom/m.bin","to":"/dest/m.bin"}"#, "{plan}");
    let last = stdout.lines().last().unwrap();
    assert!(last.starts_with(r#"{"event":"done""#), "the last line is not the done event: {last}");
    expect_same(&src, &read_ok(&img), "after the JSON run");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dry_run_with_deletes_reports_them_and_writes_nothing() {
    let dir = test_dir("sync_dry_delete");
    let (base, src) = mixed(&dir);
    let img = built(&dir, &base);
    let before = digest(&img);
    let out = sync_ok(&src, &img, &["--delete", "--dry-run"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("delete: /extra.txt"), "{text}");
    assert!(text.contains("delete: /gonedir/"), "{text}");
    assert!(text.contains("move: /movefrom/m.bin → /dest/m.bin"), "{text}");
    assert_eq!(digest(&img), before, "--dry-run with deletes changed the image");
    sync_ok(&src, &img, &["--delete"]);
    fsck_clean(&img);
    expect_same(&src, &read_ok(&img), "after the real run");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn crash_at_every_phase_of_a_mixed_run_is_exactly_old_then_new() {
    let dir = test_dir("sync_crash_mixed");
    let (base, src) = mixed(&dir);
    let pristine = built(&dir, &base);
    let old = read_source_tree(&base);
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
        let got = as_src(&vol);
        for (p, n) in &got {
            let ok = |t: &BTreeMap<String, SrcNode>| {
                t.get(p).is_some_and(|s| s.is_dir == n.is_dir && s.bytes == n.bytes && (n.is_dir || s.mtime == n.mtime))
            };
            assert!(ok(&old) || ok(&new), "{phase}: {p} is neither its old nor its new version");
        }
        match phase {
            "dirty" | "data" | "alloc" | "fat" => {
                let problems = compare_trees(&old, &vol);
                assert!(problems.is_empty(), "{phase}: not exactly the old tree:\n{}", problems.join("\n"));
            }
            "unlink" => {
                let extra: Vec<_> = got.keys().filter(|k| !old.contains_key(*k)).collect();
                assert!(extra.is_empty(), "{phase}: paths that only the new tree has are visible: {extra:?}");
            }
            _ => {
                let problems = compare_trees(&new, &vol);
                assert!(problems.is_empty(), "{phase}: not exactly the new tree:\n{}", problems.join("\n"));
            }
        }
        // Recovery: fsck -y clears the dirty flag, then the sync completes the job. Clusters an
        // interrupted run leaked stay allocated (see the ignored test below), so the read tolerates them.
        let _ = Command::new("fsck.exfat").arg("-y").arg(&img).output().unwrap();
        sync_ok(&src, &img, &["--delete"]);
        fsck_clean(&img);
        let vol = exfat_reader::read_volume_allow_leaks(&img).unwrap_or_else(|e| panic!("{phase}: {e}"));
        let problems = compare_trees(&new, &vol);
        assert!(problems.is_empty(), "{phase}: after recovery:\n{}", problems.join("\n"));
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// Mutates the source tree at random: returns a description of what was done.
struct Gen {
    rng: XorShift,
    next: u64,
    clock: i64,
}

impl Gen {
    /// A name no entry has had before (so no two entries ever differ only in case by accident).
    fn name(&mut self) -> String {
        self.next += 1;
        format!("n{:03}", self.next)
    }

    /// A modification time no file has had before.
    fn time(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn bytes(&mut self, max: u64) -> Vec<u8> {
        let mut buf = vec![0u8; self.rng.below(max + 1) as usize];
        self.rng.fill(&mut buf);
        buf
    }
}

fn entries(root: &Path) -> Vec<(std::path::PathBuf, bool)> {
    let mut out = Vec::new();
    let mut ents: Vec<_> = fs::read_dir(root).unwrap().map(|e| e.unwrap()).collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let is_dir = e.file_type().unwrap().is_dir();
        let p = e.path();
        out.push((p.clone(), is_dir));
        if is_dir {
            out.extend(entries(&p));
        }
    }
    out
}

fn pick(rng: &mut XorShift, v: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    (!v.is_empty()).then(|| v[rng.below(v.len() as u64) as usize].clone())
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap().display().to_string()
}

/// One random mutation of the source tree. `same_meta` allows a same-size, same-time content
/// change (only --checksum can see that, so only checksum rounds use it).
fn mutate(g: &mut Gen, src: &Path, same_meta: bool) -> String {
    let all = entries(src);
    let files: Vec<_> = all.iter().filter(|e| !e.1).map(|e| e.0.clone()).collect();
    let dirs_only: Vec<_> = all.iter().filter(|e| e.1).map(|e| e.0.clone()).collect();
    let mut dirs = dirs_only.clone();
    dirs.push(src.to_path_buf());
    let op = g.rng.below(10);
    match op {
        0 => {
            let d = pick(&mut g.rng, &dirs).unwrap();
            let p = d.join(format!("{}.bin", g.name()));
            let bytes = g.bytes(40_000);
            write_file(&p, &bytes);
            let t = g.time();
            set_mtime(&p, t, 0);
            format!("add {}", rel(src, &p))
        }
        1 => match pick(&mut g.rng, &files) {
            Some(f) => {
                fs::remove_file(&f).unwrap();
                format!("delete {}", rel(src, &f))
            }
            None => "noop".into(),
        },
        2 => match pick(&mut g.rng, &files) {
            Some(f) if same_meta => {
                let md = fs::metadata(&f).unwrap();
                let mt = md.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap();
                let mut bytes = vec![0u8; md.len() as usize];
                g.rng.fill(&mut bytes);
                write_file(&f, &bytes);
                set_mtime(&f, mt.as_secs() as i64, mt.subsec_nanos());
                format!("same-size same-time change {}", rel(src, &f))
            }
            Some(f) => {
                let bytes = g.bytes(40_000);
                write_file(&f, &bytes);
                let t = g.time();
                set_mtime(&f, t, 0);
                format!("modify {}", rel(src, &f))
            }
            None => "noop".into(),
        },
        3 => match pick(&mut g.rng, &files) {
            Some(f) => {
                let t = g.time();
                set_mtime(&f, t, 0);
                format!("touch {}", rel(src, &f))
            }
            None => "noop".into(),
        },
        4 => match pick(&mut g.rng, &files) {
            Some(f) => {
                let to = pick(&mut g.rng, &dirs).unwrap().join(format!("{}.bin", g.name()));
                fs::rename(&f, &to).unwrap();
                format!("rename {} -> {}", rel(src, &f), rel(src, &to))
            }
            None => "noop".into(),
        },
        5 => match pick(&mut g.rng, &dirs_only) {
            Some(d) => {
                let to_parent = pick(&mut g.rng, &dirs).unwrap();
                if to_parent.starts_with(&d) {
                    return "noop".into();
                }
                let to = to_parent.join(g.name());
                fs::rename(&d, &to).unwrap();
                format!("move dir {} -> {}", rel(src, &d), rel(src, &to))
            }
            None => "noop".into(),
        },
        6 => {
            let d = pick(&mut g.rng, &dirs).unwrap().join(g.name());
            fs::create_dir_all(&d).unwrap();
            let inner = d.join(format!("{}.bin", g.name()));
            write_file(&inner, b"fresh\n");
            set_mtime(&inner, 1_900_000_000 + g.rng.below(1000) as i64, 0);
            format!("mkdir {} with a file", rel(src, &d))
        }
        7 => match pick(&mut g.rng, &dirs_only) {
            Some(d) => {
                fs::remove_dir_all(&d).unwrap();
                format!("rmdir {}", rel(src, &d))
            }
            None => "noop".into(),
        },
        8 => {
            let mut all_names: Vec<_> = all.iter().map(|e| e.0.clone()).collect();
            all_names.sort();
            match pick(&mut g.rng, &all_names) {
                Some(p) => {
                    let name = p.file_name().unwrap().to_string_lossy().to_string();
                    let flipped =
                        if name.chars().any(|c| c.is_lowercase()) { name.to_uppercase() } else { name.to_lowercase() };
                    if flipped == name {
                        return "noop".into();
                    }
                    let to = p.with_file_name(&flipped);
                    fs::rename(&p, &to).unwrap();
                    format!("case {} -> {flipped}", rel(src, &p))
                }
                None => "noop".into(),
            }
        }
        _ => match pick(&mut g.rng, &files) {
            Some(f) => {
                fs::remove_file(&f).unwrap();
                fs::create_dir_all(&f).unwrap();
                let inner = f.join(format!("{}.bin", g.name()));
                write_file(&inner, b"was a file\n");
                let t = g.time();
                set_mtime(&inner, t, 0);
                format!("file {} becomes a directory", rel(src, &f))
            }
            None => "noop".into(),
        },
    }
}

#[test]
fn random_mutation_rounds_converge_to_the_source() {
    for seed in [1u64, 2, 3, 5, 8, 13] {
        let dir = test_dir(&format!("sync_property_{seed}"));
        let src = dir.join("src");
        let mut g = Gen { rng: XorShift(seed), next: 0, clock: T0 + 100_000 };
        for _ in 0..3 {
            let d = format!("d{}", g.name());
            for _ in 0..4 {
                let f = src.join(&d).join(format!("{}.bin", g.name()));
                let bytes = g.bytes(30_000);
                write_file(&f, &bytes);
                let t = g.time();
                set_mtime(&f, t, 0);
            }
        }
        let deep = src.join("x/y/z");
        let t = g.time();
        write_file(&deep.join("leaf.txt"), b"leaf\n");
        set_mtime(&deep.join("leaf.txt"), t, 0);
        let img = built(&dir, &src);
        for round in 0..4 {
            let checksum = round % 2 == 1;
            let ops: Vec<String> = (0..4).map(|_| mutate(&mut g, &src, checksum)).collect();
            eprintln!("seed {seed} round {round} (checksum: {checksum}): {ops:?}");
            let mut args = vec!["--delete"];
            if checksum {
                args.push("--checksum");
            }
            let out = sync_cmd(&src, &img, &args);
            assert!(
                out.status.success(),
                "seed {seed} round {round}: sync failed: {}\nops: {ops:?}",
                String::from_utf8_lossy(&out.stderr)
            );
            fsck_clean(&img);
            let vol = exfat_reader::read_volume(&img)
                .unwrap_or_else(|e| panic!("seed {seed} round {round}: strict read failed: {e}\nops: {ops:?}"));
            let problems = compare_trees(&read_source_tree(&src), &vol);
            assert!(
                problems.is_empty(),
                "seed {seed} round {round}: volume differs from the source\nops: {ops:?}\n{}",
                problems.join("\n")
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
