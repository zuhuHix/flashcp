//! Integration tests for `flashcp pull`.
//!
//! Each volume is built by the real binary (`flashcp build`), then read back with `flashcp pull`
//! into a fresh directory and compared with the source tree. The image must never change. Pull
//! must refuse non-exFAT input without creating anything, and must never write through a symlink.
//! These tests need no root and always run. The kernel-driver variants live in tests/pull_kernel.rs.
//!
//! Mtimes are compared the same way the build tests compare them: `read_source_tree` applies the
//! exFAT rule (truncate to 10 ms, clamp pre-1980 times to 1980-01-01) to both sides, so a pulled
//! file must carry exactly the exFAT value that the build wrote.

#[allow(dead_code)]
mod common;

use common::{
    MB, SrcNode, XorShift, assert_success, flashcp, gen_dataset, make_image, make_tricky_tree, read_source_tree,
    run_build, test_dir, write_file,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Output;

// ───────────────────────── helpers ─────────────────────────

/// `flashcp pull IMG DEST EXTRA...`
fn pull_cmd(img: &Path, dest: &Path, extra: &[&str]) -> Output {
    flashcp().arg("pull").arg(img).arg(dest).args(extra).output().expect("run flashcp pull")
}

/// `flashcp pull` that must succeed.
fn pull_ok(img: &Path, dest: &Path, extra: &[&str]) -> Output {
    let out = pull_cmd(img, dest, extra);
    assert_success(&out, "flashcp pull");
    out
}

/// `flashcp pull` that must fail, with every needle in stderr.
fn pull_fails(img: &Path, dest: &Path, extra: &[&str], needles: &[&str]) {
    let out = pull_cmd(img, dest, extra);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "flashcp pull unexpectedly succeeded\nstdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    for n in needles {
        assert!(err.contains(n), "stderr does not contain {n:?}:\n{err}");
    }
}

/// Builds `src` onto a fresh zeroed image of `img_bytes` bytes with the given build options.
fn build_image(src: &Path, img: &Path, img_bytes: u64, extra: &[&str]) {
    make_image(img, img_bytes);
    let out = run_build(src, img, extra);
    assert_success(&out, "flashcp build");
}

/// Deterministic pseudo-random bytes (same seed, same bytes).
fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    XorShift(seed | 1).fill(&mut buf);
    buf
}

/// Every node under `dir` as (is_dir, bytes), or an empty map when `dir` does not exist.
/// Used to show that a failed or refused pull left the destination exactly as it was.
fn tree_state(dir: &Path) -> BTreeMap<String, (bool, Vec<u8>)> {
    if !dir.exists() {
        return BTreeMap::new();
    }
    read_source_tree(dir).into_iter().map(|(k, v)| (k, (v.is_dir, v.bytes))).collect()
}

/// Asserts that `dest` holds exactly the tree at `src`: the same paths, types, file bytes and
/// file mtimes (sec and ns at the 10 ms exFAT granularity). Directory mtimes are compared only
/// when `dir_mtimes` is set. Extra entries on either side are failures.
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
            let first = g.bytes.iter().zip(&w.bytes).position(|(a, b)| a != b);
            problems.push(format!(
                "content differs: {path}: want {} bytes, got {} bytes, first mismatch at {first:?}",
                w.bytes.len(),
                g.bytes.len()
            ));
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
        problems.iter().take(30).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// Byte offsets of every occurrence of `needle` in `hay`.
fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    hay.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect()
}

/// exFAT entry-set checksum (spec "EntrySetChecksum"): 16-bit rotate-right-by-one and add over
/// every byte, skipping bytes 2 and 3 (the checksum field itself).
fn entry_set_checksum(set: &[u8]) -> u16 {
    set.iter()
        .enumerate()
        .fold(0u16, |c, (i, &b)| if i == 2 || i == 3 { c } else { c.rotate_right(1).wrapping_add(b as u16) })
}

/// Non-directory entries of a source tree, and their total byte count.
fn file_count_and_bytes(src: &Path) -> (u64, u64) {
    let tree = read_source_tree(src);
    let files = tree.values().filter(|s: &&SrcNode| !s.is_dir);
    (files.clone().count() as u64, files.map(|s| s.bytes.len() as u64).sum())
}

/// Value of a flat-object field, without quotes. Works for numbers and strings.
fn json_field<'a>(obj: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{key}\":");
    let start = obj.find(&pat).unwrap_or_else(|| panic!("no field {key:?} in {obj}")) + pat.len();
    let rest = &obj[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim_matches('"')
}

// ───────────────────────── 1. round trips of the tricky tree ─────────────────────────

/// Build the tricky tree with `extra`, pull it, and compare: content, types, empty entries,
/// mtimes (directories included), and an image that did not change.
fn tricky_round_trip(name: &str, extra: &[&str]) {
    let dir = test_dir(name);
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    build_image(&src, &img, 96 * MB, extra);
    let image_before = fs::read(&img).expect("read image");

    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);

    assert_same_tree(&src, &dest, true);
    assert!(fs::read(&img).expect("read image") == image_before, "pull changed the image");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn pull_tricky_tree_round_trip() {
    tricky_round_trip("pull_tricky_prefix", &["--fat", "prefix", "--cluster", "4096"]);
}

#[test]
fn pull_tricky_tree_round_trip_fat_full() {
    tricky_round_trip("pull_tricky_full", &["--fat", "full", "--cluster", "4096"]);
}

// ───────────────────────── 2. --json events ─────────────────────────

#[test]
fn pull_json_plan_progress_and_done() {
    let dir = test_dir("pull_json");
    let src = dir.join("src");
    gen_dataset("tiny")(&src);
    let img = dir.join("disk.img");
    build_image(&src, &img, 96 * MB, &[]);
    let dest = dir.join("out");

    let out = pull_ok(&img, &dest, &["--json"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 2, "expected at least a plan and a done line, got:\n{text}");
    for l in &lines {
        assert!(l.starts_with("{\"event\":\"") && l.ends_with('}'), "stdout line is not a JSON event: {l}");
    }

    let plan = lines[0];
    assert!(plan.contains("\"event\":\"plan\""), "first line is not a plan event: {plan}");
    assert!(plan.contains("\"op\":\"pull\""), "plan line does not say op pull: {plan}");

    let done = lines[lines.len() - 1];
    assert!(done.contains("\"event\":\"done\""), "last line is not a done event: {done}");
    let (files, bytes) = file_count_and_bytes(&src);
    assert_eq!(json_field(done, "files"), files.to_string(), "done.files: {done}");
    assert_eq!(json_field(done, "bytes"), bytes.to_string(), "done.bytes: {done}");
    assert!(json_field(done, "secs").parse::<f64>().is_ok(), "done.secs is not a number: {done}");

    assert_same_tree(&src, &dest, true);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 3. --from ─────────────────────────

#[test]
fn pull_from_subtree_is_contents_only_and_case_insensitive() {
    let dir = test_dir("pull_from");
    let src = dir.join("src");
    write_file(&src.join("top.txt"), b"top level\n");
    write_file(&src.join("Sub/other.txt"), b"sibling of Dir\n");
    write_file(&src.join("Sub/Dir/a.txt"), b"a\n");
    write_file(&src.join("Sub/Dir/nested/b.bin"), &pattern(31, 70_000));
    write_file(&src.join("Sub/Dir/nested/deeper/c.txt"), b"c\n");
    fs::create_dir_all(src.join("Sub/Dir/empty")).expect("empty dir");
    let img = dir.join("disk.img");
    build_image(&src, &img, 64 * MB, &[]);

    // The contents of Sub/Dir land directly in the destination, not under a "Dir" folder.
    let dest = dir.join("out");
    pull_ok(&img, &dest, &["--from", "sub/dir"]);
    assert_same_tree(&src.join("Sub/Dir"), &dest, true);

    // Same subtree, spelled with other case.
    let dest2 = dir.join("out-upper");
    pull_ok(&img, &dest2, &["--from", "SUB/DIR"]);
    assert_same_tree(&src.join("Sub/Dir"), &dest2, true);

    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn pull_from_missing_or_file_fails() {
    let dir = test_dir("pull_from_bad");
    let src = dir.join("src");
    write_file(&src.join("top.txt"), b"top level\n");
    write_file(&src.join("Sub/Dir/a.txt"), b"a\n");
    let img = dir.join("disk.img");
    build_image(&src, &img, 64 * MB, &[]);

    // A --from path that is not on the volume.
    pull_fails(&img, &dir.join("out-missing"), &["--from", "no/such/dir"], &["--from"]);
    // A --from path that names a file, not a directory.
    pull_fails(&img, &dir.join("out-file"), &["--from", "top.txt"], &["--from"]);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 4. collisions ─────────────────────────

#[test]
fn pull_collisions_error_skip_and_overwrite() {
    let dir = test_dir("pull_collisions");
    let src = dir.join("src");
    write_file(&src.join("clash.txt"), b"SOURCE clash\n");
    write_file(&src.join("other.txt"), b"other\n");
    write_file(&src.join("sub/new.txt"), b"new\n");
    let img = dir.join("disk.img");
    build_image(&src, &img, 64 * MB, &[]);

    let dest = dir.join("out");
    write_file(&dest.join("clash.txt"), b"PRE existing\n");
    write_file(&dest.join("unrelated.txt"), b"keep me\n");
    let before = tree_state(&dest);

    // Default: an existing destination file is an error, and nothing at all is written.
    pull_fails(&img, &dest, &[], &["already exist", "--skip-existing"]);
    assert!(tree_state(&dest) == before, "a refused pull changed the destination");
    assert!(!dest.join("other.txt").exists(), "other.txt was written by a refused pull");
    assert!(!dest.join("sub").exists(), "sub/ was created by a refused pull");

    // --skip-existing: the existing file is kept as it was, everything else is pulled.
    pull_ok(&img, &dest, &["--skip-existing"]);
    assert_eq!(fs::read(dest.join("clash.txt")).expect("read clash"), b"PRE existing\n", "--skip-existing replaced");
    assert_eq!(fs::read(dest.join("unrelated.txt")).expect("read unrelated"), b"keep me\n");
    assert_eq!(fs::read(dest.join("other.txt")).expect("read other"), b"other\n");
    assert_eq!(fs::read(dest.join("sub/new.txt")).expect("read new"), b"new\n");

    // --overwrite: the existing file is replaced with the volume's content.
    pull_ok(&img, &dest, &["--overwrite"]);
    assert_eq!(
        fs::read(dest.join("clash.txt")).expect("read clash"),
        b"SOURCE clash\n",
        "--overwrite kept the old file"
    );
    assert_eq!(fs::read(dest.join("unrelated.txt")).expect("read unrelated"), b"keep me\n");
    // Apart from the file only the destination had, it now matches the volume exactly.
    fs::remove_file(dest.join("unrelated.txt")).expect("remove unrelated");
    assert_same_tree(&src, &dest, false);

    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn pull_merges_into_existing_directory() {
    let dir = test_dir("pull_merge");
    let src = dir.join("src");
    write_file(&src.join("sub/new.txt"), b"new\n");
    write_file(&src.join("sub/inner/deep.txt"), b"deep\n");
    let img = dir.join("disk.img");
    build_image(&src, &img, 64 * MB, &[]);

    // The destination already has sub/ with a file the volume does not have.
    let dest = dir.join("out");
    write_file(&dest.join("sub/keep.txt"), b"keep\n");

    pull_ok(&img, &dest, &[]);
    assert_eq!(fs::read(dest.join("sub/keep.txt")).expect("read keep"), b"keep\n", "existing file lost in merge");
    assert_eq!(fs::read(dest.join("sub/new.txt")).expect("read new"), b"new\n");
    assert_eq!(fs::read(dest.join("sub/inner/deep.txt")).expect("read deep"), b"deep\n");

    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 5. ValidDataLength shorter than DataLength ─────────────────────────

const VDL_NAME: &str = "vdl-test.bin";

/// The file's DataLength is 3 clusters and 100 bytes. The image is patched so ValidDataLength is
/// 5000 and the entry-set checksum is recomputed. Pull must output the full length, the first
/// 5000 bytes of data, and zeros after that.
#[test]
fn pull_honours_valid_data_length() {
    let dir = test_dir("pull_vdl");
    let src = dir.join("src");
    let len = 3 * 4096 + 100;
    write_file(&src.join(VDL_NAME), &pattern(4242, len));
    write_file(&src.join("other.txt"), b"x");
    let img = dir.join("disk.img");
    build_image(&src, &img, 32 * MB, &["--cluster", "4096"]);

    // Locate the file's entry set through its name: the 0xC1 name entry is at set+64 and its
    // characters start at set+66, so the set begins 66 bytes before the first name character.
    let name: Vec<u8> = VDL_NAME.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut bytes = fs::read(&img).expect("read image");
    let hits = find_all(&bytes, &name);
    assert_eq!(hits.len(), 1, "expected exactly one UTF-16LE occurrence of {VDL_NAME}");
    let set = hits[0] - 66;
    assert_eq!(bytes[set], 0x85, "entry set does not start with a file entry");
    assert_eq!(bytes[set + 32], 0xC0, "no stream extension after the file entry");
    assert_eq!(bytes[set + 64], 0xC1, "no name entry at set+64");
    let set_len = 32 * (1 + bytes[set + 1] as usize);
    let data_len = u64::from_le_bytes(bytes[set + 56..set + 64].try_into().expect("8 bytes"));
    assert_eq!(data_len, len as u64, "DataLength of the stored file");

    bytes[set + 40..set + 48].copy_from_slice(&5000u64.to_le_bytes());
    let sum = entry_set_checksum(&bytes[set..set + set_len]);
    bytes[set + 2..set + 4].copy_from_slice(&sum.to_le_bytes());
    fs::write(&img, &bytes).expect("write patched image");

    let dest = dir.join("out");
    pull_ok(&img, &dest, &[]);
    let got = fs::read(dest.join(VDL_NAME)).expect("read pulled file");
    assert_eq!(got.len(), len, "pulled length must be DataLength, not ValidDataLength");
    let want = pattern(4242, len);
    assert!(got[..5000] == want[..5000], "bytes before ValidDataLength differ");
    assert!(got[5000..].iter().all(|&b| b == 0), "bytes past ValidDataLength are not zero");
    assert_eq!(fs::read(dest.join("other.txt")).expect("read other"), b"x");

    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 6. no writes through symlinks ─────────────────────────

/// The destination holds a symlink where the volume has a directory. Pull must fail and must not
/// write anything into the symlink's target.
#[test]
fn pull_refuses_to_write_through_symlink() {
    let dir = test_dir("pull_symlink");
    let src = dir.join("src");
    write_file(&src.join("deep/inner/file.txt"), b"inside\n");
    write_file(&src.join("plain.txt"), b"plain\n");
    let img = dir.join("disk.img");
    build_image(&src, &img, 64 * MB, &[]);

    let outside = dir.join("outside");
    fs::create_dir_all(&outside).expect("create outside dir");
    let dest = dir.join("out");
    fs::create_dir_all(&dest).expect("create dest");
    std::os::unix::fs::symlink(&outside, dest.join("deep")).expect("create symlink");

    let out = pull_cmd(&img, &dest, &[]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "pull succeeded through a symlink at deep/");
    assert!(!err.contains("unrecognized"), "pull is not implemented (CLI rejected the subcommand):\n{err}");
    let entries: Vec<_> = fs::read_dir(&outside).expect("read outside").collect();
    assert!(entries.is_empty(), "pull wrote through the symlink into {}", outside.display());

    // Positive control: the same image pulls cleanly into a directory without the symlink.
    let clean = dir.join("clean");
    pull_ok(&img, &clean, &[]);
    assert_same_tree(&src, &clean, true);

    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── 7. non-exFAT input is refused ─────────────────────────

/// Garbage and empty inputs must be refused with a message on stderr. The destination must not
/// be created, and the input must not change.
#[test]
fn pull_refuses_non_exfat_input() {
    let dir = test_dir("pull_garbage");
    let mut garbage = vec![0u8; MB as usize];
    XorShift(0xDEAD_BEEF_0BAD_F00D).fill(&mut garbage);
    let cases: [(&str, Vec<u8>); 2] = [("garbage.img", garbage), ("empty.img", Vec::new())];
    for (name, content) in cases {
        let img = dir.join(name);
        fs::write(&img, &content).expect("write input");
        let dest = dir.join(format!("out-{name}"));

        let out = pull_cmd(&img, &dest, &[]);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!err.contains("unrecognized"), "pull is not implemented (CLI rejected the subcommand):\n{err}");
        assert!(!out.status.success(), "{name}: pull accepted a non-exFAT input");
        assert!(!err.trim().is_empty(), "{name}: no error message on stderr");
        assert!(!dest.exists(), "{name}: destination was created for a refused input");
        assert!(fs::read(&img).expect("read input") == content, "{name}: input changed");
    }
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}
