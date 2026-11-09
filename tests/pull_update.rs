//! Integration tests for `flashcp pull --update` and `--delete-extraneous`.
//!
//! Each image is built by the real binary (`flashcp build`), pulled, then the source is changed
//! and the image rebuilt, so every case runs the whole path a user would. Files are identified by
//! inode and mtime, which shows whether --update rewrote them. No root needed.

#[allow(dead_code)]
mod common;

use common::{
    MB, XorShift, assert_success, flashcp, make_image, read_source_tree, run_build, set_mtime, test_dir, unix_secs,
    write_file,
};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::Path;
use std::process::Output;

// ───────────────────────── helpers ─────────────────────────

/// `flashcp pull IMG DEST EXTRA...`
fn pull_cmd(img: &Path, dest: &Path, extra: &[&str]) -> Output {
    flashcp().arg("pull").arg(img).arg(dest).args(extra).output().expect("run flashcp pull")
}

/// `flashcp pull` that must succeed; returns its stdout.
fn pull_ok(img: &Path, dest: &Path, extra: &[&str]) -> String {
    let out = pull_cmd(img, dest, extra);
    assert_success(&out, "flashcp pull");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Builds `src` onto a fresh zeroed image.
fn build_image(src: &Path, img: &Path) {
    make_image(img, 64 * MB);
    let out = run_build(src, img, &[]);
    assert_success(&out, "flashcp build");
}

/// Deterministic pseudo-random bytes (same seed, same bytes).
fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    XorShift(seed | 1).fill(&mut buf);
    buf
}

/// (inode, mtime secs, mtime nsec) of every regular file under `dir`, by relative path.
fn file_stats(dir: &Path) -> BTreeMap<String, (u64, i64, i64)> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, (u64, i64, i64)>) {
        for e in fs::read_dir(dir).expect("read dir") {
            let e = e.expect("dir entry");
            let md = fs::symlink_metadata(e.path()).expect("stat");
            let name = e.file_name().into_string().expect("UTF-8 name");
            let key = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if md.is_dir() {
                walk(&e.path(), &key, out);
            } else if md.is_file() {
                out.insert(key, (md.ino(), md.mtime(), md.mtime_nsec()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, "", &mut out);
    out
}

/// Asserts that `dest` holds exactly the tree at `src`: the same paths, types, file bytes and
/// file mtimes (at the 10 ms exFAT granularity). Directory mtimes are compared only when
/// `dir_mtimes` is set. Extra entries on either side are failures.
fn assert_same_tree(src: &Path, dest: &Path, dir_mtimes: bool) {
    let want = read_source_tree(src);
    let got = read_source_tree(dest);
    let mut problems = Vec::new();
    for (path, w) in &want {
        let Some(g) = got.get(path) else {
            problems.push(format!("missing in pulled tree: {path}"));
            continue;
        };
        if g.is_dir != w.is_dir {
            problems.push(format!("type differs: {path} (want dir={}, got dir={})", w.is_dir, g.is_dir));
            continue;
        }
        if !w.is_dir && g.bytes != w.bytes {
            problems.push(format!("content differs: {path}: want {} bytes, got {}", w.bytes.len(), g.bytes.len()));
        }
        if (!w.is_dir || dir_mtimes) && g.mtime != w.mtime {
            problems.push(format!("mtime differs: {path}: want {:?}, got {:?}", w.mtime, g.mtime));
        }
    }
    for path in got.keys() {
        if !want.contains_key(path) {
            problems.push(format!("extra in pulled tree: {path}"));
        }
    }
    assert!(
        problems.is_empty(),
        "pulled tree {} differs from {} ({} problems):\n{}",
        dest.display(),
        src.display(),
        problems.len(),
        problems.join("\n")
    );
}

/// Number of regular files in the source tree.
fn file_count(src: &Path) -> usize {
    read_source_tree(src).values().filter(|s| !s.is_dir).count()
}

/// The source tree used by most cases: six files over three directories, with mtimes that carry
/// sub-10 ms digits. Directory mtimes are whatever the filesystem gave them.
fn make_source(src: &Path) {
    write_file(&src.join("a.txt"), b"alpha, version one\n");
    write_file(&src.join("b.bin"), &pattern(1, 70_000));
    write_file(&src.join("sub/c.bin"), &pattern(2, 9_000));
    write_file(&src.join("sub/d.txt"), b"delta\n");
    write_file(&src.join("sub/deep/e.txt"), b"echo\n");
    write_file(&src.join("gone.txt"), b"going away\n");
    for (i, rel) in ["a.txt", "b.bin", "sub/c.bin", "sub/d.txt", "sub/deep/e.txt", "gone.txt"].iter().enumerate() {
        set_mtime(&src.join(rel), unix_secs(2024, 5, 1, 10, 0, 0) + i as i64 * 3_600, 123_456_789 + i as u32);
    }
}

// ───────────────────────── a. nothing is rewritten when nothing changed ─────────────────────────

#[test]
fn update_rerun_rewrites_nothing() {
    let dir = test_dir("pull_update_rerun");
    let src = dir.join("src");
    make_source(&src);
    let img = dir.join("disk.img");
    build_image(&src, &img);
    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);
    let before = file_stats(&dest);
    assert_eq!(before.len(), file_count(&src));

    let text = pull_ok(&img, &dest, &["--update"]);
    assert_eq!(file_stats(&dest), before, "--update rewrote or moved a file that was up to date");
    assert!(text.contains("pulled: 0 files"), "expected nothing to copy:\n{text}");
    let n = file_count(&src);
    assert!(text.contains(&format!(", {n} up to date")), "expected {n} up to date:\n{text}");
    assert!(!text.contains("deleted"), "nothing should be deleted:\n{text}");
    assert_same_tree(&src, &dest, true);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── b. changed, added and removed source files ─────────────────────────

#[test]
fn update_after_source_changes_replaces_only_what_changed() {
    let dir = test_dir("pull_update_changes");
    let src = dir.join("src");
    make_source(&src);
    let img = dir.join("disk.img");
    build_image(&src, &img);
    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);
    let before = file_stats(&dest);

    // The new source: a.txt changed (new size, new mtime), sub/new.txt added, gone.txt removed.
    write_file(&src.join("a.txt"), b"alpha, version two, and longer now\n");
    set_mtime(&src.join("a.txt"), unix_secs(2025, 1, 1, 0, 0, 0), 42_000_000);
    write_file(&src.join("sub/new.txt"), b"brand new\n");
    set_mtime(&src.join("sub/new.txt"), unix_secs(2025, 1, 2, 0, 0, 0), 0);
    fs::remove_file(src.join("gone.txt")).expect("remove gone.txt from the source");
    build_image(&src, &img);

    pull_ok(&img, &dest, &["--update"]);
    let after = file_stats(&dest);
    assert_ne!(after["a.txt"].0, before["a.txt"].0, "the changed a.txt was not replaced (same inode)");
    for k in ["b.bin", "sub/c.bin", "sub/d.txt", "sub/deep/e.txt", "gone.txt"] {
        assert_eq!(after[k], before[k], "{k} was rewritten although it did not change");
    }
    assert!(after.contains_key("sub/new.txt"), "the added file was not pulled");
    assert_eq!(fs::read(dest.join("a.txt")).expect("read a.txt"), b"alpha, version two, and longer now\n");

    // Without --delete-extraneous, the file the volume no longer has stays in DEST.
    assert!(dest.join("gone.txt").exists(), "gone.txt was deleted without --delete-extraneous");
    fs::remove_file(dest.join("gone.txt")).expect("remove gone.txt from DEST");
    // Directory mtimes are not compared: removing a file just now changed its directory.
    assert_same_tree(&src, &dest, false);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── c. a changed mtime alone is enough ─────────────────────────

#[test]
fn update_replaces_a_file_whose_only_change_is_its_mtime() {
    let dir = test_dir("pull_update_mtime");
    let src = dir.join("src");
    make_source(&src);
    let img = dir.join("disk.img");
    build_image(&src, &img);
    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);
    let before = file_stats(&dest);

    // Same size, same bytes; only the DEST copy's mtime moves (one hour later).
    set_mtime(&dest.join("sub/d.txt"), before["sub/d.txt"].1 + 3_600, 0);
    pull_ok(&img, &dest, &["--update"]);

    let after = file_stats(&dest);
    assert_ne!(after["sub/d.txt"].0, before["sub/d.txt"].0, "sub/d.txt was not replaced (same inode)");
    assert_eq!(
        (after["sub/d.txt"].1, after["sub/d.txt"].2),
        (before["sub/d.txt"].1, before["sub/d.txt"].2),
        "the replacement did not restore the volume's mtime"
    );
    for (k, v) in &before {
        if k != "sub/d.txt" {
            assert_eq!(after[k], *v, "{k} was rewritten although only sub/d.txt differed");
        }
    }
    assert_same_tree(&src, &dest, true);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── d. --delete-extraneous ─────────────────────────

#[test]
fn delete_extraneous_removes_strays_and_leaves_outside_alone() {
    let dir = test_dir("pull_update_delete");
    let src = dir.join("src");
    make_source(&src);
    let img = dir.join("disk.img");
    build_image(&src, &img);
    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);

    // Strays in DEST: a file at the top, a file inside a planned directory, a nested directory
    // tree with a symlink to a directory OUTSIDE DEST, and a top-level symlink to that directory.
    let outside = dir.join("outside");
    write_file(&outside.join("keep/inner.txt"), b"must survive\n");
    write_file(&dest.join("stray.txt"), b"stray\n");
    write_file(&dest.join("sub/extra.txt"), b"extra\n");
    write_file(&dest.join("stray-dir/a/b/c.bin"), &pattern(9, 5_000));
    write_file(&dest.join("stray-dir/x.txt"), b"x\n");
    symlink(&outside, dest.join("stray-dir/into")).expect("symlink into outside");
    symlink(&outside, dest.join("link-out")).expect("symlink to outside");

    let text = pull_ok(&img, &dest, &["--update", "--delete-extraneous"]);
    // stray.txt, sub/extra.txt, link-out, and stray-dir with a, a/b, a/b/c.bin, x.txt and into.
    assert!(text.contains(", deleted 9 extraneous"), "expected 9 deletions:\n{text}");

    assert_eq!(fs::read(outside.join("keep/inner.txt")).expect("read outside"), b"must survive\n");
    for gone in ["stray.txt", "sub/extra.txt", "stray-dir", "link-out"] {
        assert!(fs::symlink_metadata(dest.join(gone)).is_err(), "{gone} still in DEST");
    }
    assert_same_tree(&src, &dest, true);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── e. the quick check's limit ─────────────────────────

/// A same-size content change whose mtime is put back is not seen by --update: the quick check
/// compares size and mtime only. A size change alongside is seen, so the check itself works.
#[test]
fn same_size_content_change_with_same_mtime_is_skipped() {
    let dir = test_dir("pull_update_limit");
    let src = dir.join("src");
    let (secs, nanos) = (unix_secs(2024, 6, 1, 8, 0, 0), 987_654_321);
    write_file(&src.join("same.txt"), &[b'A'; 32]);
    write_file(&src.join("other.txt"), b"short\n");
    set_mtime(&src.join("same.txt"), secs, nanos);
    set_mtime(&src.join("other.txt"), secs, nanos);
    let img = dir.join("disk.img");
    build_image(&src, &img);
    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);
    let before = file_stats(&dest);

    // The source changes: same.txt gets other bytes of the same length, mtime put back; other.txt
    // gets a different length and a new mtime.
    write_file(&src.join("same.txt"), &[b'B'; 32]);
    set_mtime(&src.join("same.txt"), secs, nanos);
    write_file(&src.join("other.txt"), b"a longer line than before\n");
    set_mtime(&src.join("other.txt"), secs + 60, 0);
    build_image(&src, &img);

    pull_ok(&img, &dest, &["--update"]);
    assert_eq!(fs::read(dest.join("same.txt")).expect("read same.txt"), [b'A'; 32], "same-size change was detected");
    let after = file_stats(&dest);
    assert_eq!(after["same.txt"], before["same.txt"], "same.txt was rewritten");
    assert_eq!(fs::read(dest.join("other.txt")).expect("read other.txt"), b"a longer line than before\n");
    assert_ne!(after["other.txt"].0, before["other.txt"].0, "other.txt was not replaced");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── f. the CLI refuses bad combinations ─────────────────────────

#[test]
fn cli_refuses_delete_extraneous_without_update_and_update_with_overwrite() {
    let dir = test_dir("pull_update_cli");
    let src = dir.join("src");
    make_source(&src);
    let img = dir.join("disk.img");
    build_image(&src, &img);

    let cases: [(&[&str], &[&str]); 3] = [
        (&["--delete-extraneous"], &["--delete-extraneous", "--update"]),
        (&["--update", "--overwrite"], &["--update", "--overwrite"]),
        (&["--update", "--skip-existing"], &["--update", "--skip-existing"]),
    ];
    for (args, needles) in cases {
        let dest = dir.join("out-refused");
        let out = pull_cmd(&img, &dest, args);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "pull {args:?} was accepted");
        for n in needles {
            assert!(err.contains(n), "pull {args:?}: stderr does not mention {n}:\n{err}");
        }
        assert!(!dest.exists(), "pull {args:?} created the destination");
    }
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}
