//! `flashcp pull` against UNTRUSTED volumes: valid images built by `flashcp build`, then patched
//! into something hostile (entry sets edited and their checksums recomputed, FAT entries
//! rewritten), and destinations that change under pull (test hook between plan and write).
//! Pull must refuse every crafted volume before creating anything, never write outside the
//! destination, and never read past VolumeLength.

#[allow(dead_code)]
mod common;

use common::{MB, assert_success, flashcp, make_image, run_build, test_dir, write_file};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Output;

fn pull_cmd(img: &Path, dest: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = flashcp();
    c.arg("pull").arg(img).arg(dest).args(extra);
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("run flashcp pull")
}

fn pull_fails(img: &Path, dest: &Path, extra: &[&str], env: &[(&str, &str)], needle: &str) -> String {
    let out = pull_cmd(img, dest, extra, env);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "flashcp pull unexpectedly succeeded:\n{err}");
    assert!(err.contains(needle), "stderr does not contain {needle:?}:\n{err}");
    err
}

fn build(src: &Path, img: &Path, bytes: u64, extra: &[&str]) {
    make_image(img, bytes);
    assert_success(&run_build(src, img, extra), "flashcp build");
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// (FAT offset, heap offset, cluster size) from the boot sector.
fn geometry(img: &[u8]) -> (usize, usize, usize) {
    let bps = 1usize << img[108];
    (le32(img, 80) as usize * bps, le32(img, 88) as usize * bps, bps << img[109])
}

/// Offset of the one entry set whose name is `name` (its 0xC1 name entry starts at set+64).
fn find_set(img: &[u8], name: &str) -> usize {
    let needle: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let hits: Vec<usize> =
        img.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect();
    assert_eq!(hits.len(), 1, "expected one occurrence of {name}");
    let set = hits[0] - 66;
    assert!(img[set] == 0x85 && img[set + 32] == 0xC0 && img[set + 64] == 0xC1, "{name}: not an entry set");
    set
}

/// Edits the entry set of `name` with `f`, then recomputes its SetChecksum.
fn patch_set(img: &mut [u8], name: &str, f: impl FnOnce(&mut [u8])) {
    let set = find_set(img, name);
    let len = 32 * (1 + img[set + 1] as usize);
    f(&mut img[set..set + len]);
    let mut c: u16 = 0;
    for (i, &b) in img[set..set + len].iter().enumerate() {
        if i != 2 && i != 3 {
            c = c.rotate_right(1).wrapping_add(b as u16);
        }
    }
    img[set + 2..set + 4].copy_from_slice(&c.to_le_bytes());
}

fn first_cluster(set: &[u8]) -> u32 {
    le32(set, 52)
}

/// Every file name under `dir`, recursively.
fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        walk(dir).into_iter().map(|p| p.strip_prefix(dir).unwrap().display().to_string()).collect();
    v.sort();
    v
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            out.push(e.path());
            if e.file_type().unwrap().is_dir() {
                out.extend(walk(&e.path()));
            }
        }
    }
    out
}

// ───────────────────────── 1. lengths and chains ─────────────────────────

/// ValidDataLength 0 with an absurd DataLength: refused before anything is created (pull used
/// to skip the bounds checks for VDL 0 and preallocate the size).
#[test]
fn crafted_huge_data_length_is_refused() {
    let dir = test_dir("crafted_huge_len");
    write_file(&dir.join("src/big.bin"), &[7u8; 4096]);
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &["--cluster", "4096"]);
    let mut b = fs::read(&img).unwrap();
    patch_set(&mut b, "big.bin", |s| {
        s[40..48].copy_from_slice(&0u64.to_le_bytes()); // ValidDataLength
        s[56..64].copy_from_slice(&(1u64 << 50).to_le_bytes()); // DataLength: 1 PiB
    });
    fs::write(&img, &b).unwrap();
    let dest = dir.join("out");
    pull_fails(&img, &dest, &[], &[], "larger than the volume's cluster heap");
    assert!(!dest.exists(), "a refused pull created the destination");

    // Within the heap's capacity but past the file's own allocation: the whole allocation is
    // walked and must stay inside the heap.
    patch_set(&mut b, "big.bin", |s| s[56..64].copy_from_slice(&(30u64 * MB).to_le_bytes()));
    fs::write(&img, &b).unwrap();
    pull_fails(&img, &dest, &[], &[], "big.bin");
    assert!(!dest.exists());
}

/// A FAT chain that loops back on itself (with a large ValidDataLength, so it would otherwise
/// generate extents for more clusters than the volume holds).
#[test]
fn crafted_fat_loop_is_refused() {
    let dir = test_dir("crafted_fat_loop");
    write_file(&dir.join("src/loop.bin"), &[1u8; 3 * 4096]);
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &["--cluster", "4096"]);
    let mut b = fs::read(&img).unwrap();
    let (fat, _, cs) = geometry(&b);
    let mut first = 0;
    patch_set(&mut b, "loop.bin", |s| {
        first = first_cluster(s) as usize;
        s[33] &= !0x02; // NoFatChain off: follow the FAT
        let len = (4000 * cs) as u64; // far more clusters than the chain really has
        s[40..48].copy_from_slice(&len.to_le_bytes());
        s[56..64].copy_from_slice(&len.to_le_bytes());
    });
    // first → first+1 → first (a loop)
    b[fat + 4 * first..fat + 4 * first + 4].copy_from_slice(&((first + 1) as u32).to_le_bytes());
    b[fat + 4 * (first + 1)..fat + 4 * (first + 1) + 4].copy_from_slice(&(first as u32).to_le_bytes());
    fs::write(&img, &b).unwrap();
    let dest = dir.join("out");
    pull_fails(&img, &dest, &[], &[], "used twice");
    assert!(!dest.exists(), "a refused pull created the destination");
}

/// Two files sharing a cluster (cross-linked).
#[test]
fn crafted_cross_link_is_refused() {
    let dir = test_dir("crafted_cross_link");
    write_file(&dir.join("src/one.bin"), &[1u8; 4096]);
    write_file(&dir.join("src/two.bin"), &[2u8; 4096]);
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &["--cluster", "4096"]);
    let mut b = fs::read(&img).unwrap();
    let one = first_cluster(&b[find_set(&b, "one.bin")..]);
    patch_set(&mut b, "two.bin", |s| s[52..56].copy_from_slice(&one.to_le_bytes()));
    fs::write(&img, &b).unwrap();
    pull_fails(&img, &dir.join("out"), &[], &[], "used twice");
}

// ───────────────────────── 4/5. directory cycles and limits ─────────────────────────

/// A subdirectory whose entry points back at its parent's clusters: fails at once (not after
/// ClusterCount directories).
#[test]
fn crafted_directory_cycle_is_refused() {
    let dir = test_dir("crafted_dir_cycle");
    write_file(&dir.join("src/cyc-outer/cyc-inner/f.txt"), b"x");
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &["--cluster", "4096"]);
    let mut b = fs::read(&img).unwrap();
    let outer = find_set(&b, "cyc-outer");
    let (flags, first, len) = (b[outer + 33], first_cluster(&b[outer..]), b[outer + 56..outer + 64].to_vec());
    patch_set(&mut b, "cyc-inner", |s| {
        s[33] = flags;
        s[52..56].copy_from_slice(&first.to_le_bytes());
        s[40..48].copy_from_slice(&len);
        s[56..64].copy_from_slice(&len);
    });
    fs::write(&img, &b).unwrap();
    let dest = dir.join("out");
    pull_fails(&img, &dest, &[], &[], "directory cycle");
    assert!(!dest.exists(), "a refused pull created the destination");
}

#[test]
fn directories_nested_deeper_than_the_limit_are_refused() {
    let dir = test_dir("crafted_depth");
    let mut p = dir.join("src");
    for _ in 0..257 {
        p.push("d");
    }
    write_file(&p.join("leaf.txt"), b"deep");
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &[]);
    pull_fails(&img, &dir.join("out"), &[], &[], "nested more than 256 deep");
    // 256 levels are fine.
    let ok = dir.join("ok");
    let mut p = ok.join("src");
    for _ in 0..255 {
        p.push("d");
    }
    write_file(&p.join("leaf.txt"), b"deep");
    let img = ok.join("v.img");
    build(&ok.join("src"), &img, 32 * MB, &[]);
    assert_success(&pull_cmd(&img, &ok.join("out"), &[], &[]), "pull 256 levels");
}

// ───────────────────────── 2. destination changing under pull ─────────────────────────

/// Between planning and writing, a checked destination directory is swapped for a symlink to a
/// directory outside the destination. Pull must fail and write nothing outside.
#[test]
fn symlink_swapped_in_after_planning_is_not_followed() {
    let dir = test_dir("crafted_swap");
    write_file(&dir.join("src/sub/inner/x.txt"), b"x");
    write_file(&dir.join("src/sub/y.txt"), b"y");
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &[]);
    let dest = dir.join("out");
    let outside = dir.join("outside");
    fs::create_dir_all(dest.join("sub")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let hook =
        format!("mv '{d}/sub' '{d}/sub.real' && ln -s '{o}' '{d}/sub'", d = dest.display(), o = outside.display());
    pull_fails(&img, &dest, &[], &[("FLASHCP_TEST_AFTER_PLAN", &hook)], "sub");
    assert!(listing(&outside).is_empty(), "pull wrote through the symlink: {:?}", listing(&outside));
    assert!(listing(&dest.join("sub.real")).is_empty(), "pull wrote into the moved directory");
}

// ───────────────────────── 3. --overwrite and hard links ─────────────────────────

#[test]
fn overwrite_does_not_write_through_hard_links() {
    let dir = test_dir("crafted_hardlink");
    write_file(&dir.join("src/clash.txt"), b"from the volume\n");
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &[]);
    let dest = dir.join("out");
    write_file(&dest.join("clash.txt"), b"precious original\n");
    let link = dir.join("elsewhere.txt");
    fs::hard_link(dest.join("clash.txt"), &link).unwrap();
    let ino = fs::metadata(&link).unwrap().ino();

    assert_success(&pull_cmd(&img, &dest, &["--overwrite"], &[]), "pull --overwrite");
    assert_eq!(fs::read(dest.join("clash.txt")).unwrap(), b"from the volume\n");
    assert_eq!(fs::read(&link).unwrap(), b"precious original\n", "the other hard link was modified");
    assert_eq!(fs::metadata(&link).unwrap().ino(), ino);
    assert_ne!(fs::metadata(dest.join("clash.txt")).unwrap().ino(), ino, "replaced in place, not renamed over");
    assert_eq!(listing(&dest), vec!["clash.txt"], "temp files left behind");
}

// ───────────────────────── 6. unaligned volume end ─────────────────────────

/// A volume whose length is not a multiple of 4 KB, with its last cluster in use, followed by
/// junk in the same image. Every read must stay inside VolumeLength and the data must match.
#[test]
fn reads_never_pass_an_unaligned_volume_end() {
    let dir = test_dir("crafted_unaligned_end");
    let src = dir.join("src");
    let vol = 8 * MB + 1536;
    let img = dir.join("v.img");
    // Size the one file to fill the heap exactly, so it ends at the (unaligned) volume end.
    write_file(&src.join("fill.bin"), &[0u8; 512]);
    // build itself never makes such a volume any more (it rounds down to 4 KB); a test hook
    // keeps the unaligned length, as other formatters may write it.
    let build_unaligned = |src: &Path| {
        make_image(&img, vol);
        let out = flashcp()
            .args(["build", "--cluster", "512", "--yes-erase"])
            .arg(src)
            .arg(&img)
            .env("FLASHCP_TEST_UNALIGNED_VOLUME", "1")
            .output()
            .unwrap();
        assert_success(&out, "flashcp build");
    };
    build_unaligned(&src);
    let info = flashcp().arg("info").arg(&img).arg("--json").output().unwrap();
    let json = String::from_utf8_lossy(&info.stdout).into_owned();
    let free: u64 = json.split("\"free\":").nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
    let len = 512 + free as usize;
    let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
    write_file(&src.join("fill.bin"), &data);
    build_unaligned(&src);
    // Junk after the volume, inside the same image.
    let mut b = fs::read(&img).unwrap();
    // VolumeLength (sectors at offset 72) is the unaligned size we asked for. build's last
    // aligned write may have extended the file past it; cut that off, then add the junk.
    assert_eq!(u64::from_le_bytes(b[72..80].try_into().unwrap()) * 512, vol);
    b.truncate(vol as usize);
    b.extend(std::iter::repeat_n(0xEEu8, MB as usize));
    fs::write(&img, &b).unwrap();

    let dest = dir.join("out");
    let out = pull_cmd(&img, &dest, &[], &[("FLASHCP_TEST_TRACE_READS", "1")]);
    assert_success(&out, "pull");
    assert!(fs::read(dest.join("fill.bin")).unwrap() == data, "pulled data differs");
    let err = String::from_utf8_lossy(&out.stderr);
    let reads: Vec<(u64, u64)> = err
        .lines()
        .filter_map(|l| l.strip_prefix("flashcp-read "))
        .map(|l| {
            let mut it = l.split(' ').map(|n| n.parse::<u64>().unwrap());
            (it.next().unwrap(), it.next().unwrap())
        })
        .collect();
    assert!(!reads.is_empty(), "no reads traced:\n{err}");
    for (off, len) in &reads {
        assert!(off + len <= vol, "read {off}+{len} passes VolumeLength {vol}");
    }
    assert!(reads.iter().any(|(off, len)| off + len == vol), "the unaligned tail was never read: {reads:?}");
}

// ───────────────────────── read strategy ─────────────────────────

/// The plan event names the read strategy; the hidden tuning flags override it.
#[test]
fn plan_reports_the_read_strategy_and_overrides_apply() {
    let dir = test_dir("crafted_strategy");
    write_file(&dir.join("src/a.txt"), b"a");
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &[]);
    let plan = |extra: &[&str]| {
        let dest = dir.join("out");
        let _ = fs::remove_dir_all(&dest);
        let mut args = vec!["--json"];
        args.extend_from_slice(extra);
        let out = pull_cmd(&img, &dest, &args, &[]);
        assert_success(&out, "pull --json");
        String::from_utf8_lossy(&out.stdout).lines().next().unwrap().to_string()
    };
    let p = plan(&[]);
    assert!(p.contains(r#""op":"pull""#) && p.contains(r#""strategy":{"transport":""#), "{p}");
    let p = plan(&["--gap-kb", "8", "--inflight", "3", "--buffered", "true"]);
    assert!(p.contains(r#""gap_kb":8,"inflight":3,"direct":false"#), "{p}");
}

// ───────────────────────── directory stream entries ─────────────────────────

/// A directory entry with ValidDataLength ≠ DataLength (the spec requires them equal): the
/// shared parser refuses it, so pull, add and info all do.
#[test]
fn directory_with_valid_length_mismatch_is_refused_by_pull_and_add() {
    let dir = test_dir("crafted_dir_vdl");
    write_file(&dir.join("src/odd-dir/f.txt"), b"x");
    write_file(&dir.join("more/new.txt"), b"y");
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &["--cluster", "4096"]);
    let mut b = fs::read(&img).unwrap();
    patch_set(&mut b, "odd-dir", |s| {
        let len = u64::from_le_bytes(s[56..64].try_into().unwrap());
        s[40..48].copy_from_slice(&(len + 4096).to_le_bytes());
    });
    fs::write(&img, &b).unwrap();
    let before = fs::read(&img).unwrap();
    let dest = dir.join("out");
    pull_fails(&img, &dest, &[], &[], "differs from DataLength");
    pull_fails(&img, &dest, &["--skip-existing"], &[], "differs from DataLength");
    assert!(!dest.exists(), "a refused pull created the destination");
    let add = flashcp().arg("add").arg(dir.join("more")).arg(&img).output().unwrap();
    let err = String::from_utf8_lossy(&add.stderr);
    assert!(!add.status.success() && err.contains("differs from DataLength"), "add accepted it:\n{err}");
    let info = flashcp().arg("info").arg(&img).output().unwrap();
    assert!(!info.status.success(), "info accepted it");
    assert!(fs::read(&img).unwrap() == before, "a refused add changed the image");
}

/// A directory whose DataLength is not a multiple of the cluster size.
#[test]
fn directory_length_not_a_cluster_multiple_is_refused() {
    let dir = test_dir("crafted_dir_len");
    write_file(&dir.join("src/odd-dir/f.txt"), b"x");
    let img = dir.join("v.img");
    build(&dir.join("src"), &img, 32 * MB, &["--cluster", "4096"]);
    let mut b = fs::read(&img).unwrap();
    patch_set(&mut b, "odd-dir", |s| {
        s[40..48].copy_from_slice(&1000u64.to_le_bytes());
        s[56..64].copy_from_slice(&1000u64.to_le_bytes());
    });
    fs::write(&img, &b).unwrap();
    let dest = dir.join("out");
    pull_fails(&img, &dest, &[], &[], "not a non-zero multiple of the cluster size");
    assert!(!dest.exists());
    let add = flashcp().arg("add").arg(dir.join("src")).arg(&img).arg("--skip-existing").output().unwrap();
    assert!(!add.status.success(), "add accepted it");
}
