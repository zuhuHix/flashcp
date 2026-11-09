//! Kernel-mount integration tests for `flashcp sync`.
//!
//! Each test writes an exFAT image through the kernel exFAT driver or through flashcp, syncs it,
//! and checks it with `fsck.exfat -n`, the strict read-back parser and `diff -r` on a remount.
//! Mounting needs root or udisks, so every test is gated on FLASHCP_MOUNT_TESTS=1 and CI runs
//! them as root (`cargo test --test sync_kernel mount`); a gated test must never SKIP there.

#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/kernel.rs"]
mod kernel;

use common::exfat_reader::{ReadNode, Volume};
use common::{
    MB, SrcNode, assert_success, compare_trees, flashcp, make_image, make_tricky_tree, read_source_tree, run_build,
    test_dir, write_file,
};
use kernel::{Mounted, copy_tree, diff_r, fresh_exfat_image, fsck_strict, read_back, write_pattern};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// A copy of a source tree with every key prefixed by `prefix` (e.g. "site/").
fn rebase(src: &BTreeMap<String, SrcNode>, prefix: &str) -> BTreeMap<String, SrcNode> {
    src.iter()
        .map(|(k, s)| (format!("{prefix}{k}"), SrcNode { is_dir: s.is_dir, bytes: s.bytes.clone(), mtime: s.mtime }))
        .collect()
}

/// The part of a volume whose paths satisfy `keep`, as a Volume of its own.
fn subset(vol: &Volume, keep: impl Fn(&str) -> bool) -> Volume {
    let tree: BTreeMap<String, ReadNode> =
        vol.tree.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    Volume { info: vol.info, label: vol.label.clone(), tree }
}

/// Types and bytes of every source entry against the volume, with no extra entries allowed.
/// Used where the kernel has set the mtimes, so only what the kernel must preserve is compared.
fn content_problems(tree: &BTreeMap<String, ReadNode>, src: &BTreeMap<String, SrcNode>) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, s) in src {
        match tree.get(path) {
            None => problems.push(format!("missing on volume: {path}")),
            Some(v) if v.is_dir != s.is_dir => problems.push(format!("type differs: {path}")),
            Some(v) if !s.is_dir && v.bytes != s.bytes => problems.push(format!("content differs: {path}")),
            Some(_) => {}
        }
    }
    for path in tree.keys() {
        if !src.contains_key(path) {
            problems.push(format!("extra on volume: {path}"));
        }
    }
    problems
}

/// `flashcp sync SRC IMG EXTRA...`, asserting success.
fn sync_ok(src: &Path, img: &Path, extra: &[&str]) {
    let out = flashcp()
        .arg("sync")
        .arg(src)
        .arg(img)
        .args(extra)
        .env("FLASHCP_TEST_NO_ADD_BUILD", "1")
        .env("XDG_STATE_HOME", img.parent().unwrap().join("state"))
        .output()
        .expect("run flashcp sync");
    assert_success(&out, &format!("flashcp sync {extra:?}"));
}

// ───────────────────────── 1. sync into a kernel-written directory ─────────────────────────

#[test]
fn mount_sync_into_kernel_written_directory() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_sync_kernel_written");
    let img = dir.join("vol.img");
    fresh_exfat_image(&img, 96 * MB, None, "KERNEL");

    // The kernel writes the tricky tree and a 12 MB file under site/, then the mtimes the
    // source had are put back, so a later sync can see which entries really moved.
    let base = dir.join("base");
    make_tricky_tree(&base);
    write_pattern(&base.join("big/big.bin"), 42, 12 * MB as usize);
    let expect = read_source_tree(&base);
    let m = Mounted::new(&img);
    copy_tree(&base, &m.at.join("site"));
    for (rel, s) in &expect {
        let p = m.at.join("site").join(rel);
        let (secs, nanos) = s.mtime;
        common::set_mtime(&p, secs, nanos);
    }
    m.unmount();
    fsck_strict(&img);

    // The source: the same tree with a change, a new file in a new directory, a deleted file,
    // a deleted directory and a moved file.
    let src = dir.join("src");
    make_tricky_tree(&src);
    write_pattern(&src.join("big/big.bin"), 42, 12 * MB as usize);
    write_file(&src.join("café.txt"), b"changed by the source\n");
    write_file(&src.join("added/new.txt"), b"new\n");
    fs::remove_file(src.join("bulk/d0/f001.bin")).expect("delete a file");
    fs::remove_dir(src.join("empty-dir")).expect("delete a directory");
    fs::create_dir_all(src.join("moved")).expect("create the move target");
    fs::rename(src.join("random-5mb.bin"), src.join("moved/random-5mb.bin")).expect("move a file");

    sync_ok(&src, &img, &["--to", "site", "--delete", "--yes"]);
    fsck_strict(&img);
    let vol = read_back(&img);
    let site = subset(&vol, |k| k.starts_with("site/"));
    let problems = content_problems(&site.tree, &rebase(&read_source_tree(&src), "site/"));
    assert!(problems.is_empty(), "site/ differs from the source:\n{}", problems.join("\n"));

    // Remount: the kernel must read the synced tree back, byte for byte.
    let m = Mounted::new(&img);
    diff_r(&src, &m.at.join("site"), &[]);
    m.unmount();
    fsck_strict(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 2. the kernel's edits are reverted by the next sync ─────────────────────────

#[test]
fn mount_sync_reverts_a_kernel_edit_on_a_flashcp_built_image() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_sync_reverts_kernel_edit");
    let img = dir.join("vol.img");
    make_image(&img, 96 * MB);

    let base = dir.join("base");
    make_tricky_tree(&base);
    assert_success(&run_build(&base, &img, &["--cluster", "4096"]), "flashcp build");
    fsck_strict(&img);

    // First sync: the source differs from the build in a few entries.
    let src = dir.join("src");
    make_tricky_tree(&src);
    write_file(&src.join("café.txt"), b"changed by the source\n");
    write_file(&src.join("added/new.txt"), b"new\n");
    fs::remove_file(src.join("bulk/d1/f002.bin")).expect("delete a file");
    sync_ok(&src, &img, &["--delete", "--yes"]);
    fsck_strict(&img);
    let problems = compare_trees(&read_source_tree(&src), &read_back(&img));
    assert!(problems.is_empty(), "after the first sync:\n{}", problems.join("\n"));

    // The kernel edits the volume: one file changed, one deleted, one new file added.
    let m = Mounted::new(&img);
    fs::write(m.at.join("café.txt"), b"kernel edit\n").expect("kernel write");
    fs::remove_file(m.at.join("bulk/d2/f010.bin")).expect("kernel delete");
    fs::write(m.at.join("kernel-only.txt"), b"kernel\n").expect("kernel create");
    m.unmount();
    fsck_strict(&img);

    // The second sync reverts all of it to the source.
    sync_ok(&src, &img, &["--delete", "--yes"]);
    fsck_strict(&img);
    let problems = compare_trees(&read_source_tree(&src), &read_back(&img));
    assert!(problems.is_empty(), "after the second sync:\n{}", problems.join("\n"));
    let m = Mounted::new(&img);
    diff_r(&src, &m.at, &[]);
    m.unmount();
    fsck_strict(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}
