//! Tier-1 tests of the driver, in process, against image files (no FUSE):
//! - one table of operation families (POSIX results and errnos);
//! - one model-based suite: random operation sequences against an in-memory reference, with
//!   commits and remounts in between, then fsck and a full content check;
//! - one crash harness: the same kind of sequence with a crash injected at every device write
//!   and sync in turn, under three loss models; the recovered volume must be fsck-clean and
//!   show exactly the tree before or after the interrupted commit.

use super::core::{ROOT, RenameFlags, SetAttr};
use super::dev::{CrashDev, Dev, FileDev, Loss};
use super::fs::{Fs, FsOpts, open_core};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) struct Img {
    pub(crate) dir: PathBuf,
    pub(crate) path: PathBuf,
}

impl Drop for Img {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn have(tool: &str) -> bool {
    std::process::Command::new(tool).arg("-V").output().is_ok()
}

/// A fresh volume: mkfs.exfat with this cluster size when installed, else flashcp's build.
pub(crate) fn image(name: &str, mb: u64, cluster: u32) -> Img {
    let dir = std::env::temp_dir().join(format!("fcp-drive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v.img");
    File::create(&path).unwrap().set_len(mb << 20).unwrap();
    let ok = have("mkfs.exfat")
        && std::process::Command::new("mkfs.exfat")
            .args(["-q", "-c", &cluster.to_string(), "-L", "DRIVE"])
            .arg(&path)
            .output()
            .is_ok_and(|o| o.status.success());
    if !ok {
        let src = dir.join("empty");
        std::fs::create_dir_all(&src).unwrap();
        let bo = crate::exfat::BuildOpts {
            cluster: Some(cluster),
            label: "DRIVE".into(),
            fat: crate::exfat::FatMode::Full,
            readers: 1,
            dry_run: false,
            serial: Some(0x1234_5678),
            job: None,
        };
        crate::exfat::build(&src, &path, &bo, &mut |_| {}).unwrap();
    }
    Img { dir, path }
}

pub(crate) fn manual() -> FsOpts {
    FsOpts { auto_commit: false, max_dirty: 64 << 20, readahead: false, ..Default::default() }
}

pub(crate) fn open_with(path: &Path, dev: Arc<dyn Dev>, opts: FsOpts) -> anyhow::Result<Fs> {
    let f = File::options().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    let (core, _) = open_core(f, len, &*dev, &opts)?;
    Ok(Fs::new(core, dev, opts))
}

pub(crate) fn open(path: &Path) -> Fs {
    let f = File::options().read(true).write(true).open(path).unwrap();
    open_with(path, Arc::new(FileDev::new(Arc::new(f))), manual()).unwrap()
}

pub(crate) fn fsck(path: &Path) {
    let out = match std::process::Command::new("fsck.exfat").arg("-n").arg(path).output() {
        Ok(o) => o,
        Err(_) => {
            assert!(std::env::var("FLASHCP_REQUIRE_FSCK").as_deref() != Ok("1"), "fsck.exfat missing");
            return;
        }
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("clean"), "fsck.exfat -n {}:\n{text}", path.display());
}

/// Path → Some(content) for files, None for directories.
pub(crate) type Tree = BTreeMap<String, Option<Vec<u8>>>;

pub(crate) fn snapshot(fs: &Fs) -> Tree {
    let mut out = Tree::new();
    let mut stack = vec![(ROOT, String::new())];
    while let Some((d, p)) = stack.pop() {
        for (i, name, is_dir) in fs.readdir(d).unwrap() {
            let path = format!("{p}/{name}");
            if is_dir {
                out.insert(path.clone(), None);
                stack.push((i, path));
            } else {
                let size = fs.getattr(i).unwrap().size;
                let mut data = Vec::new();
                while (data.len() as u64) < size {
                    let chunk = fs.read(i, data.len() as u64, 1 << 20).unwrap();
                    assert!(!chunk.is_empty());
                    data.extend(chunk);
                }
                out.insert(path, Some(data));
            }
        }
    }
    out
}

/// The differences between two trees, compactly (paths, sizes, first differing offset).
pub(crate) fn diff(got: &Tree, want: &Tree) -> String {
    let mut out = Vec::new();
    for (k, v) in want {
        match got.get(k) {
            None => out.push(format!("missing {k}")),
            Some(g) if g != v => match (g, v) {
                (Some(a), Some(b)) => {
                    let at = a.iter().zip(b.iter()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
                    out.push(format!("{k}: {} bytes, want {}; first difference at {at}", a.len(), b.len()));
                }
                _ => out.push(format!("{k}: type differs")),
            },
            _ => {}
        }
    }
    for k in got.keys() {
        if !want.contains_key(k) {
            out.push(format!("extra {k}"));
        }
    }
    out.join("\n")
}

macro_rules! same {
    ($got:expr, $want:expr, $($ctx:tt)*) => {{
        let (g, w) = (&$got, &$want);
        if g != w {
            panic!("{}:\n{}", format!($($ctx)*), diff(g, w));
        }
    }};
}

fn resolve(fs: &Fs, path: &str) -> Result<u64, i32> {
    let mut i = ROOT;
    for c in path.split('/').filter(|c| !c.is_empty()) {
        i = fs.lookup(i, OsStr::new(c))?.ino;
    }
    Ok(i)
}

fn split(path: &str) -> (&str, &str) {
    let k = path.rfind('/').unwrap();
    (&path[..k], &path[k + 1..])
}

// ───────────────────────── operation families ─────────────────────────

#[test]
fn operation_families() {
    let img = image("ops", 32, 4096);
    let fs = open(&img.path);
    let n = |s: &str| OsStr::new(s).to_owned();
    // create / write / read / case-insensitive lookup
    let a = fs.create(ROOT, &n("Hello.txt"), true, false).unwrap();
    assert_eq!(fs.write(a.ino, 0, b"hello world").unwrap(), 11);
    assert_eq!(fs.lookup(ROOT, &n("HELLO.TXT")).unwrap().ino, a.ino);
    assert_eq!(fs.create(ROOT, &n("hello.TXT"), true, false), Err(libc::EEXIST));
    // names
    for (bad, e) in [("a:b", libc::EINVAL), ("x.", libc::EINVAL), (&"n".repeat(256)[..], libc::ENAMETOOLONG)] {
        assert_eq!(fs.create(ROOT, &n(bad), true, false).map(|_| ()), Err(e), "{bad}");
    }
    assert_eq!(fs.create(ROOT, &n(".flashcp-drive-journal"), true, false).map(|_| ()), Err(libc::EINVAL));
    // mkdir / rmdir / unlink errors
    let d = fs.mkdir(ROOT, &n("dir")).unwrap();
    let f = fs.create(d.ino, &n("in.bin"), true, false).unwrap();
    fs.write(f.ino, 100_000, b"tail").unwrap(); // sparse: zeros before
    assert_eq!(fs.rmdir(ROOT, &n("dir")), Err(libc::ENOTEMPTY));
    assert_eq!(fs.unlink(ROOT, &n("dir")), Err(libc::EISDIR));
    assert_eq!(fs.rmdir(ROOT, &n("Hello.txt")), Err(libc::ENOTDIR));
    assert_eq!(fs.mkdir(ROOT, &n("DIR")).map(|_| ()), Err(libc::EEXIST));
    // rename: into a directory, onto a file, a dir into itself, case-only, noreplace
    let none = RenameFlags::default();
    assert_eq!(fs.rename(ROOT, &n("dir"), d.ino, &n("sub"), none), Err(libc::EINVAL));
    fs.rename(ROOT, &n("Hello.txt"), d.ino, &n("moved.txt"), none).unwrap();
    assert_eq!(
        fs.rename(d.ino, &n("moved.txt"), d.ino, &n("in.bin"), RenameFlags { noreplace: true, exchange: false }),
        Err(libc::EEXIST)
    );
    fs.commit_now("test").unwrap();
    fs.rename(d.ino, &n("moved.txt"), d.ino, &n("in.bin"), none).unwrap(); // replaces
    fs.rename(d.ino, &n("in.bin"), d.ino, &n("IN.BIN"), none).unwrap(); // case only
    assert_eq!(fs.readdir(d.ino).unwrap().into_iter().map(|e| e.1).collect::<Vec<_>>(), vec!["IN.BIN"]);
    // truncate down / up, utimens, read-only attribute
    let t = fs.lookup(d.ino, &n("in.bin")).unwrap().ino;
    fs.setattr(t, &SetAttr { size: Some(5), ..Default::default() }).unwrap();
    fs.setattr(t, &SetAttr { size: Some(9), ..Default::default() }).unwrap();
    assert_eq!(fs.read(t, 0, 100).unwrap(), b"hello\0\0\0\0");
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    let at = fs.setattr(t, &SetAttr { mtime: Some(when), mode: Some(0o444), ..Default::default() }).unwrap();
    assert!(at.read_only);
    assert_eq!(fs.open(t, true, false), Err(libc::EACCES));
    fs.setattr(t, &SetAttr { mode: Some(0o644), ..Default::default() }).unwrap();
    // statfs accounts for pending data
    let before = fs.statfs().bfree;
    let big = fs.create(ROOT, &n("big"), true, false).unwrap();
    fs.write(big.ino, 0, &vec![7u8; 1 << 20]).unwrap();
    assert!(fs.statfs().bfree <= before - 256);
    fs.flush().unwrap();
    // unlink while open: the content stays readable until release
    fs.open(big.ino, false, false).unwrap();
    fs.unlink(ROOT, &n("big")).unwrap();
    fs.flush().unwrap();
    assert_eq!(fs.read(big.ino, (1 << 20) - 1, 10).unwrap(), vec![7u8]);
    fs.release(big.ino);
    fs.release(big.ino);
    // everything survives a remount, exactly
    let want = snapshot(&fs);
    fs.shutdown().unwrap();
    fsck(&img.path);
    let fs2 = open(&img.path);
    assert_eq!(snapshot(&fs2), want);
    let st = fs2.getattr(resolve(&fs2, "/dir/IN.BIN").unwrap()).unwrap();
    assert_eq!(st.mtime, when);
    assert!(!want.contains_key("/big"));
    // ENOSPC: refused, nothing half-written
    let huge = fs2.create(ROOT, &n("huge"), true, false).unwrap();
    assert_eq!(fs2.write(huge.ino, 0, &vec![1u8; 40 << 20]), Err(libc::ENOSPC));
    fs2.shutdown().unwrap();
    fsck(&img.path);
}

// ───────────────────────── model-based suite ─────────────────────────

/// A random sequence against a reference model; commits, remounts and content checks.
fn model_run(seed: u64, steps: usize, cluster: u32) {
    let img = image(&format!("model-{seed}-{cluster}"), 48, cluster);
    let mut fs = open(&img.path);
    let mut rng = crate::util::Rng(seed);
    let mut model = Tree::new();
    let names = ["a", "b", "C", "dd", "e.txt", "a long name with spaces and more than fifteen chars.bin", "ü"];
    let pick_name = |rng: &mut crate::util::Rng| names[(rng.next() % names.len() as u64) as usize];
    let dirs = |m: &Tree| -> Vec<String> {
        let mut v: Vec<String> = m.iter().filter(|(_, c)| c.is_none()).map(|(p, _)| p.clone()).collect();
        v.push(String::new());
        v
    };
    let ci = |m: &Tree, p: &str| -> Option<String> { m.keys().find(|k| k.to_lowercase() == p.to_lowercase()).cloned() };
    let trace = std::env::var_os("FLASHCP_MODEL_TRACE").is_some();
    let log = |step: usize, m: String| {
        if trace {
            eprintln!("[{seed}/{cluster}] {step}: {m}");
        }
    };
    for step in 0..steps {
        let op = rng.next() % 100;
        let _ = trace;
        let ds = dirs(&model);
        let files: Vec<String> = model.iter().filter(|(_, c)| c.is_some()).map(|(p, _)| p.clone()).collect();
        match op {
            0..=14 => {
                let d = &ds[(rng.next() % ds.len() as u64) as usize];
                let p = format!("{d}/{}", pick_name(&mut rng));
                let r = fs.create(resolve(&fs, d).unwrap(), OsStr::new(split(&p).1), true, false);
                if ci(&model, &p).is_some() {
                    assert_eq!(r.map(|_| ()), Err(libc::EEXIST));
                } else {
                    let a = r.unwrap();
                    fs.release(a.ino);
                    log(step, format!("create {p}"));
                    model.insert(p, Some(Vec::new()));
                }
            }
            15..=21 => {
                let d = &ds[(rng.next() % ds.len() as u64) as usize];
                let p = format!("{d}/{}{}", pick_name(&mut rng), "_d");
                if p.matches('/').count() > 4 {
                    continue;
                }
                let r = fs.mkdir(resolve(&fs, d).unwrap(), OsStr::new(split(&p).1));
                if ci(&model, &p).is_some() {
                    assert_eq!(r.map(|_| ()), Err(libc::EEXIST));
                } else {
                    r.unwrap();
                    log(step, format!("mkdir {p}"));
                    model.insert(p, None);
                }
            }
            22..=49 if !files.is_empty() => {
                let p = &files[(rng.next() % files.len() as u64) as usize];
                let content = model.get_mut(p).unwrap().as_mut().unwrap();
                let max = [100u64, 5000, 70_000, 600_000][(rng.next() % 4) as usize];
                let off = rng.next() % (content.len() as u64 + max);
                let len = 1 + rng.next() % max;
                let mut data = vec![0u8; len as usize];
                rng.fill(&mut data);
                let i = resolve(&fs, p).unwrap();
                fs.write(i, off, &data).unwrap();
                log(step, format!("write {p} {off}+{len}"));
                let end = (off + len) as usize;
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[off as usize..end].copy_from_slice(&data);
            }
            50..=57 if !files.is_empty() => {
                let p = &files[(rng.next() % files.len() as u64) as usize];
                let content = model.get_mut(p).unwrap().as_mut().unwrap();
                let size = rng.next() % (content.len() as u64 * 2 + 10);
                fs.setattr(resolve(&fs, p).unwrap(), &SetAttr { size: Some(size), ..Default::default() }).unwrap();
                content.resize(size as usize, 0);
                log(step, format!("truncate {p} {size}"));
            }
            58..=63 if !files.is_empty() => {
                let p = files[(rng.next() % files.len() as u64) as usize].clone();
                let (d, n) = split(&p);
                fs.unlink(resolve(&fs, d).unwrap(), OsStr::new(n)).unwrap();
                log(step, format!("unlink {p}"));
                model.remove(&p);
            }
            64..=67 => {
                let empty: Vec<String> = ds
                    .iter()
                    .filter(|d| !d.is_empty() && !model.keys().any(|k| k.starts_with(&format!("{d}/"))))
                    .cloned()
                    .collect();
                if let Some(p) = empty.first() {
                    let (d, n) = split(p);
                    fs.rmdir(resolve(&fs, d).unwrap(), OsStr::new(n)).unwrap();
                    log(step, format!("rmdir {p}"));
                    model.remove(p);
                }
            }
            68..=79 => {
                let all: Vec<String> = model.keys().cloned().collect();
                if all.is_empty() {
                    continue;
                }
                let src = all[(rng.next() % all.len() as u64) as usize].clone();
                let d = &ds[(rng.next() % ds.len() as u64) as usize];
                let dst = format!("{d}/{}", pick_name(&mut rng));
                let (sd, sn) = split(&src);
                let r = fs.rename(
                    resolve(&fs, sd).unwrap(),
                    OsStr::new(sn),
                    resolve(&fs, d).unwrap(),
                    OsStr::new(split(&dst).1),
                    RenameFlags::default(),
                );
                // the model's verdict
                let src_dir = model[&src].is_none();
                let into_self = src_dir && (dst.to_lowercase() + "/").starts_with(&(src.to_lowercase() + "/"));
                let existing = ci(&model, &dst);
                let expect: Result<(), i32> = if into_self && existing.as_deref() != Some(&src) {
                    Err(libc::EINVAL)
                } else if let Some(e) = &existing {
                    if e == &src {
                        Ok(())
                    } else {
                        match (src_dir, model[e].is_none()) {
                            (true, false) => Err(libc::ENOTDIR),
                            (false, true) => Err(libc::EISDIR),
                            (true, true) if model.keys().any(|k| k.starts_with(&format!("{e}/"))) => {
                                Err(libc::ENOTEMPTY)
                            }
                            _ => Ok(()),
                        }
                    }
                } else {
                    Ok(())
                };
                assert_eq!(r, expect, "step {step}: rename {src} -> {dst}");
                if r.is_ok() {
                    log(step, format!("rename {src} -> {dst}"));
                    if let Some(e) = existing.filter(|e| e != &src) {
                        model.remove(&e);
                    }
                    let moved: Vec<(String, Option<Vec<u8>>)> = model
                        .iter()
                        .filter(|(k, _)| *k == &src || k.starts_with(&format!("{src}/")))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    for (k, _) in &moved {
                        model.remove(k);
                    }
                    for (k, v) in moved {
                        model.insert(format!("{dst}{}", &k[src.len()..]), v);
                    }
                }
            }
            80..=91 => {
                log(step, "commit".into());
                fs.commit_now("test").map(|_| ()).unwrap()
            }
            92..=95 => {
                fs.shutdown().unwrap();
                fsck(&img.path);
                fs = open(&img.path);
                same!(snapshot(&fs), model, "after remount at step {step}");
            }
            _ => {
                same!(snapshot(&fs), model, "live view at step {step}");
            }
        }
    }
    fs.shutdown().unwrap();
    fsck(&img.path);
    let fs = open(&img.path);
    same!(snapshot(&fs), model, "final");
    fs.shutdown().unwrap();
}

#[test]
fn model_random_sequences() {
    // A bounded corpus: a few seeds per cluster size (512 B clusters hit multi-cluster entry
    // sets and directory growth; 64 KB clusters slack and in-place tails).
    let seeds: u64 = std::env::var("FLASHCP_MODEL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    for cluster in [512u32, 4096, 65536] {
        for seed in 0..seeds {
            model_run(seed * 7919 + cluster as u64, 300, cluster);
        }
    }
}

// ───────────────────────── crash harness ─────────────────────────

/// One scenario: ops applied between commits (each closure ends in a state to commit).
type Step = fn(&Fs);

fn scenario() -> Vec<Step> {
    vec![
        |fs| {
            let d = fs.mkdir(ROOT, OsStr::new("photos")).unwrap();
            for k in 0..12 {
                let f = fs.create(d.ino, OsStr::new(&format!("img{k:02}.jpg")), true, false).unwrap();
                fs.write(f.ino, 0, &vec![k as u8 + 1; 3000 + k * 1000]).unwrap();
                fs.release(f.ino);
            }
            let f = fs.create(ROOT, OsStr::new("notes.txt"), true, false).unwrap();
            fs.write(f.ino, 0, b"version one").unwrap();
            fs.release(f.ino);
        },
        |fs| {
            // overwrite in place (copy-on-write), append, truncate, rename across dirs,
            // replace a target, delete, a big file's middle
            let n = fs.lookup(ROOT, OsStr::new("notes.txt")).unwrap().ino;
            fs.write(n, 0, b"VERSION TWO, longer").unwrap();
            let p = fs.lookup(ROOT, OsStr::new("photos")).unwrap().ino;
            let a = fs.lookup(p, OsStr::new("img03.jpg")).unwrap().ino;
            fs.write(a, 6000, &[0xAA; 9000]).unwrap();
            let b = fs.lookup(p, OsStr::new("img04.jpg")).unwrap().ino;
            fs.setattr(b, &SetAttr { size: Some(100), ..Default::default() }).unwrap();
            let d2 = fs.mkdir(ROOT, OsStr::new("album")).unwrap();
            fs.rename(p, OsStr::new("img05.jpg"), d2.ino, OsStr::new("best.jpg"), RenameFlags::default()).unwrap();
            fs.rename(p, OsStr::new("img06.jpg"), p, OsStr::new("img07.jpg"), RenameFlags::default()).unwrap();
            fs.unlink(p, OsStr::new("img08.jpg")).unwrap();
            let big = fs.create(ROOT, OsStr::new("video.bin"), true, false).unwrap();
            fs.write(big.ino, 0, &vec![0x5A; 6 << 20]).unwrap();
            fs.release(big.ino);
        },
        |fs| {
            let v = fs.lookup(ROOT, OsStr::new("video.bin")).unwrap().ino;
            fs.write(v, 3 << 20, &[0x11; 70_000]).unwrap(); // copy-on-write of a few clusters
            fs.write(v, 6 << 20, &[0x22; 50_000]).unwrap(); // extension
            let p = fs.lookup(ROOT, OsStr::new("photos")).unwrap().ino;
            for k in 0..10 {
                let _ = fs.unlink(p, OsStr::new(&format!("img{k:02}.jpg")));
            }
            let _ = fs.unlink(p, OsStr::new("img10.jpg"));
            let _ = fs.unlink(p, OsStr::new("img11.jpg"));
            fs.rmdir(ROOT, OsStr::new("photos")).unwrap();
            fs.rename(
                ROOT,
                OsStr::new("album"),
                ROOT,
                OsStr::new("Album 2024 — the long renamed one"),
                RenameFlags::default(),
            )
            .unwrap();
        },
    ]
}

/// Runs the scenario on `img` with a device that fails from op `fail_at` on; returns the
/// snapshots after each completed commit (the first is the empty start) and the device.
fn run_scenario(path: &Path, fail_at: Option<u64>) -> (Vec<Tree>, Arc<CrashDev>, bool) {
    let f = File::options().read(true).write(true).open(path).unwrap();
    let dev = Arc::new(CrashDev::new(f, fail_at));
    let fs = open_with(path, dev.clone(), manual()).unwrap();
    let mut snaps = vec![snapshot(&fs)];
    for step in scenario() {
        step(&fs);
        let want = snapshot(&fs);
        match fs.flush() {
            Ok(()) => snaps.push(want),
            Err(_) => {
                snaps.push(want);
                return (snaps, dev, true);
            }
        }
    }
    (snaps, dev, false)
}

#[test]
fn crash_at_every_device_op() {
    for cluster in [4096u32, 512] {
        let img = image(&format!("crash-{cluster}"), 32, cluster);
        let template = img.dir.join("template.img");
        std::fs::copy(&img.path, &template).unwrap();
        let (_, dev, failed) = run_scenario(&img.path, None);
        assert!(!failed);
        let total = dev.ops();
        let stride: u64 = std::env::var("FLASHCP_CRASH_STRIDE").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
        let mut k = 0;
        while k < total {
            for (li, loss) in [Loss::None, Loss::All, Loss::Random(k * 31 + 7)].into_iter().enumerate() {
                std::fs::copy(&template, &img.path).unwrap();
                let (snaps, dev, failed) = run_scenario(&img.path, Some(k));
                assert!(failed, "op {k} of {total} did not fail");
                dev.crash(loss);
                drop(dev);
                // recovery happens on open
                let f = File::options().read(true).write(true).open(&img.path).unwrap();
                let fs = match open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), manual()) {
                    Ok(fs) => fs,
                    Err(e) => panic!("crash at op {k}/{total} ({loss:?}, cluster {cluster}): recovery failed: {e:#}"),
                };
                let got = snapshot(&fs);
                let n = snaps.len();
                assert!(
                    got == snaps[n - 2] || got == snaps[n - 1],
                    "crash at op {k}/{total} ({loss:?}, cluster {cluster}): the volume shows neither the tree \
                     before nor after commit {}",
                    n - 1
                );
                fs.shutdown().unwrap();
                if li == 0 || k % 5 == 0 {
                    fsck(&img.path);
                }
            }
            k += stride;
        }
    }
}

/// Readahead: a bulk read of a directory (and a big file) is served from the cache, and
/// commits that free and reuse clusters in between never make it serve stale bytes.
#[test]
fn readahead_serves_bulk_reads_and_never_stale_data() {
    let img = image("ra", 64, 4096);
    let fs = open(&img.path);
    let d = fs.mkdir(ROOT, OsStr::new("d")).unwrap();
    let mut rng = crate::util::Rng(5);
    for k in 0..80u64 {
        let f = fs.create(d.ino, OsStr::new(&format!("f{k:03}")), true, false).unwrap();
        let mut b = vec![0u8; 1000 + (k as usize * 997) % 30_000];
        rng.fill(&mut b);
        fs.write(f.ino, 0, &b).unwrap();
        fs.release(f.ino);
    }
    let big = fs.create(ROOT, OsStr::new("big"), true, false).unwrap();
    let mut b = vec![0u8; 24 << 20];
    rng.fill(&mut b);
    fs.write(big.ino, 0, &b).unwrap();
    fs.shutdown().unwrap();
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let opts = FsOpts { readahead: true, ..manual() };
    let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    let want = snapshot(&open(&img.path));
    let mut got = Tree::new();
    let mut deleted = 0;
    for (i, name, _) in fs.readdir(resolve(&fs, "/d").unwrap()).unwrap() {
        let Ok(st) = fs.getattr(i) else { continue }; // deleted below
        let size = st.size;
        let mut data = Vec::new();
        while (data.len() as u64) < size {
            data.extend(fs.read(i, data.len() as u64, 128 << 10).unwrap());
        }
        got.insert(format!("/d/{name}"), Some(data));
        // free a file further on and fill the space with something else, committed
        if deleted < 10 && name.ends_with('5') {
            let victim = format!("f{:03}", name[1..].parse::<u64>().unwrap() + 10);
            let dd = resolve(&fs, "/d").unwrap();
            if fs.unlink(dd, OsStr::new(&victim)).is_ok() {
                deleted += 1;
                fs.commit_now("t").unwrap();
                let n = fs.create(ROOT, OsStr::new(&format!("new{deleted}")), true, false).unwrap();
                fs.write(n.ino, 0, &vec![0xEE; 40_000]).unwrap();
                fs.release(n.ino);
                fs.commit_now("t").unwrap();
            }
        }
    }
    let bi = resolve(&fs, "/big").unwrap();
    let mut data = Vec::new();
    while data.len() < 24 << 20 {
        data.extend(fs.read(bi, data.len() as u64, 128 << 10).unwrap());
    }
    got.insert("/big".into(), Some(data));
    for (k, v) in &got {
        if let Some(w) = want.get(k) {
            assert!(w == v, "{k} read back wrong");
        }
    }
    let (hits, misses) = fs.ra_stats().unwrap();
    assert!(hits > misses, "readahead hits {hits}, misses {misses}");
    fs.shutdown().unwrap();
}

/// The other commands on a volume drive mode used: sync --delete and add leave the journal
/// file alone (and refuse while a commit is pending), pull and info skip it.
#[test]
fn add_sync_pull_respect_the_drive_journal() {
    let img = image("interop", 32, 4096);
    let fs = open(&img.path);
    let f = fs.create(ROOT, OsStr::new("old.txt"), true, false).unwrap();
    fs.write(f.ino, 0, b"old").unwrap();
    fs.shutdown().unwrap();
    let src = img.dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("new.txt"), b"new").unwrap();
    let so = crate::sync::SyncOpts { delete: true, yes: true, readers: 1, ..Default::default() };
    let st = crate::sync::sync(&src, &img.path, &so, &mut |_| {}).unwrap();
    assert_eq!(st.deleted, 1, "only old.txt goes");
    let info = crate::add::info(&img.path).unwrap();
    assert_eq!(info.files, 1, "info counts user files only");
    let out = img.dir.join("out");
    crate::pull::pull(&img.path, &out, &crate::pull::PullOpts::default(), &mut |_| {}).unwrap();
    let names: Vec<_> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec![std::ffi::OsString::from("new.txt")]);
    // the journal is still there and drive mode keeps working on it
    let fs = open(&img.path);
    let f = fs.create(ROOT, OsStr::new("again"), true, false).unwrap();
    fs.write(f.ino, 0, b"x").unwrap();
    fs.shutdown().unwrap();
    fsck(&img.path);
    // a pending commit makes add refuse
    use std::os::unix::fs::FileExt;
    let fs = open(&img.path);
    let place = fs.journal_place().unwrap();
    fs.shutdown().unwrap();
    let rw = File::options().read(true).write(true).open(&img.path).unwrap();
    let v = crate::volume::Vol::open(rw.try_clone().unwrap(), rw.metadata().unwrap().len(), true, false).unwrap();
    let base = v.cluster_off(place.first);
    let mut h = vec![0u8; 512];
    rw.read_exact_at(&mut h, base).unwrap();
    h[24] = 1; // ACTIVE (the header hash now fails: still "idle"), so craft a valid one
    let body = super::djournal::encode_body(&v, &[], &[]).unwrap();
    super::djournal::write_active(&FileDev::new(Arc::new(rw.try_clone().unwrap())), &v, &place, 9, &body).unwrap();
    let ao = crate::add::AddOpts { readers: 1, ..Default::default() };
    let e = crate::add::add(&src, &img.path, &ao, &mut |_| {}).unwrap_err();
    assert!(format!("{e:#}").contains("interrupted commit of flashcp mount"), "{e:#}");
    // mounting replays it (nothing to write) and clears it
    let fs = open(&img.path);
    fs.shutdown().unwrap();
    let ao = crate::add::AddOpts { readers: 1, to: Some("sub".into()), ..Default::default() };
    crate::add::add(&src, &img.path, &ao, &mut |_| {}).unwrap();
    fsck(&img.path);
}

/// Benchmark harness (ignored): reads the whole volume in FLASHCP_RA_BENCH_IMG like `cp -r`
/// does (readdir order, 128 KB reads), with readahead, under FLASHCP_TRACE when set, for
/// `flashcp sim run`.
#[test]
#[ignore]
fn ra_bench_trace() {
    let Some(img) = std::env::var_os("FLASHCP_RA_BENCH_IMG") else { return };
    let path = PathBuf::from(img);
    crate::sim::trace::init_from_env("mount-read").unwrap();
    let f = File::options().read(true).write(true).open(&path).unwrap();
    let opts = FsOpts { readahead: std::env::var_os("FLASHCP_RA_OFF").is_none(), ..manual() };
    let fs = open_with(&path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    let t = std::time::Instant::now();
    let mut bytes = 0u64;
    let mut stack = vec![ROOT];
    while let Some(d) = stack.pop() {
        for (i, _, is_dir) in fs.readdir(d).unwrap() {
            if is_dir {
                stack.push(i);
                continue;
            }
            let size = fs.getattr(i).unwrap().size;
            let mut off = 0;
            while off < size {
                let n = fs.read(i, off, 128 << 10).unwrap().len() as u64;
                off += n;
            }
            bytes += size;
        }
    }
    eprintln!("read {bytes} bytes in {:.3} s, readahead {:?}", t.elapsed().as_secs_f64(), fs.ra_stats());
    fs.shutdown().unwrap();
    crate::sim::trace::finish();
}

/// Benchmark harness (ignored): copies FLASHCP_WR_BENCH_SRC into the (empty) volume in
/// FLASHCP_RA_BENCH_IMG like `cp -r` (128 KB writes), then one flush, under FLASHCP_TRACE.
#[test]
#[ignore]
fn wr_bench_trace() {
    let (Some(img), Some(src)) = (std::env::var_os("FLASHCP_RA_BENCH_IMG"), std::env::var_os("FLASHCP_WR_BENCH_SRC"))
    else {
        return;
    };
    let path = PathBuf::from(img);
    crate::sim::trace::init_from_env("mount-write").unwrap();
    let f = File::options().read(true).write(true).open(&path).unwrap();
    let opts = FsOpts { max_dirty: 1 << 30, ..manual() };
    let fs = open_with(&path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    let t = std::time::Instant::now();
    fn copy(fs: &Fs, dir: u64, src: &Path) {
        let mut ents: Vec<_> = std::fs::read_dir(src).unwrap().map(|e| e.unwrap()).collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            let p = e.path();
            if p.is_dir() {
                let d = fs.mkdir(dir, &e.file_name()).unwrap();
                copy(fs, d.ino, &p);
            } else {
                let data = std::fs::read(&p).unwrap();
                let a = fs.create(dir, &e.file_name(), true, false).unwrap();
                for (k, c) in data.chunks(128 << 10).enumerate() {
                    fs.write(a.ino, (k << 17) as u64, c).unwrap();
                }
                fs.release(a.ino);
            }
        }
    }
    copy(&fs, ROOT, Path::new(&src));
    let staged = t.elapsed().as_secs_f64();
    fs.flush().unwrap();
    eprintln!("staged in {staged:.3} s, durable at {:.3} s", t.elapsed().as_secs_f64());
    fs.shutdown().unwrap();
    crate::sim::trace::finish();
}

/// A batch whose journal would not fit the journal file is committed in parts, each one
/// complete; the tree after the last is exact.
#[test]
fn oversized_batches_split_into_commits() {
    let img = image("split", 64, 4096);
    // The hook is read when a journal file is created: set it only around this volume's
    // first commit (another test creating its journal meanwhile just gets a small one).
    let fs = open(&img.path);
    unsafe { std::env::set_var(super::djournal::JOURNAL_BYTES_ENV, "65536") };
    let d = fs.mkdir(ROOT, OsStr::new("many")).unwrap();
    fs.commit_now("t").unwrap(); // creates the small journal
    unsafe { std::env::remove_var(super::djournal::JOURNAL_BYTES_ENV) };
    let mut rng = crate::util::Rng(77);
    for _ in 0..3000 {
        // random names: the journal compresses well otherwise
        let name: String = (0..40).map(|_| char::from(b'a' + (rng.next() % 26) as u8)).collect();
        let f = fs.create(d.ino, OsStr::new(&name), true, false).unwrap();
        fs.release(f.ino);
    }
    let before = fs.commits();
    fs.flush().unwrap();
    assert!(fs.commits() > before + 1, "expected a split commit");
    let want = snapshot(&fs);
    fs.shutdown().unwrap();
    fsck(&img.path);
    same!(snapshot(&open(&img.path)), want, "after split commits");
}

/// Review regressions: a committed directory renamed and retimed right after mount (never
/// loaded); a big split batch of deletes, moves out of a removed directory and a replaced
/// target; flush while a writer keeps writing.
#[test]
fn review_regressions() {
    let img = image("regress", 64, 4096);
    let fs = open(&img.path);
    let n = |s: &str| OsStr::new(s).to_owned();
    let d = fs.mkdir(ROOT, &n("olddir")).unwrap();
    let keep = fs.create(d.ino, &n("keep.txt"), true, false).unwrap();
    fs.write(keep.ino, 0, b"kept").unwrap();
    let mut rng = crate::util::Rng(9);
    let big = fs.mkdir(ROOT, &n("big")).unwrap();
    for _ in 0..1500 {
        let name: String = (0..40).map(|_| char::from(b'a' + (rng.next() % 26) as u8)).collect();
        let f = fs.create(big.ino, &n(&name), true, false).unwrap();
        fs.release(f.ino);
    }
    let t = fs.create(ROOT, &n("target"), true, false).unwrap();
    fs.write(t.ino, 0, b"old target").unwrap();
    fs.shutdown().unwrap();
    // 1: rename + utimens of a directory nobody listed since the mount
    let fs = open(&img.path);
    fs.rename(ROOT, &n("olddir"), ROOT, &n("newdir"), RenameFlags::default()).unwrap();
    let nd = fs.lookup(ROOT, &n("newdir")).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    fs.setattr(nd.ino, &SetAttr { mtime: Some(when), ..Default::default() }).unwrap();
    fs.flush().unwrap();
    fs.shutdown().unwrap();
    // 2: on a volume with a small journal (split commits): delete everything in big/, move
    //    keep.txt out of a directory that goes, replace a target
    let img2 = image("regress-split", 64, 4096);
    let fs = open(&img2.path);
    unsafe { std::env::set_var(super::djournal::JOURNAL_BYTES_ENV, "12288") };
    let nd = fs.mkdir(ROOT, &n("newdir")).unwrap().ino;
    fs.commit_now("t").unwrap();
    unsafe { std::env::remove_var(super::djournal::JOURNAL_BYTES_ENV) };
    let keep = fs.create(nd, &n("keep.txt"), true, false).unwrap();
    fs.write(keep.ino, 0, b"kept").unwrap();
    let b = fs.mkdir(ROOT, &n("big")).unwrap().ino;
    for _ in 0..1500 {
        let name: String = (0..40).map(|_| char::from(b'a' + (rng.next() % 26) as u8)).collect();
        let f = fs.create(b, &n(&name), true, false).unwrap();
        fs.release(f.ino);
    }
    let t = fs.create(ROOT, &n("target"), true, false).unwrap();
    fs.write(t.ino, 0, b"old target").unwrap();
    fs.flush().unwrap();
    let before = fs.commits();
    // all but one (removing a whole directory needs no entry writes inside it)
    for (_, name, _) in fs.readdir(b).unwrap().into_iter().skip(1) {
        fs.unlink(b, &n(&name)).unwrap();
    }
    fs.rename(nd, &n("keep.txt"), ROOT, &n("target"), RenameFlags::default()).unwrap();
    fs.rmdir(ROOT, &n("newdir")).unwrap();
    let want = snapshot(&fs);
    fs.flush().unwrap();
    assert!(fs.commits() > before + 1, "expected split commits");
    fs.shutdown().unwrap();
    fsck(&img2.path);
    same!(snapshot(&open(&img2.path)), want, "after deletes and moves");
    assert_eq!(want.get("/target").cloned().flatten().as_deref(), Some(&b"kept"[..]));
    // 3: flush returns while another thread keeps writing
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let opts = FsOpts { auto_commit: true, idle: std::time::Duration::from_millis(50), readahead: false, ..manual() };
    let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    let w = fs.create(ROOT, &n("stream"), true, false).unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (fs2, stop2) = (fs.clone(), stop.clone());
    let writer = std::thread::spawn(move || {
        let mut off = 0u64;
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            fs2.write(w.ino, off, &[7u8; 4096]).unwrap();
            off = (off + 4096) % (4 << 20);
        }
    });
    for _ in 0..5 {
        fs.flush().unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    fs.shutdown().unwrap();
    fsck(&img.path);
}
