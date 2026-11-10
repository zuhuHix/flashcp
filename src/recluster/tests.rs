//! Tests of `flashcp recluster`, in process, on image files:
//! - one model suite: random trees (made through the drive-mode driver, so with fragmented
//!   files, unused entries and Unicode names) on volumes of random size and cluster size, with
//!   garbage in their free space, re-clustered to a random smaller size; the validating
//!   parser must then find every entry and every file byte as before, `fsck.exfat` must be
//!   clean and the driver must serve the same tree;
//! - one crash harness: the same conversion with a crash injected at every device write and
//!   sync in turn, under three loss models; what is left must be the old or the new volume,
//!   fsck-clean, with every file intact, and a second run must finish the job;
//! - one table of refusals: every reason, each leaving the image byte for byte as it was.

use super::*;
use crate::drive::dev::{CrashDev, Loss};
use crate::drive::tests::{Img, fsck, image, open, snapshot};
use crate::drive::{ROOT, RenameFlags, SetAttr};
use crate::util::Rng;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;

const COST: Cost = Cost { bps: 20e6, skip_s: 1e-3, read_bps: 40e6 };

fn rw(p: &Path) -> File {
    File::options().read(true).write(true).open(p).unwrap()
}

fn opts(cluster: Option<u32>, fat: FatMode) -> ReclusterOpts {
    ReclusterOpts { cluster, fat, dry_run: false, expect_serial: None, expect_hw: None }
}

/// `recluster` of the image at `p` through `dev` (the plain file when None).
fn run(p: &Path, o: &ReclusterOpts, dev: Option<&dyn Dev>) -> Result<Stats> {
    run_seen(p, o, dev, &mut |_| {})
}

fn run_seen(p: &Path, o: &ReclusterOpts, dev: Option<&dyn Dev>, sink: &mut dyn FnMut(&Event)) -> Result<Stats> {
    let f = rw(p);
    let len = f.metadata().unwrap().len();
    let plain = FileDev::new(Arc::new(rw(p)));
    run_on(&f, len, dev.unwrap_or(&plain), o, &COST, &|| Ok(()), sink)
}

fn reason(r: Result<Stats>) -> &'static str {
    let e = r.expect_err("should be refused");
    refusal_of(&e).unwrap_or_else(|| panic!("not a refusal: {e:#}")).reason
}

fn sum(p: &Path) -> u128 {
    use std::io::Read;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut f = File::open(p).unwrap();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        match f.read(&mut buf).unwrap() {
            0 => return h.digest128(),
            n => h.update(&buf[..n]),
        }
    }
}

/// Garbage into the free clusters (the first 48 MB of them), as on a stick that was used
/// before: whatever recluster leaves unwritten (the FAT entries nothing uses) then holds it.
fn dirty_free_space(p: &Path) {
    let f = rw(p);
    let v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, true).unwrap();
    let mut rng = Rng(0x5EED);
    let mut block = vec![0u8; 1 << 20];
    rng.fill(&mut block);
    let mut left = 48u64 << 20;
    for (s, n) in free_extents(&v.bitmap, v.cc) {
        let (mut off, end) = (v.cluster_off(s as u32), v.cluster_off(s as u32) + (n * v.cs).min(left));
        left -= end - off;
        while off < end {
            let len = (end - off).min(block.len() as u64) as usize;
            f.write_all_at(&block[..len], off).unwrap();
            off += len as u64;
        }
    }
}

/// A random tree written through the driver: nested directories, long and Unicode names,
/// empty files, files rewritten in the middle (copy-on-write: fragments), truncated, renamed
/// and deleted ones, over several commits. Stops creating at `budget` bytes of file data.
fn populate(p: &Path, seed: u64, ops: usize, budget: u64) {
    let fs = open(p);
    let mut rng = Rng(seed * 2 + 1);
    let stems = [
        "f",
        "Ünï-cödé ",
        "日本語のファイル ",
        "a long name with spaces and more than fifteen chars ",
        "😀 ",
        "an even longer name, long enough to need a good many file name entries in its entry set, which is what \
         makes an entry set straddle a sector and a cluster boundary ",
    ];
    let mut dirs = vec![(ROOT, 0usize)];
    let mut files: Vec<(u64, String)> = Vec::new();
    // What the volume already holds takes part: its files get rewritten, renamed, deleted.
    let mut k = 0;
    while k < dirs.len() {
        let (dir, depth) = dirs[k];
        for (ino, name, is_dir) in fs.readdir(dir).unwrap() {
            if is_dir {
                dirs.push((ino, depth + 1));
            } else {
                files.push((dir, name));
            }
        }
        k += 1;
    }
    let (mut serial, mut written, mut staged) = (0u64, 0u64, 0u64);
    for _ in 0..ops {
        let pick = rng.next();
        let (dir, depth) = dirs[(rng.next() % dirs.len() as u64) as usize];
        serial += 1;
        let name = format!("{}{serial}", stems[(rng.next() % stems.len() as u64) as usize]);
        match pick % 100 {
            // (a full volume says ENOSPC to any of these: the tree is then what fitted)
            0..=11 if depth < 5 => {
                if let Ok(a) = fs.mkdir(dir, OsStr::new(&name)) {
                    dirs.push((a.ino, depth + 1));
                }
            }
            0..=54 if written < budget => {
                let size = match rng.next() % 10 {
                    0 => 0,
                    1..=5 => 1 + rng.next() % 6000,
                    6..=8 => 1 + rng.next() % 150_000,
                    _ => 1 + rng.next() % 3_000_000,
                };
                let Ok(a) = fs.create(dir, OsStr::new(&name), true, false) else { continue };
                let mut data = vec![0u8; size as usize];
                rng.fill(&mut data);
                if fs.write(a.ino, 0, &data).is_err() {
                    written = budget;
                }
                fs.release(a.ino);
                files.push((dir, name));
                written += size;
                staged += size;
            }
            55..=69 if !files.is_empty() => {
                let (d, n) = &files[(rng.next() % files.len() as u64) as usize];
                let a = fs.lookup(*d, OsStr::new(n)).unwrap();
                let mut data = vec![0u8; 1 + (rng.next() % 40_000) as usize];
                rng.fill(&mut data);
                let _ = fs.write(a.ino, rng.next() % (a.size + 1), &data);
                staged += data.len() as u64;
            }
            70..=76 if !files.is_empty() => {
                let (d, n) = &files[(rng.next() % files.len() as u64) as usize];
                let a = fs.lookup(*d, OsStr::new(n)).unwrap();
                let size = rng.next() % (a.size + 5000);
                let _ = fs.setattr(a.ino, &SetAttr { size: Some(size), ..Default::default() });
            }
            77..=86 if !files.is_empty() => {
                let (d, n) = files.swap_remove((rng.next() % files.len() as u64) as usize);
                fs.unlink(d, OsStr::new(&n)).unwrap();
            }
            87..=92 if !files.is_empty() => {
                let k = (rng.next() % files.len() as u64) as usize;
                let (d, n) = files[k].clone();
                if fs.rename(d, OsStr::new(&n), dir, OsStr::new(&name), RenameFlags::default()).is_ok() {
                    files[k] = (dir, name);
                }
            }
            _ => {
                fs.flush().unwrap();
                staged = 0;
            }
        }
        if staged > 8 << 20 {
            fs.flush().unwrap();
            staged = 0;
        }
    }
    fs.shutdown().unwrap();
}

/// Sets or clears allocation bits of `clusters` (all in the bitmap's first run).
fn set_bits(p: &Path, clusters: &[u64], on: bool) {
    let f = rw(p);
    let mut v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, true).unwrap();
    for &c in clusters {
        let (byte, mask) = (((c - 2) / 8) as usize, 1u8 << ((c - 2) % 8));
        assert_eq!(v.bitmap[byte] & mask != 0, !on, "cluster {c}");
        v.bitmap[byte] ^= mask;
    }
    assert_eq!(runs_of(&v.bitmap_clusters).len(), 1);
    f.write_all_at(&v.bitmap, v.cluster_off(v.bitmap_clusters[0])).unwrap();
}

/// One file in many pieces: all free space but a few short runs is marked allocated (leaked)
/// while the driver writes it, so its clusters cannot be one run.
fn fragmented_file(p: &Path, seed: u64) {
    let f = File::open(p).unwrap();
    let v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, true).unwrap();
    let mut rng = Rng(seed * 2 + 7);
    let (mut leaked, mut kept) = (Vec::new(), 0u64);
    for (s, n) in free_extents(&v.bitmap, v.cc) {
        let mut c = s;
        while c < s + n {
            let keep = if kept < 48 { (1 + rng.next() % 3).min(s + n - c) } else { 0 };
            let skip = (1 + rng.next() % 2).min(s + n - c - keep);
            leaked.extend(c + keep..c + keep + skip);
            kept += keep;
            c += keep + skip;
        }
    }
    let cs = v.cs;
    drop(v);
    set_bits(p, &leaked, true);
    let fs = open(p);
    if let Ok(a) = fs.create(ROOT, OsStr::new("in pieces.bin"), true, false) {
        let mut data = vec![0u8; (kept * cs / 2) as usize];
        rng.fill(&mut data);
        let _ = fs.write(a.ino, 0, &data);
        fs.release(a.ino);
    }
    fs.shutdown().unwrap();
    set_bits(p, &leaked, false);
}

/// Turns some contiguous (NoFatChain) files into FAT-chained ones, as other systems leave
/// them: the same clusters chained in order (a file that was once extended), or in REVERSE
/// order (a chain that starts high and ends low, as an allocator that wrapped around makes).
fn chainify(p: &Path, seed: u64) {
    let f = rw(p);
    let mut v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, false).unwrap();
    let mut rng = Rng(seed * 2 + 5);
    let mut stack = vec![v.dirs.pop().unwrap()];
    while let Some(d) = stack.pop() {
        for ch in d.children.values() {
            if ch.is_dir {
                stack.push(v.read_child(0, &d.path, ch).unwrap());
                continue;
            }
            let n = ch.len.div_ceil(v.cs);
            let how = rng.next() % 5;
            if !ch.no_fat_chain || n == 0 || how > 2 || crate::volume::is_sync_journal(ch) {
                continue;
            }
            let mut chain: Vec<u32> = (ch.first..ch.first + n as u32).collect();
            if how == 2 {
                chain.reverse();
            }
            for (k, &c) in chain.iter().enumerate() {
                let next = chain.get(k + 1).copied().unwrap_or(EOC);
                f.write_all_at(&next.to_le_bytes(), v.fat_off + c as u64 * 4).unwrap();
            }
            let mut set = d.data[ch.set_off..ch.set_off + ch.set_len].to_vec();
            set[33] &= !2;
            set[52..56].copy_from_slice(&chain[0].to_le_bytes());
            let sum = checksum16(&set, true);
            set[2..4].copy_from_slice(&sum.to_le_bytes());
            for (k, b) in set.iter().enumerate() {
                let o = (ch.set_off + k) as u64;
                f.write_all_at(&[*b], v.cluster_off(d.clusters[(o / v.cs) as usize]) + o % v.cs).unwrap();
            }
        }
    }
}

/// A used stick: a tree laid out by `flashcp build` with clusters of `cluster` bytes
/// (directories, then file data, from the start of the heap: right where a recluster needs
/// room, as on a stick another system filled), garbage in the free space, then `ops` random
/// changes through the driver, a file in many pieces, and FAT chains where other systems
/// would have left some.
fn volume(name: &str, mb: u64, cluster: u32, seed: u64, ops: usize, budget: u64) -> Img {
    let img = image(name, mb, cluster);
    let src = img.dir.join("src");
    let mut rng = Rng(seed * 2 + 99);
    let clusters = (mb << 20) / cluster as u64;
    let dirs = ["", "docs", "docs/Ünï cödé", "写真", "docs/Ünï cödé/deep/deeper", "empty"];
    for d in dirs {
        std::fs::create_dir_all(src.join(d)).unwrap();
    }
    let mut used = 0;
    for k in 0..40 {
        let size = match rng.next() % 8 {
            0 => 0,
            1..=4 => 1 + rng.next() % 9000,
            5..=6 => 1 + rng.next() % 400_000,
            _ => 1 + rng.next() % 2_500_000,
        };
        used += size.div_ceil(cluster as u64);
        if used > clusters / 4 {
            break;
        }
        let mut data = vec![0u8; size as usize];
        rng.fill(&mut data);
        let dir = dirs[(rng.next() % 5) as usize];
        std::fs::write(src.join(dir).join(format!("built {k} with a name of some length.bin")), data).unwrap();
    }
    let bo = crate::exfat::BuildOpts {
        cluster: Some(cluster),
        label: "MODEL".into(),
        fat: FatMode::Prefix,
        readers: 2,
        dry_run: false,
        serial: Some(seed as u32 | 0x1000),
        job: None,
    };
    crate::exfat::build(&src, &img.path, &bo, &mut |_| {}).unwrap();
    dirty_free_space(&img.path);
    populate(&img.path, seed, ops, budget);
    if !seed.is_multiple_of(3) {
        fragmented_file(&img.path, seed);
    }
    chainify(&img.path, seed);
    img
}

/// Everything the volume holds, as the validating parser reads it: per directory its live
/// entries in order (the fields a re-addressing changes blanked, see [`comparable`]), per file
/// its bytes (zeros past ValidDataLength). With the cluster size.
type Digest = BTreeMap<String, Vec<u8>>;

fn digest(p: &Path) -> (u64, Digest) {
    let f = File::open(p).unwrap();
    let t = open_tree(&f, f.metadata().unwrap().len()).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
    let mut out = Digest::new();
    let mut paths = vec![String::new(); t.dirs.len()];
    for (di, d) in t.dirs.iter().enumerate() {
        out.insert(format!("{}/", paths[di]), comparable(&t, d, di));
        for &oi in &d.kids {
            let o = &t.objs[oi as usize];
            let set = &d.content[o.at as usize..(o.at + o.set_len) as usize];
            let name: Vec<u16> =
                (0..set[35] as usize).map(|i| le16(set, 64 + 32 * (i / 15) + 2 + 2 * (i % 15))).collect();
            let path = format!("{}/{}", paths[di], String::from_utf16(&name).unwrap());
            if o.is_dir {
                paths[o.dir as usize] = path;
                continue;
            }
            let mut data = Vec::with_capacity(o.len as usize);
            for (off, len) in extents(&o.runs, t.heap, t.cs, le64(set, 40)) {
                let at = data.len();
                data.resize(at + len as usize, 0);
                f.read_exact_at(&mut data[at..], off).unwrap();
            }
            data.resize(o.len as usize, 0);
            assert!(out.insert(path, data).is_none());
        }
    }
    (t.cs, out)
}

fn same(got: &Digest, want: &Digest, what: &str) {
    if got != want {
        let diff: Vec<&String> = want
            .keys()
            .filter(|k| got.get(*k) != want.get(*k))
            .chain(got.keys().filter(|k| !want.contains_key(*k)))
            .collect();
        panic!("{what}: {} entries differ, first {:?}", diff.len(), &diff[..diff.len().min(5)]);
    }
}

fn boot_regions_agree(p: &Path) -> bool {
    backup_state(&read_boot(&File::open(p).unwrap()).unwrap()).unwrap() == Backup::Same
}

#[test]
fn model_random_trees_on_random_geometries() {
    let seeds: u64 = std::env::var("FLASHCP_MODEL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(16);
    let (mut converted, mut moved, mut chained) = (0, 0, 0);
    for seed in 0..seeds {
        let mut rng = Rng(seed * 7919 + 13);
        let (old, new, mb): (u32, Option<u32>, u64) = if seed % 2 == 0 {
            // What the command is for: a stick's default clusters to 4 KB, on a volume big
            // enough (sparse) that the new FAT displaces files, not just the first directories.
            let old = [32768, 131_072][(seed / 2 % 2) as usize];
            (old, None, ((6 * old as u64 * 4096) >> 20) + rng.next() % 64)
        } else {
            // Any cluster size to any smaller one, down to 512 bytes, on a small volume.
            let old = [8192u32, 16384, 32768, 65536, 131_072, 1 << 20][(rng.next() % 6) as usize];
            let smaller: Vec<u32> = (9..old.trailing_zeros()).map(|s| 1 << s).collect();
            (old, Some(smaller[(rng.next() % smaller.len() as u64) as usize]), 24 + rng.next() % 120)
        };
        let fat = if rng.next().is_multiple_of(3) { FatMode::Full } else { FatMode::Prefix };
        let what = format!("seed {seed}: {mb} MB, clusters {old} → {new:?}, {fat:?}");
        // The small ones up to 85 % full, so some are too full to convert.
        let budget = ((mb << 20) * (5 + rng.next() % 56) / 100).min(40 << 20);
        let ops = 40 + (rng.next() % 160) as usize;
        let img = volume(&format!("recl-model-{seed}"), mb, old, seed, ops, budget);
        fsck(&img.path);
        let (cs, before) = digest(&img.path);
        assert_eq!(cs, old as u64);
        chained += {
            let f = File::open(&img.path).unwrap();
            let t = open_tree(&f, f.metadata().unwrap().len()).unwrap();
            t.objs.iter().filter(|o| !o.nfc && o.runs.len() > 1).count()
        };
        let tree = {
            let fs = open(&img.path);
            let t = snapshot(&fs);
            fs.shutdown().unwrap();
            t
        };
        // (Not on the multi-GB ones: they are never too full, and hashing them takes seconds.)
        let image_before = if mb <= 256 { sum(&img.path) } else { 0 };
        let boot_before = read_boot(&File::open(&img.path).unwrap()).unwrap();
        let mut events = Vec::new();
        match run_seen(&img.path, &opts(new, fat), None, &mut |e| events.push(e.clone())) {
            Ok(s) => {
                // plan, switched (once or twice), a last progress at 100 %, done: in that order
                let Event::Plan(plan) = &events[0] else { panic!("{what}: {events:?}") };
                let n = events.len();
                assert!(
                    events[n - 1] == Event::Done(s.clone())
                        && matches!(events[n - 2], Event::Progress { bytes_done, bytes_total } if bytes_done == bytes_total)
                        && events.iter().filter(|e| matches!(e, Event::Switched { .. })).count()
                            == 1 + plan.relocate as usize
                        && s.written == plan.write_bytes,
                    "{what}: {events:?}"
                );
                moved += (plan.move_bytes > 0) as u64;
                let want = new.map_or(DEFAULT_CLUSTER, |c| c as u64);
                assert!(s.changed && s.cluster == want && s.free_after >= s.free_before, "{what}: {s:?}");
                converted += 1;
            }
            Err(e) => {
                let r = refusal_of(&e).unwrap_or_else(|| panic!("{what}: {e:#}"));
                assert!(matches!(r.reason, "no-space" | "geometry"), "{what}: refused: {}", r.message);
                assert!(mb <= 256 && sum(&img.path) == image_before, "{what}: a refusal wrote something");
                continue;
            }
        }
        fsck(&img.path);
        let (cs, after) = digest(&img.path);
        assert_eq!(cs, new.map_or(DEFAULT_CLUSTER, |c| c as u64), "{what}");
        same(&after, &before, &what);
        assert!(boot_regions_agree(&img.path), "{what}");
        // Of the boot sector, only the geometry fields and boot code below 440 may differ: the
        // bytes where a partition table would be decide whether Windows shows a superfloppy.
        let boot_after = read_boot(&File::open(&img.path).unwrap()).unwrap();
        assert!(boot_after[..80] == boot_before[..80] && boot_after[440..512] == boot_before[440..512], "{what}");
        assert_eq!(boot_after[512..12 * 512], boot_before[512..12 * 512], "{what}: sectors 1 to 11 were written");
        // Again: nothing to do. And the driver serves the same tree and can write to it.
        assert!(!run(&img.path, &opts(new, fat), None).unwrap().changed, "{what}");
        let fs = open(&img.path);
        assert!(snapshot(&fs) == tree, "{what}: the driver sees another tree");
        let a = fs.create(ROOT, OsStr::new("written after the recluster"), true, false).unwrap();
        fs.write(a.ino, 0, &[7u8; 50_000]).unwrap();
        fs.release(a.ino);
        fs.shutdown().unwrap();
        fsck(&img.path);
    }
    // The corpus must reach what matters: conversions, file data in the way of the new FAT,
    // files in pieces.
    assert!(
        converted * 2 >= seeds && moved * 3 >= seeds && chained as u64 >= seeds,
        "{converted} of {seeds} volumes converted, {moved} moved file data, {chained} fragmented files"
    );
}

#[test]
fn crash_at_every_device_op_leaves_the_old_or_the_new_volume() {
    // 8 KB to 512 B clusters: on a volume this small, that is the ratio at which the new FAT
    // displaces files (as 128 KB to 4 KB does on a real stick), so both steps have work.
    let (old, new) = (8192u32, 512u64);
    let img = volume("recl-crash", 32, old, 1, 90, 9 << 20);
    let template = img.dir.join("template.img");
    std::fs::copy(&img.path, &template).unwrap();
    let (_, before) = digest(&img.path);
    let o = opts(Some(new as u32), FatMode::Prefix);

    let dev = CrashDev::new(rw(&img.path), None);
    let s = run(&img.path, &o, Some(&dev)).unwrap();
    let total = dev.ops();
    assert!(s.changed && total > 12, "{total} device ops");
    let stride: u64 = std::env::var("FLASHCP_CRASH_STRIDE").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let (mut olds, mut news, mut owed) = (0, 0, 0);
    for k in (0..total).step_by(stride as usize) {
        for loss in [Loss::None, Loss::All, Loss::Random(k * 31 + 7)] {
            let what = format!("crash at op {k}/{total} ({loss:?})");
            std::fs::copy(&template, &img.path).unwrap();
            let dev = CrashDev::new(rw(&img.path), Some(k));
            let e = run(&img.path, &o, Some(&dev)).err().unwrap_or_else(|| panic!("{what}: op did not fail"));
            assert!(refusal_of(&e).is_none(), "{what}: {e:#}");
            dev.crash(loss);
            drop(dev);
            // Whatever is there is a whole volume: the old one or the new one, never a mix.
            fsck(&img.path);
            let (cs, now) = digest(&img.path);
            assert!(cs == old as u64 || cs == new, "{what}: cluster size {cs}");
            same(&now, &before, &what);
            if cs == new {
                news += 1
            } else {
                olds += 1
            }
            if !boot_regions_agree(&img.path) {
                owed += 1;
                // A run that is then refused still completes the backup it owes.
                assert_eq!(reason(run(&img.path, &opts(Some(1 << 20), FatMode::Prefix), None)), "bad-cluster-size");
                assert!(boot_regions_agree(&img.path), "{what}: a refused run left the backup boot sector owed");
            }
            // The next run finishes (or starts over), and nothing is left to do after it.
            let s = run(&img.path, &o, None).unwrap_or_else(|e| panic!("{what}: the second run failed: {e:#}"));
            assert_eq!((s.cluster, s.changed), (new, cs != new), "{what}: {s:?}");
            fsck(&img.path);
            let (cs, now) = digest(&img.path);
            assert_eq!(cs, new, "{what}");
            same(&now, &before, &format!("{what}, after the second run"));
            assert!(boot_regions_agree(&img.path), "{what}: the backup boot sector was not completed");
        }
    }
    if stride == 1 {
        // Both switches were crossed, and both left their backup write to a second run.
        assert!(olds > 0 && news > 0 && owed >= 2, "old {olds}, new {news}, backup owed {owed}");
    }
}

/// The medium behind the claimed fd changes (a card swapped in a reader) after the check and
/// planning: before each phase that writes (relocation, switch 1, the new metadata, switch 2)
/// the boot sector is read again, and a change stops the run with `wrong-volume` before it
/// writes anything more.
#[test]
fn a_medium_change_before_any_writing_phase_stops_the_run() {
    use std::cell::{Cell, RefCell};
    let (old, new) = (8192u32, 512u32);
    let img = volume("recl-medium", 32, old, 1, 90, 9 << 20);
    let template = img.dir.join("template.img");
    std::fs::copy(&img.path, &template).unwrap();
    let serial = boot_serial(&std::fs::read(&img.path).unwrap()[..512]);
    let o = ReclusterOpts { expect_serial: Some(serial), ..opts(Some(new), FatMode::Prefix) };
    let len = std::fs::metadata(&img.path).unwrap().len();
    // Count the phases of an undisturbed run (two switches here).
    let calls = Cell::new(0);
    {
        let f = rw(&img.path);
        let plain = FileDev::new(Arc::new(rw(&img.path)));
        let s = run_on(
            &f,
            len,
            &plain,
            &o,
            &COST,
            &|| {
                calls.set(calls.get() + 1);
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();
        assert!(s.changed);
    }
    assert_eq!(calls.get(), 4, "relocation, switch 1, new metadata, switch 2");
    for at in 1..=4 {
        std::fs::copy(&template, &img.path).unwrap();
        let f = rw(&img.path);
        let plain = FileDev::new(Arc::new(rw(&img.path)));
        let (n, seen) = (Cell::new(0), RefCell::new(None));
        let hook = || -> Result<()> {
            n.set(n.get() + 1);
            if n.get() == at {
                // Another medium: its serial differs (the rest of sector 0 may well be equal).
                let other = rw(&img.path);
                let mut b = [0u8; 4];
                other.read_exact_at(&mut b, 100).unwrap();
                b[0] ^= 0x5a;
                other.write_all_at(&b, 100).unwrap();
                *seen.borrow_mut() = Some(sum(&img.path));
            }
            Ok(())
        };
        let r = run_on(&f, len, &plain, &o, &COST, &hook, &mut |_| {});
        assert_eq!(reason(r), "wrong-volume", "change before phase {at}");
        assert_eq!(Some(sum(&img.path)), *seen.borrow(), "phase {at}: written after the change");
    }
    // Without --expect-serial the boot sector still has to stay what the run planned from.
    std::fs::copy(&template, &img.path).unwrap();
    let f = rw(&img.path);
    let plain = FileDev::new(Arc::new(rw(&img.path)));
    let n = Cell::new(0);
    let hook = || -> Result<()> {
        n.set(n.get() + 1);
        if n.get() == 2 {
            rw(&img.path).write_all_at(&[0x77], 92).unwrap(); // the cluster count, low byte
        }
        Ok(())
    };
    let r = run_on(&f, len, &plain, &opts(Some(new), FatMode::Prefix), &COST, &hook, &mut |_| {});
    assert_eq!(reason(r), "wrong-volume");
}

/// The claimed device is a clone of the stick the user was asked about (same volume serial,
/// another hardware serial), or its hardware serial cannot be read: refused before any write.
#[test]
fn another_physical_drive_behind_the_fd_is_refused() {
    let img = volume("recl-hw", 32, 8192, 2, 40, 4 << 20);
    let before = sum(&img.path);
    let serial = boot_serial(&std::fs::read(&img.path).unwrap()[..512]);
    let o = ReclusterOpts {
        expect_serial: Some(serial),
        expect_hw: Some("ORIGINAL01".into()),
        ..opts(Some(512), FatMode::Prefix)
    };
    for live in [Some("CLONE02".to_string()), None] {
        HW_SERIAL.with(|h| *h.borrow_mut() = Some(live.clone()));
        let r = run(&img.path, &o, None);
        HW_SERIAL.with(|h| *h.borrow_mut() = None);
        assert_eq!(reason(r), "wrong-volume", "{live:?}");
        assert_eq!(sum(&img.path), before, "{live:?}: written to");
    }
    HW_SERIAL.with(|h| *h.borrow_mut() = Some(Some("ORIGINAL01".into())));
    let r = run(&img.path, &o, None);
    HW_SERIAL.with(|h| *h.borrow_mut() = None);
    assert!(r.unwrap().changed, "the right drive is converted");
}

/// The medium changes between a switch's main and backup boot-sector writes: the backup is not
/// written onto the replacement, and a rerun on the original drive completes the switch.
#[test]
fn a_medium_change_between_the_two_boot_writes_writes_no_backup() {
    let (old, new) = (8192u32, 512u32);
    let img = volume("recl-backup-swap", 32, old, 1, 90, 9 << 20);
    let template = img.dir.join("template.img");
    std::fs::copy(&img.path, &template).unwrap();
    let (_, before) = digest(&img.path);
    let o = opts(Some(new), FatMode::Prefix);
    let len = std::fs::metadata(&img.path).unwrap().len();
    for step in ["relocated", "reclustered"] {
        std::fs::copy(&template, &img.path).unwrap();
        let f = rw(&img.path);
        let plain = FileDev::new(Arc::new(rw(&img.path)));
        let (switched, seen) = (std::cell::RefCell::new(None), std::cell::RefCell::new(None));
        let mut sink = |e: &Event| {
            if let Event::Switched { step: s, .. } = e
                && *s == step
            {
                // Another medium now: sector 0 is not the one just written.
                let dev = rw(&img.path);
                let mut b = vec![0u8; 512];
                dev.read_exact_at(&mut b, 0).unwrap();
                *switched.borrow_mut() = Some(b.clone());
                b[100] ^= 0x5a;
                dev.write_all_at(&b, 0).unwrap();
                *seen.borrow_mut() = Some(sum(&img.path));
            }
        };
        let r = run_on(&f, len, &plain, &o, &COST, &|| Ok(()), &mut sink);
        assert_eq!(reason(r), "wrong-volume", "{step}");
        assert_eq!(Some(sum(&img.path)), *seen.borrow(), "{step}: written to the replacement");
        // The original drive comes back: its main sector is the switched one, the backup owed.
        rw(&img.path).write_all_at(switched.borrow().as_ref().unwrap(), 0).unwrap();
        assert!(!boot_regions_agree(&img.path), "{step}: the backup is owed");
        let s = run(&img.path, &o, None).unwrap_or_else(|e| panic!("{step}: the rerun failed: {e:#}"));
        assert_eq!(s.cluster, new as u64, "{step}");
        assert!(boot_regions_agree(&img.path), "{step}: the rerun completed the backup");
        fsck(&img.path);
        let (_, now) = digest(&img.path);
        same(&now, &before, step);
    }
}

/// On a block device the boot sector must be read past the page cache; when that read cannot
/// be done there is no cached fallback: nothing is written.
#[test]
fn no_write_without_a_direct_read_of_the_boot_sector() {
    let img = volume("recl-direct", 32, 8192, 3, 40, 4 << 20);
    let before = sum(&img.path);
    DIRECT_READ_FAILS.with(|c| c.set(true));
    let r = run(&img.path, &opts(Some(512), FatMode::Prefix), None);
    DIRECT_READ_FAILS.with(|c| c.set(false));
    assert_eq!(reason(r), "wrong-volume");
    assert_eq!(sum(&img.path), before, "written without a direct read");
}

/// The hardware serial comes from the first USB parent of the block device in sysfs.
#[test]
fn hardware_serial_from_sysfs() {
    let d = std::env::temp_dir().join(format!("fcp-sysfs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let usb = d.join("devices/pci0/usb2/2-1");
    let part = usb.join("2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb/sdb1");
    std::fs::create_dir_all(&part).unwrap();
    std::fs::write(usb.join("idVendor"), "0951\n").unwrap();
    std::fs::write(usb.join("serial"), "E0D55EA5 74B5\n").unwrap();
    std::fs::create_dir_all(d.join("dev/block")).unwrap();
    std::os::unix::fs::symlink(&part, d.join("dev/block/8:17")).unwrap();
    assert_eq!(hw_serial_in(&d, libc::makedev(8, 17)).as_deref(), Some("E0D55EA5_74B5"));
    assert_eq!(hw_serial_in(&d, libc::makedev(8, 33)), None, "no such device");
    std::fs::remove_file(usb.join("serial")).unwrap();
    assert_eq!(hw_serial_in(&d, libc::makedev(8, 17)), None, "no serial");
    let _ = std::fs::remove_dir_all(&d);
}

/// Device offsets in the root directory: of its end-of-directory marker, and of a live entry set.
fn root_of(p: &Path) -> (u64, u64) {
    let f = File::open(p).unwrap();
    let v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), true, false).unwrap();
    let (root, at) = (&v.dirs[0], v.cluster_off(v.dirs[0].clusters[0]));
    assert!((root.end as u64) < v.cs, "the test wants the root's entries in its first cluster");
    (at + root.end as u64, at + root.children.values().next().unwrap().set_off as u64)
}

/// Marks free clusters allocated (leaked: every command tolerates that), all or every other.
fn leak(p: &Path, every_other: bool) {
    let f = rw(p);
    let mut v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, true).unwrap();
    for (s, n) in free_extents(&v.bitmap.clone(), v.cc) {
        for c in (s..s + n).step_by(if every_other { 2 } else { 1 }) {
            v.bitmap[((c - 2) / 8) as usize] |= 1 << ((c - 2) % 8);
        }
    }
    assert_eq!(runs_of(&v.bitmap_clusters).len(), 1);
    f.write_all_at(&v.bitmap, v.cluster_off(v.bitmap_clusters[0])).unwrap();
}

#[test]
fn every_refusal_has_its_reason_and_writes_nothing() {
    type Craft = fn(&Path);
    // (reason, whether --check sees it too, old cluster, --cluster, how the volume is spoiled)
    let cases: Vec<(&str, bool, u32, Option<u32>, Craft)> = vec![
        ("dirty", true, 32768, None, |p| rw(p).write_all_at(&[2], 106).unwrap()),
        ("not-exfat", true, 32768, None, |p| rw(p).write_all_at(b"NTFS    ", 3).unwrap()),
        ("unsupported-sector-size", true, 32768, None, |p| rw(p).write_all_at(&[12], 108).unwrap()),
        // a file's name entry no longer matches its set checksum
        ("invalid-volume", true, 32768, None, |p| rw(p).write_all_at(b"!", root_of(p).1 + 70).unwrap()),
        ("backup-boot-mismatch", true, 32768, None, |p| rw(p).write_all_at(b"x", 13 * 512 + 100).unwrap()),
        ("bad-cluster-size", true, 32768, Some(3000), |_| {}),
        ("bad-cluster-size", true, 32768, Some(65536), |_| {}),
        // sync's journal file is in the root: an interrupted sync
        ("pending-sync", true, 32768, None, |p| {
            let upc = crate::exfat::upcase_map();
            let n = crate::exfat::Node {
                name: crate::volume::SYNC_JOURNAL_NAME.encode_utf16().collect(),
                src: Default::default(),
                is_dir: false,
                size: 0,
                mtime: (1_700_000_000, 0),
                children: vec![],
                first_cluster: 0,
                clusters: 0,
            };
            let mut set = crate::exfat::entry_set(&n, &upc);
            set[4] = crate::volume::SYNC_JOURNAL_ATTRS as u8;
            rw(p).write_all_at(&crate::exfat::finish_set(set, 0, 0), root_of(p).0).unwrap()
        }),
        // drive mode's journal says a commit is in flight
        ("pending-commit", true, 32768, None, |p| {
            let fs = open(p);
            let place = fs.journal_place().expect("the first commit creates the journal");
            fs.shutdown().unwrap();
            let f = rw(p);
            let v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, false).unwrap();
            let dev = FileDev::new(Arc::new(f));
            crate::drive::djournal::write_active(&dev, &v, &place, 99, b"a commit that never finished").unwrap();
        }),
        // a benign entry flashcp does not know, with clusters of its own
        ("unsupported-entry", false, 32768, None, |p| {
            let mut e = [0u8; 32];
            (e[0], e[4], e[20]) = (0xA5, 1, 9);
            rw(p).write_all_at(&e, root_of(p).0).unwrap()
        }),
        ("no-space", true, 32768, None, |p| leak(p, false)),
        // a cluster marked bad: its mark must not turn into free space
        ("bad-clusters", true, 32768, None, |p| {
            let f = rw(p);
            let v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, true).unwrap();
            let c = free_extents(&v.bitmap, v.cc).last().map(|e| e.0 + e.1 / 2).unwrap();
            f.write_all_at(&BAD.to_le_bytes(), v.fat_off + c * 4).unwrap();
            drop(v);
            set_bits(p, &[c], true);
        }),
        // a FAT chain that loops, under a DataLength far beyond the volume: refused, not walked
        // (--check does not walk files)
        ("invalid-volume", false, 32768, None, |p| {
            let f = rw(p);
            let v = Vol::open(f.try_clone().unwrap(), f.metadata().unwrap().len(), false, false).unwrap();
            let root = &v.dirs[0];
            let ch = root.children.values().find(|c| !c.is_dir && c.len > 2 * v.cs).expect("a file of 3+ clusters");
            f.write_all_at(&(ch.first + 1).to_le_bytes(), v.fat_off + ch.first as u64 * 4).unwrap();
            f.write_all_at(&ch.first.to_le_bytes(), v.fat_off + (ch.first as u64 + 1) * 4).unwrap();
            let mut set = root.data[ch.set_off..ch.set_off + ch.set_len].to_vec();
            set[33] &= !2;
            set[56..64].copy_from_slice(&(1u64 << 45).to_le_bytes());
            let sum = checksum16(&set, true);
            set[2..4].copy_from_slice(&sum.to_le_bytes());
            f.write_all_at(&set, v.cluster_off(root.clusters[0]) + ch.set_off as u64).unwrap();
        }),
        // a benign primary whose SECONDARY owns clusters
        ("unsupported-entry", false, 32768, None, |p| {
            let mut e = [0u8; 64];
            (e[0], e[1], e[32], e[33], e[52]) = (0xA5, 1, 0xE5, 1, 9);
            rw(p).write_all_at(&e, root_of(p).0).unwrap()
        }),
        // The bitmap of this volume takes two clusters, and no two free ones are adjacent.
        ("no-contiguous-space", true, 4096, Some(512), |p| leak(p, true)),
    ];
    for (k, (want, check_sees, old, new, craft)) in cases.into_iter().enumerate() {
        let img = volume(&format!("recl-refuse-{k}"), if old == 4096 { 160 } else { 24 }, old, 11, 30, 2 << 20);
        craft(&img.path);
        let before = sum(&img.path);
        assert_eq!(reason(run(&img.path, &opts(new, FatMode::Prefix), None)), want, "case {k}");
        assert_eq!(sum(&img.path), before, "{want}: the refusal wrote something");
        // --check gives the same reason (an explicit --cluster is not its business).
        if new.is_none() && check_sees {
            let c = check(&TargetSpec::Path(img.path.clone())).unwrap();
            assert!(!c.benefit && c.reason == want, "{want}: check says {c:?}");
            assert_eq!(sum(&img.path), before, "{want}: --check wrote something");
        }
    }
    // Geometries no volume in a test can have: more clusters than exFAT counts, and a heap
    // offset past the boot sector's 32-bit sector field.
    let r = |e: Result<Geo>| refusal_of(&e.unwrap_err()).unwrap().reason;
    assert_eq!(r(new_geometry(3 << 40, 4 << 20, 32 << 20, 98_000, 512)), "too-many-clusters");
    assert_eq!(r(new_geometry(9 << 50, 3 << 40, 32 << 20, 1 << 28, 4 << 20)), "geometry");
    assert_eq!(r(new_geometry(1 << 20, 1 << 19, 1 << 17, 1, 512)), "geometry");
    // --check alone: small clusters are not worth an offer; a roomy 32 KB volume is.
    let img = image("recl-check", 24, 4096);
    assert_eq!(check(&TargetSpec::Path(img.path.clone())).unwrap().reason, "small-clusters");
    let img = volume("recl-check-yes", 24, 32768, 5, 30, 2 << 20);
    let c = check(&TargetSpec::Path(img.path.clone())).unwrap();
    assert!(c.benefit && c.new_cluster == 4096 && !c.finish && c.need_free <= c.free, "{c:?}");
}
