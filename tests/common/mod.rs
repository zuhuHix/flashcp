//! Shared helpers for the integration tests: scratch dirs, the flashcp runner, a deterministic
//! "tricky" source tree, the source-side walk, the comparison against the read-back parser,
//! and the fsck.exfat wrapper.

pub mod exfat_reader;

use exfat_reader::Volume;
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MB: u64 = 1 << 20;
/// 1980-01-01 00:00:00 UTC, the earliest time an exFAT timestamp can hold.
pub const EXFAT_EPOCH: i64 = 315_532_800;

/// Fresh scratch directory for one test, under CARGO_TARGET_TMPDIR.
pub fn test_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

pub fn flashcp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_flashcp"))
}

/// Creates (or truncates) an image file of exactly `bytes` bytes. set_len makes it sparse.
pub fn make_image(path: &Path, bytes: u64) {
    let f = File::create(path).expect("create image");
    f.set_len(bytes).expect("set image length");
}

/// `flashcp build SRC IMG --yes-erase EXTRA...`
pub fn run_build(src: &Path, img: &Path, extra: &[&str]) -> Output {
    flashcp().arg("build").arg(src).arg(img).arg("--yes-erase").args(extra).output().expect("run flashcp build")
}

pub fn assert_success(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed (exit {:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Deterministic xorshift64 generator (seed must be non-zero).
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

/// Overwrites the first `bytes` bytes of `path` with pseudo-random data.
pub fn fill_random(path: &Path, bytes: u64, seed: u64) {
    let mut f = fs::OpenOptions::new().write(true).open(path).expect("open image for junk fill");
    let mut rng = XorShift(seed);
    let mut buf = vec![0u8; MB as usize];
    let mut left = bytes;
    while left > 0 {
        let n = left.min(MB) as usize;
        rng.fill(&mut buf[..n]);
        f.write_all(&buf[..n]).expect("write junk");
        left -= n as u64;
    }
    f.sync_all().expect("sync junk");
}

pub fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, bytes).expect("write file");
}

/// Seconds since the Unix epoch for a UTC calendar date and time.
pub fn unix_secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + h * 3_600 + mi * 60 + s
}

pub fn set_mtime(path: &Path, secs: i64, nanos: u32) {
    let t = UNIX_EPOCH + Duration::new(secs as u64, nanos);
    File::open(path).and_then(|f| f.set_modified(t)).expect("set mtime");
}

/// Writes the deterministic "tricky" tree under `root` (created if missing) and sets mtimes.
///
/// Covers Unicode and emoji names (UTF-16 surrogate pairs), a 200-character name, empty files
/// and directories, six levels of nesting, a 5 MB pseudo-random file, 200 small files of
/// 0..20 KB over four directories, files of exactly 4096 and 4097 bytes, and mtimes that hit
/// 2024-02-29, odd seconds, sub-10 ms digits, and a pre-1980 time.
pub fn make_tricky_tree(root: &Path) {
    fs::create_dir_all(root).expect("create tree root");
    let p = |rel: &str| root.join(rel);
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);

    write_file(&p("café.txt"), b"cafe\n");
    write_file(&p("日本語.txt"), "こんにちは\n".as_bytes());
    write_file(&p("Жук.bin"), &[0xD0, 0x96, 0x00, 0x01, 0xFF]);
    write_file(&p("😀 file.dat"), b"smile");
    let long_name = format!("{}.txt", "L".repeat(196));
    assert_eq!(long_name.chars().count(), 200);
    write_file(&p(&long_name), b"a 200-character name");
    write_file(&p("empty.txt"), b"");
    fs::create_dir_all(p("empty-dir")).expect("empty dir");

    let mut exact = vec![0u8; 4096];
    rng.fill(&mut exact);
    write_file(&p("exact-4096.bin"), &exact);
    let mut plus_one = vec![0u8; 4097];
    rng.fill(&mut plus_one);
    write_file(&p("cluster-plus-1.bin"), &plus_one);

    let mut big = vec![0u8; 5 * 1024 * 1024];
    rng.fill(&mut big);
    write_file(&p("random-5mb.bin"), &big);

    write_file(&p("deep/l2/l3/l4/l5/l6/leaf.txt"), b"six levels down");

    for d in 0..4 {
        for f in 0..50 {
            let size = if f == 0 { 0 } else { rng.below(20 * 1024 + 1) as usize };
            let mut buf = vec![0u8; size];
            rng.fill(&mut buf);
            write_file(&p(&format!("bulk/d{d}/f{f:03}.bin")), &buf);
        }
    }

    // Timestamps are set last: writing a file or a child changes its parent's mtime.
    set_mtime(&p("café.txt"), unix_secs(2024, 2, 29, 12, 34, 57), 123_456_789);
    set_mtime(&p("日本語.txt"), unix_secs(2024, 2, 29, 0, 0, 0), 0);
    set_mtime(&p("empty.txt"), unix_secs(1972, 6, 15, 12, 0, 0), 0);
    set_mtime(&p("exact-4096.bin"), unix_secs(2026, 11, 9, 10, 0, 1), 999_999_999);
    set_mtime(&p("random-5mb.bin"), unix_secs(2026, 11, 9, 8, 30, 0), 0);
    set_mtime(&p("deep/l2/l3/l4/l5/l6/leaf.txt"), unix_secs(2024, 2, 29, 23, 59, 59), 500_000_000);
    set_mtime(&p("cluster-plus-1.bin"), unix_secs(2015, 5, 5, 5, 5, 5), 4_321_000);
    for d in 0..4 {
        for f in 0..50 {
            let secs = unix_secs(2001, 9, 9, 1, 46, 40) + (d * 50 + f) as i64 * 86_399;
            set_mtime(&p(&format!("bulk/d{d}/f{f:03}.bin")), secs, (f as u32 * 19_000_001) % 1_000_000_000);
        }
    }
    set_mtime(&p("empty-dir"), unix_secs(2010, 7, 4, 12, 0, 0), 0);
    set_mtime(&p("deep/l2/l3/l4/l5/l6"), unix_secs(1999, 12, 31, 23, 59, 58), 0);
    for d in 0..4 {
        set_mtime(&p(&format!("bulk/d{d}")), unix_secs(2000, 1, 1, 0, 0, 0) + d as i64, 0);
    }
}

/// `flashcp gen DIR --profile PROFILE` as a tree maker.
pub fn gen_dataset(profile: &'static str) -> impl FnOnce(&Path) {
    move |dir: &Path| {
        let out = flashcp().arg("gen").arg(dir).arg("--profile").arg(profile).output().expect("run flashcp gen");
        assert_success(&out, "flashcp gen");
    }
}

/// A source node as the read-back must reproduce it.
pub struct SrcNode {
    pub is_dir: bool,
    pub bytes: Vec<u8>,
    /// Expected read-back mtime: the source mtime truncated to 10 ms, or 1980-01-01 if earlier.
    pub mtime: (i64, u32),
}

fn expected_mtime(t: SystemTime) -> (i64, u32) {
    let d = t.duration_since(UNIX_EPOCH).expect("mtime before 1970 is not expected");
    let secs = d.as_secs() as i64;
    let nanos = d.subsec_nanos() / 10_000_000 * 10_000_000;
    if secs < EXFAT_EPOCH { (EXFAT_EPOCH, 0) } else { (secs, nanos) }
}

/// Walks the source tree (sorted, regular files and directories only).
pub fn read_source_tree(root: &Path) -> BTreeMap<String, SrcNode> {
    let mut out = BTreeMap::new();
    walk_source(root, "", &mut out);
    out
}

fn walk_source(dir: &Path, prefix: &str, out: &mut BTreeMap<String, SrcNode>) {
    let mut ents: Vec<_> = fs::read_dir(dir).expect("read source dir").map(|e| e.expect("entry")).collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let path = e.path();
        let md = fs::symlink_metadata(&path).expect("stat source");
        let name = e.file_name().into_string().expect("UTF-8 source name");
        let key = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let mtime = expected_mtime(md.modified().expect("mtime"));
        if md.is_dir() {
            out.insert(key.clone(), SrcNode { is_dir: true, bytes: Vec::new(), mtime });
            walk_source(&path, &key, out);
        } else if md.is_file() {
            let bytes = fs::read(&path).expect("read source file");
            out.insert(key, SrcNode { is_dir: false, bytes, mtime });
        } else {
            panic!("non-regular source entry {}", path.display());
        }
    }
}

/// Every difference between the source tree and the volume. Empty means equal.
pub fn compare_trees(src: &BTreeMap<String, SrcNode>, vol: &Volume) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, s) in src {
        let Some(v) = vol.tree.get(path) else {
            problems.push(format!("missing on volume: {path}"));
            continue;
        };
        if v.is_dir != s.is_dir {
            problems.push(format!("type differs: {path} (source dir={}, volume dir={})", s.is_dir, v.is_dir));
            continue;
        }
        if !s.is_dir && v.bytes != s.bytes {
            let first = v.bytes.iter().zip(&s.bytes).position(|(a, b)| a != b);
            problems.push(format!(
                "content differs: {path}: source {} bytes, volume {} bytes, first mismatch at {first:?}",
                s.bytes.len(),
                v.bytes.len()
            ));
        }
        if v.mtime != s.mtime {
            problems.push(format!("mtime differs: {path}: source {:?}, volume {:?}", s.mtime, v.mtime));
        }
    }
    for path in vol.tree.keys() {
        if !src.contains_key(path) {
            problems.push(format!("extra on volume: {path}"));
        }
    }
    problems
}

/// Runs `fsck.exfat -n` and asserts a clean result. Skips (with a notice) when fsck.exfat is not
/// on PATH, unless FLASHCP_REQUIRE_FSCK=1, in which case a missing fsck.exfat is a failure.
pub fn fsck_clean(img: &Path) {
    let out = match Command::new("fsck.exfat").arg("-n").arg(img).output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            assert!(
                env::var("FLASHCP_REQUIRE_FSCK").as_deref() != Ok("1"),
                "fsck.exfat is not on PATH and FLASHCP_REQUIRE_FSCK=1"
            );
            eprintln!("SKIP fsck check: fsck.exfat is not on PATH");
            return;
        }
        Err(e) => panic!("cannot run fsck.exfat: {e}"),
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success() && text.contains("clean"),
        "fsck.exfat -n {} (exit {:?}):\n{text}",
        img.display(),
        out.status.code()
    );
}

/// Builds `src` (made by `make_src`) onto a fresh image of `img_bytes` bytes, then checks fsck
/// and the read-back against the source. Removes the scratch dir (sources and image) on success.
pub fn round_trip(name: &str, img_bytes: u64, junk: bool, extra: &[&str], make_src: impl FnOnce(&Path)) -> Volume {
    let dir = test_dir(name);
    let src = dir.join("src");
    make_src(&src);
    let img = dir.join("disk.img");
    make_image(&img, img_bytes);
    if junk {
        fill_random(&img, img_bytes, 0xC0FF_EE00_1234_5678);
    }
    let out = run_build(&src, &img, extra);
    assert_success(&out, "flashcp build");
    fsck_clean(&img);
    let vol = exfat_reader::read_volume(&img).unwrap_or_else(|e| panic!("read-back of {name} failed: {e}"));
    let problems = compare_trees(&read_source_tree(&src), &vol);
    assert!(
        problems.is_empty(),
        "read-back of {name} differs from the source ({} problems):\n{}",
        problems.len(),
        problems.iter().take(30).cloned().collect::<Vec<_>>().join("\n")
    );
    fs::remove_dir_all(&dir).expect("remove scratch dir");
    vol
}

/// Reads an image in full and asserts every byte is zero.
pub fn assert_all_zero(img: &Path) {
    let mut f = File::open(img).expect("open image");
    let mut buf = vec![0u8; MB as usize];
    loop {
        let n = f.read(&mut buf).expect("read image");
        if n == 0 {
            break;
        }
        assert!(buf[..n].iter().all(|&b| b == 0), "image has non-zero bytes");
    }
}
