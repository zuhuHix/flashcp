//! The cluster-size policy end to end: sub-4 KB clusters only when flashcp fills a small volume
//! with tiny files, 4 KB otherwise, and an explicit --cluster always wins. Every volume must
//! pass `fsck.exfat -n` and read back exactly through the independent parser.

#[allow(dead_code)]
mod common;

use common::{MB, assert_success, make_image, round_trip, run_build, test_dir, write_file};
use std::fs;
use std::path::Path;

/// 2400 files of 200 … 1500 bytes in 24 directories (about 2 MB of data): at 4 KB clusters
/// that is ~2430 clusters, ≥ 50 % of a 16 MB volume's heap, with slack far above 15 %.
fn tiny_files(root: &Path) {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for d in 0..24 {
        for f in 0..100 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = 200 + (x % 1300) as usize;
            let body: Vec<u8> = (0..len).map(|i| (i as u64 ^ x) as u8).collect();
            write_file(&root.join(format!("d{d:02}/f{f:03}.txt")), &body);
        }
    }
}

fn plan_line(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).lines().find(|l| l.contains("plan:")).unwrap_or_default().to_string()
}

#[test]
fn full_small_volume_of_tiny_files_gets_sub_4k_clusters() {
    let vol = round_trip("cost_tiny_full", 16 * MB, false, &[], tiny_files);
    assert!(vol.info.cluster_size < 4096, "expected a sub-4K cluster, got {}", vol.info.cluster_size);
}

#[test]
fn same_files_on_a_mostly_empty_volume_stay_at_4k_or_more() {
    // 128 MB: the files use ~8 % of the heap at 4 KB, so other systems keep room for big files.
    let vol = round_trip("cost_tiny_empty", 128 * MB, false, &[], tiny_files);
    assert!(vol.info.cluster_size >= 4096, "sub-4K cluster on a mostly empty volume: {}", vol.info.cluster_size);
}

#[test]
fn big_files_on_a_full_volume_stay_at_4k_or_more() {
    // Full, but the data is not tiny files (slack at 4 KB ≪ 15 %).
    let vol = round_trip("cost_big_full", 16 * MB, false, &[], |root| {
        for k in 0..4 {
            write_file(&root.join(format!("b{k}.bin")), &vec![k as u8 + 1; (2 * MB) as usize]);
        }
    });
    assert!(vol.info.cluster_size >= 4096, "sub-4K cluster for big files: {}", vol.info.cluster_size);
}

#[test]
fn explicit_cluster_overrides_the_policy() {
    let vol = round_trip("cost_tiny_explicit", 16 * MB, false, &["--cluster", "4096"], tiny_files);
    assert_eq!(vol.info.cluster_size, 4096);
}

#[test]
fn plan_line_explains_the_cluster_choice() {
    let dir = test_dir("cost_plan_line");
    let src = dir.join("src");
    tiny_files(&src);
    let img = dir.join("disk.img");
    make_image(&img, 16 * MB);
    let out = run_build(&src, &img, &["--dry-run"]);
    assert_success(&out, "flashcp build --dry-run");
    let line = plan_line(&out);
    assert!(line.contains("auto: est.") && line.contains("sub-4K allowed"), "plan line: {line}");
    make_image(&img, 128 * MB);
    let line = plan_line(&run_build(&src, &img, &["--dry-run"]));
    assert!(line.contains("sub-4K: data fills"), "plan line: {line}");
    let line = plan_line(&run_build(&src, &img, &["--dry-run", "--cluster", "8192"]));
    assert!(line.contains("set by --cluster"), "plan line: {line}");
    fs::remove_dir_all(&dir).expect("remove scratch dir");
}
