//! Kernel-mount integration tests for `flashcp pull`.
//!
//! The volume is written through the kernel exFAT driver (copy, or plain file writes on a
//! fragmented volume), unmounted, checked with `fsck.exfat -n` and the independent parser, and
//! then pulled. The pulled tree must match what the kernel wrote.
//!
//! Mtimes: the kernel stamps files with the time of writing, so the expected mtime of every
//! file is taken from the volume as the independent parser reads it (exact, at the 10 ms exFAT
//! granularity). Directory mtimes are not compared. The mount needs root or udisks, so every
//! test is gated on FLASHCP_MOUNT_TESTS=1 and CI runs them as root (`cargo test --test pull_kernel mount`).
//! A gated test must never SKIP there.

#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/kernel.rs"]
mod kernel;

use common::exfat_reader::Volume;
use common::{MB, SrcNode, assert_success, flashcp, make_tricky_tree, read_source_tree, test_dir};
use kernel::{Mounted, copy_tree, fresh_exfat_image, fsck_strict, pattern, read_back, write_pattern};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

// ───────────────────────── helpers ─────────────────────────

/// `flashcp pull IMG DEST EXTRA...`, asserting success.
fn pull_ok(img: &Path, dest: &Path, extra: &[&str]) {
    let out: Output = flashcp().arg("pull").arg(img).arg(dest).args(extra).output().expect("run flashcp pull");
    assert_success(&out, "flashcp pull");
}

fn run_sync() {
    let status = Command::new(kernel::tool("sync")).status().expect("run sync");
    assert!(status.success(), "sync failed");
}

/// Every node of a volume as a SrcNode (bytes and mtime as the parser reads them).
fn from_volume(vol: &Volume) -> BTreeMap<String, SrcNode> {
    vol.tree
        .iter()
        .map(|(k, n)| (k.clone(), SrcNode { is_dir: n.is_dir, bytes: n.bytes.clone(), mtime: n.mtime }))
        .collect()
}

/// Asserts that `got` holds exactly the nodes of `want`: same paths and types, same file bytes,
/// and, when `check_mtime` is set, the same file mtimes. Directory mtimes are never compared.
fn assert_same(want: &BTreeMap<String, SrcNode>, got: &BTreeMap<String, SrcNode>, what: &str, check_mtime: bool) {
    let mut problems = Vec::new();
    for (path, w) in want {
        let Some(g) = got.get(path) else {
            problems.push(format!("missing: {path}"));
            continue;
        };
        if g.is_dir != w.is_dir {
            problems.push(format!("type differs: {path}"));
        } else if !w.is_dir {
            if g.bytes != w.bytes {
                let first = g.bytes.iter().zip(&w.bytes).position(|(a, b)| a != b);
                problems.push(format!(
                    "content differs: {path}: want {} bytes, got {} bytes, first mismatch at {first:?}",
                    w.bytes.len(),
                    g.bytes.len()
                ));
            }
            if check_mtime && g.mtime != w.mtime {
                problems.push(format!("mtime differs: {path}: want {:?}, got {:?}", w.mtime, g.mtime));
            }
        }
    }
    for path in got.keys() {
        if !want.contains_key(path) {
            problems.push(format!("extra: {path}"));
        }
    }
    assert!(
        problems.is_empty(),
        "{what}: {} problems:\n{}",
        problems.len(),
        problems.iter().take(30).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// A host-side expectation entry: a directory, or a file with known bytes (mtime unused here).
fn dir_node() -> SrcNode {
    SrcNode { is_dir: true, bytes: Vec::new(), mtime: (0, 0) }
}

fn file_node(bytes: Vec<u8>) -> SrcNode {
    SrcNode { is_dir: false, bytes, mtime: (0, 0) }
}

/// Byte-for-byte copy of the whole image, for "pull does not change the image" checks.
fn image_bytes(img: &Path) -> Vec<u8> {
    fs::read(img).expect("read image")
}

// ───────────────────────── 1. kernel-written tricky tree ─────────────────────────

#[test]
fn mount_pull_kernel_written() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_pull_kernel_written");
    let img = dir.join("vol.img");
    fresh_exfat_image(&img, 96 * MB, None, "PULLK");

    let src = dir.join("src");
    make_tricky_tree(&src);
    let m = Mounted::new(&img);
    copy_tree(&src, &m.at);
    m.unmount();
    fsck_strict(&img);

    let vol = read_back(&img);
    // The kernel must have stored what was copied (content and types; the mtimes are kernel's).
    assert_same(&read_source_tree(&src), &from_volume(&vol), "kernel-written volume", false);

    let before = image_bytes(&img);
    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);
    assert!(image_bytes(&img) == before, "pull changed the image");

    assert_same(&from_volume(&vol), &read_source_tree(&dest), "pulled tree", true);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 2. fragmented volume ─────────────────────────

const FILL_LEN: usize = MB as usize;

/// A 64 MB, 4 KB-cluster volume that the kernel fills, churns, and then writes big files into.
/// The big files cannot fit in any hole (the largest hole is one 1 MB fill file), so the kernel
/// must split them into FAT chains. Pull must return every surviving byte.
#[test]
fn mount_pull_fragmented() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_pull_fragmented");
    let img = dir.join("frag.img");
    fresh_exfat_image(&img, 64 * MB, Some(4096), "FRAGPULL");

    // Expected content of everything that survives, keyed by volume path.
    let mut expect: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let m = Mounted::new(&img);

    // 200 small and medium files of 1..150000 bytes, in one directory.
    fs::create_dir_all(m.at.join("mix")).expect("create mix");
    for i in 0..200u64 {
        let len = 1 + ((i * 7919) % 150_000) as usize;
        write_pattern(&m.at.join(format!("mix/f{i:03}.bin")), 500 + i, len);
    }
    // Fill the rest of the disk with 1 MB files until the kernel reports ENOSPC.
    fs::create_dir_all(m.at.join("fill")).expect("create fill");
    let mut fills = 0u64;
    loop {
        let path = m.at.join(format!("fill/f{fills:03}.bin"));
        let res = fs::File::create(&path).and_then(|mut f| f.write_all(&pattern(1000 + fills, FILL_LEN)));
        match res {
            Ok(()) => fills += 1,
            Err(e) => {
                assert_eq!(e.raw_os_error(), Some(28), "fill failed with something other than ENOSPC: {e}");
                let _ = fs::remove_file(&path);
                break;
            }
        }
        assert!(fills < 200, "fill never ran out of space");
    }
    assert!(fills >= 20, "only {fills} MB of fill files");

    // Punch holes: every other small file, and every other 1 MB file.
    run_sync();
    for i in 0..200u64 {
        if i % 2 == 0 {
            fs::remove_file(m.at.join(format!("mix/f{i:03}.bin"))).expect("punch small hole");
        } else {
            expect.insert(format!("mix/f{i:03}.bin"), pattern(500 + i, 1 + ((i * 7919) % 150_000) as usize));
        }
    }
    for i in 0..fills {
        if i % 2 == 1 {
            fs::remove_file(m.at.join(format!("fill/f{i:03}.bin"))).expect("punch 1 MB hole");
        } else {
            expect.insert(format!("fill/f{i:03}.bin"), pattern(1000 + i, FILL_LEN));
        }
    }
    run_sync();

    // Three 4 MB files: no hole is that large, so each one has to be split.
    fs::create_dir_all(m.at.join("big")).expect("create big");
    for k in 0..3u64 {
        write_pattern(&m.at.join(format!("big/b{k}.bin")), 9000 + k, 4 * MB as usize);
        expect.insert(format!("big/b{k}.bin"), pattern(9000 + k, 4 * MB as usize));
    }
    run_sync();
    m.unmount();
    fsck_strict(&img);

    let vol = read_back(&img);
    let chained = (0..3).filter(|k| !vol.tree[&format!("big/b{k}.bin")].no_fat_chain).count();
    assert!(chained >= 1, "no 4 MB file was FAT-chained; the volume is not fragmented as intended");

    // What the kernel wrote must be what we wrote (the parser is strict about the structure).
    let mut want: BTreeMap<String, SrcNode> = BTreeMap::new();
    for d in ["mix", "fill", "big"] {
        want.insert(d.to_string(), dir_node());
    }
    for (path, bytes) in &expect {
        want.insert(path.clone(), file_node(bytes.clone()));
    }
    assert_same(&want, &from_volume(&vol), "kernel-written fragmented volume", false);

    let dest = dir.join("out");
    let before = image_bytes(&img);
    pull_ok(&img, &dest, &[]);
    assert!(image_bytes(&img) == before, "pull changed the image");
    assert_same(&from_volume(&vol), &read_source_tree(&dest), "pulled fragmented tree", true);
    eprintln!("fragmented volume: {chained} of 3 big files are FAT-chained, {fills} fill files");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 3. pull while the image is still mounted ─────────────────────────

/// Pulling from an image the kernel still has mounted (after sync) must succeed read-only,
/// print a warning that mentions "mounted", and return the content. The mount is left alone.
#[test]
fn mount_pull_from_mounted_volume() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_pull_mounted");
    let img = dir.join("live.img");
    fresh_exfat_image(&img, 64 * MB, None, "LIVEPULL");

    let m = Mounted::new(&img);
    write_pattern(&m.at.join("live/a.bin"), 61, 300_000);
    write_pattern(&m.at.join("live/b/c.txt"), 62, 10);
    fs::create_dir_all(m.at.join("live/empty")).expect("create empty dir");
    run_sync();

    let before = image_bytes(&img);
    let dest = dir.join("out");
    let out = flashcp().arg("pull").arg(&img).arg(&dest).output().expect("run flashcp pull");
    assert_success(&out, "flashcp pull on a mounted image");
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(err.contains("mounted"), "no warning about the mounted volume on stderr:\n{err}");
    assert!(image_bytes(&img) == before, "pull changed the mounted image");

    let mut want: BTreeMap<String, SrcNode> = BTreeMap::new();
    want.insert("live".into(), dir_node());
    want.insert("live/b".into(), dir_node());
    want.insert("live/empty".into(), dir_node());
    want.insert("live/a.bin".into(), file_node(pattern(61, 300_000)));
    want.insert("live/b/c.txt".into(), file_node(pattern(62, 10)));
    assert_same(&want, &read_source_tree(&dest), "pulled from mounted image", false);

    m.unmount();
    fsck_strict(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}
