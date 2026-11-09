//! Kernel-mount integration tests for `flashcp add`.
//!
//! Each test builds an exFAT image, changes it through the kernel exFAT driver (or through
//! flashcp add), and checks it with `fsck.exfat -n` and the independent read-back parser. The
//! kernel mounts need root or udisks, so every test is gated on FLASHCP_MOUNT_TESTS=1 and CI
//! runs them as root (`cargo test --test add_kernel mount`); a gated test must never SKIP there.

#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/kernel.rs"]
mod kernel;

use common::exfat_reader::{ReadNode, Volume};
use common::{
    MB, SrcNode, assert_success, compare_trees, flashcp, gen_dataset, make_tricky_tree, read_source_tree, test_dir,
    write_file,
};
use kernel::{
    Mounted, copy_tree, diff_r, fresh_exfat_image, fsck_strict, info_json, json_u64, pattern, read_back, read_bytes,
    write_pattern,
};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

/// A copy of a source tree with every key prefixed by `prefix` (e.g. "added/").
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

/// Type and content of every source entry against the volume (no extra entries allowed).
/// Used where the kernel has set the mtimes, so only bytes and types are compared.
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

fn check_no_problems(what: &str, problems: Vec<String>) {
    assert!(problems.is_empty(), "{what}: {} problems:\n{}", problems.len(), problems.join("\n"));
}

/// `flashcp add SRC IMG [--to DIR]`, asserting success.
fn add_ok(src: &Path, img: &Path, to: Option<&str>) {
    let mut cmd = flashcp();
    cmd.arg("add").arg(src).arg(img);
    if let Some(t) = to {
        cmd.args(["--to", t]);
    }
    let out = cmd.output().expect("run flashcp add");
    assert_success(&out, "flashcp add");
}

// ───────────────────────── 1. add onto a kernel-populated volume ─────────────────────────

#[test]
fn mount_add_onto_kernel_populated() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_add_onto_kernel_populated");
    let img = dir.join("vol.img");
    fresh_exfat_image(&img, 96 * MB, None, "KERNEL");

    let kern_src = dir.join("kern-src");
    make_tricky_tree(&kern_src);
    let kern_expect = read_source_tree(&kern_src);
    let m = Mounted::new(&img);
    copy_tree(&kern_src, &m.at);
    m.unmount();

    let add_src = dir.join("add-src");
    gen_dataset("tiny")(&add_src);
    let add_expect = read_source_tree(&add_src);
    add_ok(&add_src, &img, Some("added"));
    fsck_strict(&img);

    let vol = read_back(&img);
    let kernel_part = subset(&vol, |k| k != "added" && !k.starts_with("added/"));
    let added_part = subset(&vol, |k| k.starts_with("added/"));
    check_no_problems("kernel-written files after add", content_problems(&kernel_part.tree, &kern_expect));
    assert!(vol.tree.get("added").is_some_and(|n| n.is_dir), "added/ is not a directory on the volume");
    let added_expect = rebase(&add_expect, "added/");
    let problems = compare_trees(&added_expect, &added_part);
    assert!(problems.is_empty(), "added part differs:\n{}", problems.join("\n"));

    // Remount: the kernel must see both parts, byte for byte.
    let m = Mounted::new(&img);
    diff_r(&kern_src, &m.at, &["added"]);
    diff_r(&add_src, &m.at.join("added"), &[]);
    m.unmount();
    fsck_strict(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 2. add into fragmented free space ─────────────────────────

const FILL_LEN: usize = MB as usize;

#[test]
fn mount_add_into_fragmented_free_space() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_add_fragmented");
    let img = dir.join("frag.img");
    fresh_exfat_image(&img, 64 * MB, Some(4096), "FRAG");

    // Fill with 1 MB files until the kernel reports ENOSPC, then punch holes in every other one.
    let mut fills: Vec<u64> = Vec::new(); // seed of each surviving-or-deleted fill file, by index
    let m = Mounted::new(&img);
    write_pattern(&m.at.join("keep/old.bin"), 77, 100_000);
    fs::create_dir_all(m.at.join("fill")).expect("create fill dir");
    loop {
        let idx = fills.len() as u64;
        let seed = 1000 + idx;
        let path = m.at.join(format!("fill/f{idx:03}.bin"));
        let res = fs::File::create(&path).and_then(|mut f| f.write_all(&pattern(seed, FILL_LEN)));
        match res {
            Ok(()) => fills.push(seed),
            Err(e) => {
                assert_eq!(e.raw_os_error(), Some(28), "fill write failed with something other than ENOSPC: {e}");
                let _ = fs::remove_file(&path);
                break;
            }
        }
        assert!(idx < 200, "fill never ran out of space");
    }
    let _ = run_sync();
    assert!(fills.len() >= 40, "only {} MB filled", fills.len());
    for (i, _) in fills.iter().enumerate().filter(|(i, _)| i % 2 == 1) {
        fs::remove_file(m.at.join(format!("fill/f{i:03}.bin"))).expect("punch hole");
    }
    m.unmount();
    fsck_strict(&img);

    // Source: a 5 MB file (larger than any hole), 300 small files under the existing "keep",
    // and a new directory.
    let src = dir.join("src");
    write_pattern(&src.join("big-5mb.bin"), 5, 5 * MB as usize);
    for i in 0..300u64 {
        let len = 1 + (i * 37 % 3000) as usize;
        write_pattern(&src.join(format!("keep/s{i:03}.txt")), 5000 + i, len);
    }
    write_file(&src.join("newdir/inner.txt"), b"inner\n");
    let src_expect = read_source_tree(&src);
    let data_total: u64 = src_expect.values().filter(|s| !s.is_dir).map(|s| s.bytes.len() as u64).sum();
    let n_entries = src_expect.len() as u64;

    let free_before = json_u64(&info_json(&img), "free");
    add_ok(&src, &img, None);
    let free_after = json_u64(&info_json(&img), "free");
    fsck_strict(&img);

    let used = free_before.saturating_sub(free_after);
    assert!(used >= data_total, "free space fell by {used} bytes, less than the {data_total} bytes of data added");
    let slack = (n_entries + 64) * 4096 + 64 * 1024;
    assert!(used <= data_total + slack, "free space fell by {used} bytes, far more than {data_total} bytes of data");

    let vol = read_back(&img);
    let big = vol.tree.get("big-5mb.bin").expect("big file on volume");
    assert!(!big.no_fat_chain, "5 MB file was not split (NoFatChain set) although no hole can hold it");
    assert_eq!(big.bytes, pattern(5, 5 * MB as usize), "5 MB file content differs");
    assert!(vol.tree.get("keep").is_some_and(|n| n.is_dir), "keep/ missing");
    assert_eq!(vol.tree.get("keep/old.bin").map(|n| &n.bytes), Some(&pattern(77, 100_000)), "old keep/old.bin changed");
    for (i, seed) in fills.iter().enumerate() {
        let key = format!("fill/f{i:03}.bin");
        if i % 2 == 1 {
            assert!(!vol.tree.contains_key(&key), "{key} should have been deleted");
        } else {
            let n = vol.tree.get(&key).unwrap_or_else(|| panic!("{key} missing on volume"));
            assert!(n.bytes == pattern(*seed, FILL_LEN), "{key} content changed by add");
        }
    }
    // Everything the source brought, and nothing else (keep/old.bin is kernel-written).
    let expect_new: BTreeMap<String, SrcNode> = rebase(&src_expect, "");
    let added = subset(&vol, |k| expect_new.contains_key(k));
    check_no_problems("new content after add", content_problems(&added.tree, &expect_new));

    // Remount: new content matches, and the surviving 1 MB files are untouched.
    let m = Mounted::new(&img);
    diff_r(&src, &m.at, &["fill", "old.bin"]);
    for (i, seed) in fills.iter().enumerate().filter(|(i, _)| i % 2 == 0) {
        let p = m.at.join(format!("fill/f{i:03}.bin"));
        assert!(read_bytes(&p) == pattern(*seed, FILL_LEN), "{} changed after add", p.display());
    }
    m.unmount();
    fsck_strict(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

fn run_sync() -> std::process::ExitStatus {
    std::process::Command::new(kernel::tool("sync")).status().expect("run sync")
}

// ───────────────────────── 3. kernel keeps writing after an add ─────────────────────────

#[test]
fn mount_add_then_kernel_writes() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_add_then_kernel_writes");
    let img = dir.join("vol.img");
    fresh_exfat_image(&img, 96 * MB, None, "KWRITE");
    let kern_src = dir.join("kern-src");
    make_tricky_tree(&kern_src);
    let m = Mounted::new(&img);
    copy_tree(&kern_src, &m.at);
    m.unmount();

    let add_src = dir.join("add-src");
    gen_dataset("tiny")(&add_src);
    let add_expect = read_source_tree(&add_src);
    add_ok(&add_src, &img, Some("added"));
    fsck_strict(&img);

    // Pick the append target and the file to delete from the added files (sorted, non-empty).
    let mut added_files: Vec<&String> =
        add_expect.iter().filter(|(_, s)| !s.is_dir && !s.bytes.is_empty()).map(|(k, _)| k).collect();
    added_files.sort();
    assert!(added_files.len() >= 3, "gen tiny produced fewer than 3 non-empty files");
    let append_rel = format!("added/{}", added_files[0]);
    let delete_rel = format!("added/{}", added_files[1]);
    let rename_from = format!("added/{}", added_files[2]);
    let rename_to = format!("added/{}.renamed", added_files[2]);
    let appended = pattern(10, 100_000);

    let m = Mounted::new(&img);
    write_pattern(&m.at.join("kernel-new.bin"), 9, 3 * MB as usize);
    write_pattern(&m.at.join("kernel-dir/nested/leaf.bin"), 11, 50_000);
    write_pattern(&m.at.join("kernel-tmp.bin"), 12, MB as usize);
    fs::remove_file(m.at.join("kernel-tmp.bin")).expect("kernel delete");
    let mut f = OpenOptions::new().append(true).open(m.at.join(&append_rel)).expect("open for append");
    f.write_all(&appended).expect("append");
    drop(f);
    fs::remove_file(m.at.join(&delete_rel)).expect("delete added file");
    fs::rename(m.at.join(&rename_from), m.at.join(&rename_to)).expect("rename added file");
    let _ = run_sync();
    m.unmount();
    fsck_strict(&img);

    let vol = read_back(&img);
    assert_eq!(vol.tree.get("kernel-new.bin").map(|n| &n.bytes), Some(&pattern(9, 3 * MB as usize)));
    assert_eq!(vol.tree.get("kernel-dir/nested/leaf.bin").map(|n| &n.bytes), Some(&pattern(11, 50_000)));
    assert!(!vol.tree.contains_key("kernel-tmp.bin"), "deleted kernel file is still there");

    let mut expect_append = add_expect[&added_files[0].clone()].bytes.clone();
    expect_append.extend_from_slice(&appended);
    assert_eq!(vol.tree.get(&append_rel).map(|n| &n.bytes), Some(&expect_append), "appended file differs");
    assert!(!vol.tree.contains_key(&delete_rel), "deleted added file is still there");
    assert!(!vol.tree.contains_key(&rename_from), "renamed file kept its old name");
    let renamed = vol.tree.get(&rename_to).expect("renamed file on volume");
    assert_eq!(renamed.bytes, add_expect[&added_files[2].clone()].bytes, "renamed file content differs");

    // The untouched kernel files are still exactly as copied.
    let kernel_part = subset(&vol, |k| k != "added" && !k.starts_with("added/") && !k.starts_with("kernel-"));
    check_no_problems(
        "kernel files after kernel writes",
        content_problems(&kernel_part.tree, &read_source_tree(&kern_src)),
    );
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 4. mounted image and device are refused ─────────────────────────

#[test]
fn mount_mounted_image_is_refused() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_mounted_refused");
    let img = dir.join("vol.img");
    fresh_exfat_image(&img, 64 * MB, None, "MOUNTED");
    let m = Mounted::new(&img);
    write_pattern(&m.at.join("kernel.txt"), 3, 1000);
    m.unmount();
    let tree_before = read_back(&img).tree;
    fsck_strict(&img);

    let src = dir.join("src");
    write_file(&src.join("a.txt"), b"added while mounted");
    let m = Mounted::new(&img);
    let bytes_before = fs::read(&img).expect("read image");

    let add = |target: &str| flashcp().arg("add").arg(&src).arg(target).output().expect("run flashcp add");
    let img_s = img.to_str().expect("UTF-8 path").to_string();

    let out = add(&img_s);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "add to a mounted image succeeded");
    assert!(err.contains("attached to loop device"), "stderr does not say the image is attached:\n{err}");

    let out = add(&m.dev);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "add to the mounted block device {} succeeded", m.dev);
    let is_root = kernel::is_root();
    let expected = if is_root {
        vec!["mounted", "in use"]
    } else {
        // Non-root: the loop node is usually not writable; either that or the in-use check.
        vec!["mounted", "in use", "Permission denied", "open"]
    };
    assert!(expected.iter().any(|n| err.contains(n)), "stderr does not explain the refusal:\n{err}");

    // Read-only inspection still works while the kernel has the image mounted.
    let info = info_json(&img);
    assert!(info.contains("\"label\":\"MOUNTED\""), "info while mounted: {info}");

    let _ = run_sync();
    let bytes_after = fs::read(&img).expect("read image");
    assert!(bytes_before == bytes_after, "refused add changed the image bytes");
    m.unmount();
    fsck_strict(&img);
    assert!(read_back(&img).tree == tree_before, "image tree changed by the refused adds");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 5. add after the kernel churned the free space ─────────────────────────

#[test]
fn mount_add_after_kernel_delete_reuses_space() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_add_after_delete");
    let img = dir.join("churn.img");
    fresh_exfat_image(&img, 64 * MB, Some(4096), "CHURN");
    const PIECE: usize = 256 * 1024;

    let m = Mounted::new(&img);
    let mut pieces: Vec<u64> = Vec::new();
    loop {
        let idx = pieces.len() as u64;
        let seed = 2000 + idx;
        let path = m.at.join(format!("churn/p{idx:03}.bin"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create churn dir");
        }
        let res = fs::File::create(&path).and_then(|mut f| f.write_all(&pattern(seed, PIECE)));
        match res {
            Ok(()) => pieces.push(seed),
            Err(e) => {
                assert_eq!(e.raw_os_error(), Some(28), "fill failed with something other than ENOSPC: {e}");
                let _ = fs::remove_file(&path);
                break;
            }
        }
        assert!(idx < 400, "fill never ran out of space");
    }
    assert!(pieces.len() >= 100, "only {} pieces written", pieces.len());
    // Kernel-side churn: runs of two deleted pieces out of every five, so the free space is
    // 512 KB holes separated by live data. Then a 2 MB temporary file is written into those
    // holes (so it is split by the kernel too) and deleted again.
    for i in 0..pieces.len() {
        if i % 5 == 1 || i % 5 == 2 {
            fs::remove_file(m.at.join(format!("churn/p{i:03}.bin"))).expect("punch hole");
        }
    }
    let _ = run_sync();
    write_pattern(&m.at.join("churn/tmp-big.bin"), 99, 2 * MB as usize);
    let _ = run_sync();
    fs::remove_file(m.at.join("churn/tmp-big.bin")).expect("delete temporary file");
    let _ = run_sync();
    m.unmount();
    fsck_strict(&img);

    let src = dir.join("src");
    write_pattern(&src.join("small-10k.bin"), 21, 10 * 1024);
    write_pattern(&src.join("mid-300k.bin"), 22, 300 * 1024);
    write_pattern(&src.join("med-700k.bin"), 23, 700 * 1024);
    write_pattern(&src.join("large-3m.bin"), 24, 3 * MB as usize);
    write_file(&src.join("zero.bin"), b"");
    write_file(&src.join("sub/a/b.txt"), b"nested small file\n");
    add_ok(&src, &img, None);
    fsck_strict(&img);

    let vol = read_back(&img);
    for name in ["large-3m.bin", "med-700k.bin"] {
        let n = vol.tree.get(name).unwrap_or_else(|| panic!("{name} missing"));
        assert!(!n.no_fat_chain, "{name} is larger than a 512 KB hole but was not split");
    }
    assert_eq!(vol.tree["large-3m.bin"].bytes, pattern(24, 3 * MB as usize));
    assert_eq!(vol.tree["med-700k.bin"].bytes, pattern(23, 700 * 1024));
    assert_eq!(vol.tree["mid-300k.bin"].bytes, pattern(22, 300 * 1024));
    assert_eq!(vol.tree["small-10k.bin"].bytes, pattern(21, 10 * 1024));
    assert!(vol.tree["zero.bin"].bytes.is_empty(), "zero-byte file is not empty");
    assert!(vol.tree.get("sub/a/b.txt").is_some_and(|n| n.bytes == b"nested small file\n"));
    for (i, seed) in pieces.iter().enumerate() {
        let key = format!("churn/p{i:03}.bin");
        if i % 5 == 1 || i % 5 == 2 {
            assert!(!vol.tree.contains_key(&key), "{key} should have been deleted");
        } else {
            assert!(vol.tree.get(&key).is_some_and(|n| n.bytes == pattern(*seed, PIECE)), "{key} changed by add");
        }
    }

    let m = Mounted::new(&img);
    diff_r(&src, &m.at, &["churn"]);
    m.unmount();
    fsck_strict(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 6. one-cluster holes between live clusters ─────────────────────────

/// Free space made of single 4 KB clusters, each between two live one-cluster files. Every new
/// file is split over many holes or lands in one; the gaps between our runs are other files'
/// data, so nothing may be written there (no zero-filled "slack" across a live cluster).
#[test]
fn mount_add_into_single_cluster_holes_keeps_neighbours() {
    if !kernel::gated() {
        return;
    }
    let dir = test_dir("mount_add_single_holes");
    let img = dir.join("holes.img");
    fresh_exfat_image(&img, 32 * MB, Some(4096), "HOLES");
    let m = Mounted::new(&img);
    fs::create_dir_all(m.at.join("d")).expect("create d");
    let mut n = 0u64;
    loop {
        let res = fs::File::create(m.at.join(format!("d/f{n:05}"))).and_then(|mut f| f.write_all(&pattern(n, 3000)));
        if let Err(e) = res {
            assert_eq!(e.raw_os_error(), Some(28), "fill failed with something other than ENOSPC: {e}");
            let _ = fs::remove_file(m.at.join(format!("d/f{n:05}")));
            break;
        }
        n += 1;
        assert!(n < 20_000, "fill never ran out of space");
    }
    for i in (0..n).step_by(2) {
        fs::remove_file(m.at.join(format!("d/f{i:05}"))).expect("punch hole");
    }
    let _ = run_sync();
    m.unmount();
    fsck_strict(&img);

    let src = dir.join("src");
    write_pattern(&src.join("big.bin"), 77, 3 * MB as usize);
    for k in 0..500u64 {
        write_pattern(&src.join(format!("s/x{k:03}")), 1000 + k, 1500);
    }
    add_ok(&src, &img, None);
    fsck_strict(&img);
    let vol = read_back(&img);
    assert!(!vol.tree["big.bin"].no_fat_chain, "big.bin should be split over the holes");
    assert_eq!(vol.tree["big.bin"].bytes, pattern(77, 3 * MB as usize));
    for k in 0..500u64 {
        assert_eq!(vol.tree[&format!("s/x{k:03}")].bytes, pattern(1000 + k, 1500), "s/x{k:03}");
    }
    for i in (1..n).step_by(2) {
        let key = format!("d/f{i:05}");
        assert!(vol.tree.get(&key).is_some_and(|f| f.bytes == pattern(i, 3000)), "{key} was damaged by add");
    }
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 7. inherited --target-fd (privileged helper) ─────────────────────────

/// Detaches the loop device even when an assertion fails.
struct LoopGuard(String);

impl Drop for LoopGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new(kernel::tool("losetup")).args(["-d", &self.0]).output();
    }
}

/// A "privileged helper" (this test, as root) opens a loop device O_RDWR|O_EXCL and hands the fd
/// to flashcp, which must use it without reopening anything: build, add and info all work,
/// fsck is clean, and nobody else can claim the device while flashcp runs.
#[test]
fn mount_target_fd_inherited_from_helper() {
    if !kernel::gated() {
        return;
    }
    if !kernel::is_root() {
        eprintln!("SKIP --target-fd test: needs root to open a loop device read-write");
        return;
    }
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let dir = test_dir("mount_target_fd");
    let img = dir.join("fd.img");
    common::make_image(&img, 64 * MB);
    let out = std::process::Command::new(kernel::tool("losetup"))
        .args(["--find", "--show"])
        .arg(&img)
        .output()
        .expect("run losetup");
    assert_success(&out, "losetup");
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _guard = LoopGuard(dev.clone());
    let excl = || OpenOptions::new().read(true).write(true).custom_flags(libc::O_EXCL).open(&dev);
    let held = excl().expect("helper: open loop device O_RDWR|O_EXCL");
    let fd = held.as_raw_fd();
    // Let the child inherit it (Rust opens with O_CLOEXEC).
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0, "clear FD_CLOEXEC");
    let fd_s = fd.to_string();

    let src = dir.join("src");
    make_tricky_tree(&src);
    let mut child = flashcp()
        .args(["build"])
        .arg(&src)
        .args(["--target-fd", &fd_s, "--yes-erase", "--cluster", "4096"])
        .spawn()
        .expect("spawn flashcp build --target-fd");
    // While it runs (and after), the exclusive claim held by the helper's fd keeps others out.
    let e = excl().expect_err("a second O_EXCL open succeeded while flashcp was running");
    assert_eq!(e.raw_os_error(), Some(libc::EBUSY), "second O_EXCL open: {e}");
    assert!(child.wait().expect("wait for flashcp").success(), "flashcp build --target-fd failed");

    let more = dir.join("more");
    gen_dataset("tiny")(&more);
    let out = flashcp().arg("add").arg(&more).args(["--target-fd", &fd_s, "--to", "more"]).output().expect("add");
    assert_success(&out, "flashcp add --target-fd");
    let out = flashcp().args(["info", "--target-fd", &fd_s, "--json"]).output().expect("info");
    assert_success(&out, "flashcp info --target-fd");
    let info = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(info.contains("\"dirty\":false"), "info: {info}");
    // A regular file is refused via --target-fd even when opened read-write.
    let reg = OpenOptions::new().read(true).write(true).open(&img).expect("open image");
    assert_eq!(unsafe { libc::fcntl(reg.as_raw_fd(), libc::F_SETFD, 0) }, 0);
    let out = flashcp()
        .arg("add")
        .arg(&more)
        .args(["--target-fd", &reg.as_raw_fd().to_string()])
        .output()
        .expect("add onto a regular-file fd");
    assert!(!out.status.success(), "--target-fd accepted a regular file");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a block device"));
    drop(reg);
    drop(held);

    fsck_strict(Path::new(&dev));
    let vol = read_back(Path::new(&dev));
    let mut want = read_source_tree(&src);
    want.extend(rebase(&read_source_tree(&more), "more/"));
    let more_dir = vol.tree.get("more").expect("more/ missing");
    assert!(more_dir.is_dir);
    want.insert("more".into(), SrcNode { is_dir: true, bytes: vec![], mtime: more_dir.mtime });
    check_no_problems("--target-fd volume", compare_trees(&want, &vol));
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}
