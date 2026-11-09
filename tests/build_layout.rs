//! Layout and crash-safety checks for `flashcp build`.
//!
//! Images whose length is not a multiple of 4 KB must keep their exact length and still read
//! back clean. The main boot sector must be clean after a successful build, and dirty after a
//! build that was killed mid-stream.

#[allow(dead_code)]
mod common;

use common::exfat_reader::read_volume;
use common::{MB, assert_success, compare_trees, flashcp, fsck_clean, make_image, make_tricky_tree, read_source_tree};
use common::{XorShift, run_build, test_dir, write_file};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

const BOOT_REGION: usize = 12 * 512;
/// VolumeFlags bit 1 (VolumeDirty).
const VOLUME_DIRTY: u16 = 0x0002;

/// Reads the first `n` bytes of a file.
fn head(path: &Path, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    File::open(path).expect("open image").read_exact(&mut buf).expect("read image head");
    buf
}

fn volume_flags(boot_sector: &[u8]) -> u16 {
    u16::from_le_bytes([boot_sector[106], boot_sector[107]])
}

// ───────────────────────── odd image sizes ─────────────────────────

/// Builds the tricky tree onto an image of `img_bytes` (not a multiple of 4 KB) and checks the
/// length is unchanged, fsck is clean, and the read-back equals the source.
fn odd_size_round_trip(img_bytes: u64, cluster: &str) {
    let name = format!("odd_{img_bytes}_{cluster}");
    let dir = test_dir(&name);
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, img_bytes);

    let out = run_build(&src, &img, &["--cluster", cluster]);
    assert_success(&out, &format!("flashcp build {name}"));
    let len = fs::metadata(&img).expect("stat image").len();
    assert_eq!(len, img_bytes, "{name}: the image length changed");
    fsck_clean(&img);

    let vol = read_volume(&img).unwrap_or_else(|e| panic!("read-back of {name} failed: {e}"));
    let problems = compare_trees(&read_source_tree(&src), &vol);
    assert!(problems.is_empty(), "{name}: read-back differs from the source:\n{}", problems.join("\n"));
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn odd_size_plus_1000_bytes() {
    for cluster in ["512", "4096", "131072"] {
        odd_size_round_trip(64 * MB + 1000, cluster);
    }
}

#[test]
fn odd_size_plus_512_bytes() {
    for cluster in ["512", "4096", "131072"] {
        odd_size_round_trip(64 * MB + 512, cluster);
    }
}

#[test]
fn odd_size_plus_4095_bytes() {
    for cluster in ["512", "4096", "131072"] {
        odd_size_round_trip(64 * MB + 4095, cluster);
    }
}

// ───────────────────────── boot sector after a clean build ─────────────────────────

#[test]
fn clean_build_marks_volume_clean_and_backup_matches() {
    for cluster in ["512", "4096"] {
        let name = format!("boot_clean_{cluster}");
        let dir = test_dir(&name);
        let src = dir.join("src");
        make_tricky_tree(&src);
        let img = dir.join("disk.img");
        make_image(&img, 64 * MB + 1000);
        let out = run_build(&src, &img, &["--cluster", cluster]);
        assert_success(&out, &format!("flashcp build {name}"));

        let raw = head(&img, 2 * BOOT_REGION);
        assert_eq!(&raw[510..512], &[0x55, 0xAA], "{name}: no boot signature");
        assert_eq!(volume_flags(&raw[..512]), 0, "{name}: VolumeFlags of the main boot sector");
        assert!(raw[..BOOT_REGION] == raw[BOOT_REGION..], "{name}: main and backup boot regions differ");
        fs::remove_dir_all(&dir).expect("remove scratch dir");
    }
}

// ───────────────────────── interrupted build ─────────────────────────

#[test]
fn interrupted_build_leaves_volume_dirty() {
    let dir = test_dir("abort_dirty");
    let src = dir.join("src");
    // 40 MB in one file: the writer hands out 8 MB buffers, and the test hook fires after the
    // first of them has been queued. The first buffer carries the boot region at offset 0.
    write_file(&src.join("big.bin"), &random_bytes(40 * MB as usize, 0xDEAD_BEEF_0042_1337));
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);

    let out = flashcp()
        .arg("build")
        .arg(&src)
        .arg(&img)
        .arg("--yes-erase")
        .env("FLASHCP_TEST_BUILD_ABORT_AT", "1")
        .output()
        .expect("run flashcp build");
    assert!(!out.status.success(), "the aborted build reported success");

    let boot = head(&img, 512);
    assert_eq!(&boot[510..512], &[0x55, 0xAA], "no boot sector reached the image; the test needs a flushed buffer");
    let flags = volume_flags(&boot);
    assert!(flags & VOLUME_DIRTY != 0, "interrupted build left VolumeFlags {flags:#06x}, not dirty");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// Pseudo-random bytes from a fixed seed.
fn random_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    XorShift(seed).fill(&mut buf);
    buf
}
