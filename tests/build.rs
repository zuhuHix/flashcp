//! Integration tests for `flashcp build`.
//!
//! Every volume is produced by the real binary, then checked twice: `fsck.exfat -n` must call it
//! clean, and the independent parser in tests/common/exfat_reader.rs must read back exactly the
//! source tree (names, types, bytes, mtimes at 10 ms resolution).

mod common;

use common::exfat_reader::Volume;
use common::{
    MB, assert_all_zero, assert_success, flashcp, gen_dataset, make_image, make_tricky_tree, round_trip, run_build,
    test_dir, write_file,
};
use std::fs;
use std::path::Path;

const IMG: u64 = 128 * MB;

fn tricky_case(name: &str, extra: &[&str], expect_cluster: Option<u64>) {
    let vol = round_trip(name, IMG, false, extra, make_tricky_tree);
    if let Some(c) = expect_cluster {
        assert_eq!(vol.info.cluster_size, c, "{name}: cluster size on the volume");
    }
}

// ───────────────────────── round trips: tricky tree ─────────────────────────

#[test]
fn tricky_prefix_cluster_4k() {
    tricky_case("tricky_prefix_4k", &["--fat", "prefix", "--cluster", "4096"], Some(4096));
}

#[test]
fn tricky_prefix_cluster_32k() {
    tricky_case("tricky_prefix_32k", &["--fat", "prefix", "--cluster", "32768"], Some(32768));
}

#[test]
fn tricky_prefix_cluster_128k() {
    tricky_case("tricky_prefix_128k", &["--fat", "prefix", "--cluster", "131072"], Some(131072));
}

#[test]
fn tricky_prefix_cluster_auto() {
    tricky_case("tricky_prefix_auto", &["--fat", "prefix"], None);
}

#[test]
fn tricky_full_cluster_4k() {
    tricky_case("tricky_full_4k", &["--fat", "full", "--cluster", "4096"], Some(4096));
}

#[test]
fn tricky_full_cluster_32k() {
    tricky_case("tricky_full_32k", &["--fat", "full", "--cluster", "32768"], Some(32768));
}

#[test]
fn tricky_full_cluster_128k() {
    tricky_case("tricky_full_128k", &["--fat", "full", "--cluster", "131072"], Some(131072));
}

#[test]
fn tricky_full_cluster_auto() {
    tricky_case("tricky_full_auto", &["--fat", "full"], None);
}

// ───────────────────────── round trips: generated dataset ─────────────────────────

#[test]
fn gen_tiny_prefix_cluster_auto() {
    round_trip("gen_tiny_prefix", IMG, false, &["--fat", "prefix"], gen_dataset("tiny"));
}

#[test]
fn gen_tiny_full_cluster_auto() {
    round_trip("gen_tiny_full", IMG, false, &["--fat", "full"], gen_dataset("tiny"));
}

// ───────────────────────── junk-filled target ─────────────────────────

#[test]
fn junk_image_prefix_is_fully_rewritten() {
    // 64 MB of pseudo-random bytes stands in for a used stick: the FAT tail and every free
    // cluster start as garbage, and the build must still leave a clean volume.
    round_trip("junk_prefix", 64 * MB, true, &["--fat", "prefix"], make_tricky_tree);
}

// ───────────────────────── label ─────────────────────────

#[test]
fn label_is_written_to_root_entry() {
    let vol: Volume = round_trip("label_my_usb", IMG, false, &["--label", "MY USB"], make_tricky_tree);
    assert_eq!(vol.label, "MY USB");
}

// ───────────────────────── dry run ─────────────────────────

#[test]
fn dry_run_leaves_image_untouched() {
    let dir = test_dir("dry_run");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    let out =
        flashcp().arg("build").arg(&src).arg(&img).arg("--dry-run").output().expect("run flashcp build --dry-run");
    assert_success(&out, "flashcp build --dry-run");
    assert!(String::from_utf8_lossy(&out.stdout).contains("plan:"), "dry run printed no plan");
    assert_eq!(fs::metadata(&img).expect("stat image").len(), 64 * MB, "dry run changed the image length");
    assert_all_zero(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

// ───────────────────────── error paths ─────────────────────────

/// Runs a build that must fail, and checks that stderr carries `needle`.
fn expect_build_error(name: &str, img_bytes: u64, make_src: impl FnOnce(&Path), extra: &[&str], needle: &str) {
    let dir = test_dir(name);
    let src = dir.join("src");
    make_src(&src);
    let img = dir.join("disk.img");
    make_image(&img, img_bytes);
    let out = run_build(&src, &img, extra);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{name}: build unexpectedly succeeded");
    assert!(err.contains(needle), "{name}: stderr does not contain {needle:?}:\n{err}");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

fn one_small_file(src: &Path) {
    write_file(&src.join("hello.txt"), b"hello\n");
}

#[test]
fn err_data_does_not_fit() {
    expect_build_error(
        "err_does_not_fit",
        8 * MB,
        |src| write_file(&src.join("big.bin"), &vec![0x5A; (10 * MB) as usize]),
        &[],
        "does not fit",
    );
}

#[test]
fn err_image_too_small() {
    expect_build_error("err_too_small", MB, make_small_tree, &[], "does not fit");
}

/// Two files and a subdirectory: small enough for a 1 MB image to be hopeless.
fn make_small_tree(src: &Path) {
    one_small_file(src);
    write_file(&src.join("sub/inner.txt"), b"inner\n");
}

#[test]
fn err_empty_image_file() {
    expect_build_error("err_empty_image", 0, one_small_file, &[], "image file is empty");
}

#[test]
fn err_cluster_3000_not_power_of_two() {
    expect_build_error("err_cluster_3000", 16 * MB, one_small_file, &["--cluster", "3000"], "power of two");
}

#[test]
fn err_cluster_100_below_sector() {
    // Regression: a sub-sector cluster used to panic or write a corrupt volume.
    expect_build_error("err_cluster_100", 16 * MB, one_small_file, &["--cluster", "100"], "power of two");
}

#[test]
fn err_missing_yes_erase() {
    let dir = test_dir("err_missing_yes_erase");
    let src = dir.join("src");
    one_small_file(&src);
    let img = dir.join("disk.img");
    make_image(&img, 16 * MB);
    let out = flashcp().arg("build").arg(&src).arg(&img).output().expect("run flashcp build");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "build without --yes-erase unexpectedly succeeded");
    assert!(err.contains("--yes-erase"), "stderr does not mention --yes-erase:\n{err}");
    assert_all_zero(&img);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

#[test]
fn err_names_differing_only_by_case() {
    expect_build_error(
        "err_case_dup",
        16 * MB,
        |src| {
            write_file(&src.join("a.txt"), b"lower");
            write_file(&src.join("A.TXT"), b"upper");
        },
        &[],
        "differ only by case",
    );
}

#[test]
fn err_colon_in_name() {
    expect_build_error("err_colon", 16 * MB, |src| write_file(&src.join("a:b.txt"), b"colon"), &[], "not allowed");
}

#[test]
fn err_label_longer_than_11_chars() {
    expect_build_error("err_label_long", 16 * MB, one_small_file, &["--label", "ABCDEFGHIJKL"], "11 characters");
}

// ───────────────────────── mount (gated) ─────────────────────────

fn run_checked(prog: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(prog).args(args).output().unwrap_or_else(|e| panic!("cannot run {prog}: {e}"));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{prog} {args:?} failed:\n{text}");
    text
}

/// First whitespace-separated token starting with `prefix`, with trailing punctuation removed.
fn token_with(text: &str, prefix: &str) -> Option<String> {
    text.split_whitespace().find(|w| w.starts_with(prefix)).map(|w| w.trim_end_matches(['.', ',']).to_string())
}

#[test]
fn mount_roundtrip_gated() {
    if std::env::var("FLASHCP_MOUNT_TESTS").as_deref() != Ok("1") {
        eprintln!("SKIP mount test: set FLASHCP_MOUNT_TESTS=1 to run it");
        return;
    }
    let dir = test_dir("mount_roundtrip");
    let src = dir.join("src");
    make_tricky_tree(&src);
    let img = dir.join("disk.img");
    make_image(&img, IMG);
    assert_success(&run_build(&src, &img, &[]), "flashcp build");
    let img_s = img.to_str().expect("UTF-8 path").to_string();
    let mnt_root = dir.join("mnt");
    fs::create_dir_all(&mnt_root).expect("create mount point");

    let is_root = run_checked("id", &["-u"]).trim() == "0";
    // Each branch returns the diff result; cleanup runs whether or not the diff passed.
    let result: Result<(), String> = if is_root {
        let mnt = mnt_root.to_str().expect("UTF-8 path").to_string();
        run_checked("mount", &["-o", "loop,ro", &img_s, &mnt]);
        let diff = std::process::Command::new("diff").arg("-r").arg(&src).arg(&mnt).output();
        let _ = std::process::Command::new("umount").arg(&mnt).output();
        match diff {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!("diff -r failed:\n{}", String::from_utf8_lossy(&o.stdout))),
            Err(e) => Err(format!("cannot run diff: {e}")),
        }
    } else {
        let setup = run_checked("udisksctl", &["loop-setup", "-f", &img_s, "--no-user-interaction"]);
        let Some(dev) = token_with(&setup, "/dev/loop") else {
            panic!("no loop device in udisksctl output:\n{setup}");
        };
        let outcome = (|| -> Result<(), String> {
            let mounted = std::process::Command::new("udisksctl")
                .args(["mount", "-b", &dev, "--no-user-interaction"])
                .output()
                .map_err(|e| format!("cannot run udisksctl: {e}"))?;
            let text =
                format!("{}{}", String::from_utf8_lossy(&mounted.stdout), String::from_utf8_lossy(&mounted.stderr));
            if !mounted.status.success() {
                return Err(format!("udisksctl mount failed:\n{text}"));
            }
            let Some(at) = text.split(" at ").nth(1).map(|s| s.trim().trim_end_matches('.').to_string()) else {
                return Err(format!("no mount point in udisksctl output:\n{text}"));
            };
            let diff = std::process::Command::new("diff")
                .arg("-r")
                .arg(&src)
                .arg(&at)
                .output()
                .map_err(|e| format!("cannot run diff: {e}"))?;
            if diff.status.success() {
                Ok(())
            } else {
                Err(format!("diff -r failed:\n{}", String::from_utf8_lossy(&diff.stdout)))
            }
        })();
        let _ = std::process::Command::new("udisksctl").args(["unmount", "-b", &dev, "--no-user-interaction"]).output();
        let _ =
            std::process::Command::new("udisksctl").args(["loop-delete", "-b", &dev, "--no-user-interaction"]).output();
        outcome
    };
    result.expect("mounted volume differs from the source tree");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// Detaches the loop device (and unmounts `mnt` if mounted) even when an assertion fails.
struct LoopDev {
    dev: String,
    mnt: std::path::PathBuf,
}

impl Drop for LoopDev {
    fn drop(&mut self) {
        let _ = std::process::Command::new("umount").arg(&self.mnt).output();
        let _ = std::process::Command::new("losetup").args(["-d", &self.dev]).output();
    }
}

/// Building onto a block device that is claimed (O_EXCL by another process) or mounted must be
/// refused. Needs root for losetup/mount, so it only runs with FLASHCP_MOUNT_TESTS=1 as root
/// (CI runs the test binary under sudo).
#[test]
fn mount_busy_device_is_refused() {
    if std::env::var("FLASHCP_MOUNT_TESTS").as_deref() != Ok("1") {
        eprintln!("SKIP busy-device test: set FLASHCP_MOUNT_TESTS=1 to run it");
        return;
    }
    if run_checked("id", &["-u"]).trim() != "0" {
        eprintln!("SKIP busy-device test: needs root (losetup, mount, writing /dev/loopN)");
        return;
    }
    let dir = test_dir("busy_device");
    let src = dir.join("src");
    write_file(&src.join("a.txt"), b"hello");
    let img = dir.join("disk.img");
    make_image(&img, 64 * MB);
    let img_s = img.to_str().expect("UTF-8 path").to_string();
    let dev = run_checked("losetup", &["-f", "--show", &img_s]).trim().to_string();
    let mnt = dir.join("mnt");
    fs::create_dir_all(&mnt).expect("create mount point");
    let guard = LoopDev { dev: dev.clone(), mnt: mnt.clone() };
    let build = || flashcp().arg("build").arg(&src).arg(&dev).arg("--yes-erase").output().expect("run flashcp");
    let stderr = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).to_string();

    // 1. Claimed exclusively by someone else (like an LVM PV or a mount would): EBUSY.
    {
        use std::os::unix::fs::OpenOptionsExt;
        const O_EXCL: i32 = 0o200; // Linux, all mainstream architectures
        let _claim = fs::OpenOptions::new().read(true).custom_flags(O_EXCL).open(&dev).expect("claim loop device");
        let out = build();
        assert!(!out.status.success(), "build onto a claimed device succeeded");
        assert!(stderr(&out).contains("is in use (mounted or held by LVM/dm/md)"), "stderr: {}", stderr(&out));
    }

    // 2. Mounted: refused, and the message names the mount point.
    assert_success(&build(), "build onto the free loop device");
    run_checked("mount", &["-o", "ro", &dev, mnt.to_str().expect("UTF-8 path")]);
    let out = build();
    assert!(!out.status.success(), "build onto a mounted device succeeded");
    assert!(stderr(&out).contains("is mounted at"), "stderr: {}", stderr(&out));
    run_checked("umount", &[mnt.to_str().expect("UTF-8 path")]);

    // 3. Free again: the build works and the volume is clean.
    assert_success(&build(), "build onto the released loop device");
    common::fsck_clean(Path::new(&dev));
    drop(guard);
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}
