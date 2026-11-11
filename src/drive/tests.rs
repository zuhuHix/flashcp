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
    let md = f.metadata()?;
    let len = if std::os::unix::fs::FileTypeExt::is_block_device(&md.file_type()) {
        crate::exfat::blk_size(&f)?
    } else {
        md.len()
    };
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
    // inode numbers follow the on-disk order (tree copiers open files in inode order)
    let listing = fs.readdir(resolve(&fs, "/d").unwrap()).unwrap();
    assert!(listing.windows(2).all(|w| w[0].0 < w[1].0), "inode numbers are not in on-disk order");
    // a second pass over the directory, later, is prefetched again (re-armed)
    std::thread::sleep(std::time::Duration::from_millis(3200));
    let (h1, _) = fs.ra_stats().unwrap();
    for (i, _, _) in &listing {
        let size = fs.getattr(*i).unwrap().size;
        let mut off = 0;
        while off < size {
            off += fs.read(*i, off, 128 << 10).unwrap().len() as u64;
        }
    }
    let (h2, _) = fs.ra_stats().unwrap();
    assert!(h2 - h1 >= listing.len() as u64 / 2, "second pass: only {} hits for {} files", h2 - h1, listing.len());
    fs.shutdown().unwrap();
}

/// Counts that would show a regression without timing anything: one fsync of a new small
/// file costs 5 device writes and 5 syncs (data, journal, bitmap, entry set, journal IDLE: a
/// light commit has no VolumeDirty), and from the commit thread, which leaves the IDLE header
/// to a commit that does not follow and a directory's times to the next commit, 4 + 4; a
/// commit onto a volume full of holes writes its
/// batch as one stream; a reader
/// taking 64 KB heads of 2 MB files makes the readahead read heads, not whole files.
#[test]
fn device_op_counts() {
    // fsync: device writes and syncs of one small file's commit
    let img = image("counts", 64, 4096);
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let dev = Arc::new(CrashDev::new(f, None));
    let fs = open_with(&img.path, dev.clone(), manual()).unwrap();
    let a = fs.create(ROOT, OsStr::new("warm"), true, false).unwrap();
    fs.write(a.ino, 0, b"journal file created").unwrap();
    fs.mkdir(ROOT, OsStr::new("sub")).unwrap();
    fs.flush().unwrap();
    let (before, light) = (dev.ops(), fs.light_commits());
    let b = fs.create(ROOT, OsStr::new("one.txt"), true, false).unwrap();
    fs.write(b.ino, 0, &[1u8; 8192]).unwrap();
    fs.fsync(b.ino).unwrap();
    let ops = dev.ops() - before; // writes + syncs
    eprintln!("count: fsync of one small file: {ops} device writes + syncs");
    // (+1 write: this set would cross a sector, so deleted entries pad up to the next one)
    assert!(ops <= 11, "one fsync took {ops} device writes and syncs (6 + 5 expected)");
    assert_eq!(fs.light_commits(), light + 1, "the fsync's commit was not light");
    fs.shutdown().unwrap();
    // the same from the commit thread: the IDLE header waits for a commit that does not come
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let dev = Arc::new(CrashDev::new(f, None));
    // (no idle commit in the way: a loaded machine could take a second between these lines)
    let opts = FsOpts {
        auto_commit: true,
        idle: std::time::Duration::from_secs(3600),
        idle_header: std::time::Duration::from_secs(3600),
        ..manual()
    };
    let fs = open_with(&img.path, dev.clone(), opts).unwrap();
    let before = dev.ops();
    // the first set of a directory: one sector (a set across two takes a full commit); the
    // directory's own times wait for the next commit that is not an fsync's: data, journal,
    // bitmap, entry set, 4 syncs, what the kernel driver writes per fsynced file
    let sub = fs.lookup(ROOT, OsStr::new("sub")).unwrap().ino;
    let mt0 = fs.getattr(sub).unwrap().mtime;
    // (past the 10 ms resolution of exFAT's times: the directory's times do change)
    std::thread::sleep(std::time::Duration::from_millis(30));
    let b = fs.create(sub, OsStr::new("two.txt"), true, false).unwrap();
    fs.write(b.ino, 0, &[2u8; 8192]).unwrap();
    fs.fsync(b.ino).unwrap();
    let ops = dev.ops() - before;
    eprintln!("count: fsync of one small file, commit thread: {ops} device writes + syncs");
    assert_eq!(fs.light_commits(), 1, "the fsync's commit was not light");
    assert!(ops <= 8, "one fsync took {ops} device writes and syncs (4 + 4 expected)");
    assert_eq!(fs.status().dirty_files, 1, "the directory's times were not left for later");
    let mt1 = fs.getattr(sub).unwrap().mtime;
    assert_ne!(mt0, mt1);
    // the next commit that is not an fsync's writes them
    fs.release(b.ino);
    fs.flush().unwrap();
    assert_eq!(fs.status().dirty_files, 0, "the directory's times stayed pending");
    fs.shutdown().unwrap();
    fsck(&img.path);
    let again = open(&img.path);
    assert_eq!(again.getattr(again.lookup(ROOT, OsStr::new("sub")).unwrap().ino).unwrap().mtime, mt1);
    again.shutdown().unwrap();
    // a volume full of holes: 600 files, every other one deleted, then 300 new files
    let img = image("holes", 64, 4096);
    let fs = open(&img.path);
    let d = fs.mkdir(ROOT, OsStr::new("old")).unwrap();
    for k in 0..600 {
        let f = fs.create(d.ino, OsStr::new(&format!("o{k:03}")), true, false).unwrap();
        fs.write(f.ino, 0, &[7u8; 4096]).unwrap();
    }
    fs.flush().unwrap();
    for k in (0..600).step_by(2) {
        fs.unlink(d.ino, OsStr::new(&format!("o{k:03}"))).unwrap();
    }
    fs.flush().unwrap();
    fs.shutdown().unwrap();
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let dev = Arc::new(CrashDev::new(f, None));
    let fs = open_with(&img.path, dev.clone(), manual()).unwrap();
    let n = fs.mkdir(ROOT, OsStr::new("new")).unwrap();
    for k in 0..300 {
        let f = fs.create(n.ino, OsStr::new(&format!("n{k:03}")), true, false).unwrap();
        fs.write(f.ino, 0, &[9u8; 4096]).unwrap();
    }
    let before = dev.ops();
    fs.flush().unwrap();
    let ops = dev.ops() - before;
    eprintln!("count: 300-file commit onto 300 holes: {ops} device ops");
    assert!(ops < 40, "a 300-file commit onto a holey volume took {ops} device ops (one per hole before)");
    fs.shutdown().unwrap();
    fsck(&img.path);
    // heads: 64 KB of each of 40 files of 2 MB
    let img = image("heads", 128, 4096);
    let fs = open(&img.path);
    let d = fs.mkdir(ROOT, OsStr::new("photos")).unwrap();
    for k in 0..40 {
        let f = fs.create(d.ino, OsStr::new(&format!("p{k:02}.jpg")), true, false).unwrap();
        fs.write(f.ino, 0, &vec![k as u8; 2 << 20]).unwrap();
    }
    fs.shutdown().unwrap();
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), FsOpts { readahead: true, ..manual() }).unwrap();
    let dir = resolve(&fs, "/photos").unwrap();
    for (i, _, _) in fs.readdir(dir).unwrap() {
        assert_eq!(fs.read(i, 0, 64 << 10).unwrap().len(), 64 << 10);
    }
    let read = fs.ra_bytes();
    eprintln!("count: 64 KB heads of 40 x 2 MB files: readahead read {read} bytes");
    assert!(read < 16 << 20, "a head reader made the readahead read {read} bytes of 80 MB");
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

/// Benchmark harness (ignored): an application that fsyncs every file it writes (a file
/// manager's copy), serially: per file create, 128 KB writes, fsync, close. The tree comes
/// from the shape file FLASHCP_WR_BENCH_SHAPE (one line per entry in copy order: `d DEPTH
/// NAMELEN` or `f DEPTH NAMELEN SIZE`, no names), with generated content, into the volume in
/// FLASHCP_RA_BENCH_IMG, under FLASHCP_TRACE. The commit thread runs (default options), so
/// fsyncs take the path a mount takes.
#[test]
#[ignore]
fn wr_fsync_bench_trace() {
    let (Some(img), Some(shape)) =
        (std::env::var_os("FLASHCP_RA_BENCH_IMG"), std::env::var_os("FLASHCP_WR_BENCH_SHAPE"))
    else {
        return;
    };
    let path = PathBuf::from(img);
    let shape = std::fs::read_to_string(shape).unwrap();
    crate::sim::trace::init_from_env("mount-write-fsync").unwrap();
    let f = File::options().read(true).write(true).open(&path).unwrap();
    let opts = FsOpts { readahead: false, ..Default::default() };
    let fs = open_with(&path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    let t = std::time::Instant::now();
    let mut stack = vec![ROOT];
    let (mut files, mut bytes, mut k) = (0u64, 0u64, 0u64);
    let mut rng = crate::util::Rng(0x5eed);
    let mut buf = vec![0u8; 128 << 10];
    for l in shape.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let depth: usize = w[1].parse().unwrap();
        let nl: usize = w[2].parse().unwrap();
        stack.truncate(depth + 1);
        k += 1;
        let mut name = format!("{k:x}-");
        while name.len() < nl.max(name.len()) {
            name.push('n');
        }
        let parent = *stack.last().unwrap();
        if w[0] == "d" {
            let d = fs.mkdir(parent, OsStr::new(&name)).unwrap();
            stack.push(d.ino);
            continue;
        }
        let size: u64 = w[3].parse().unwrap();
        let a = fs.create(parent, OsStr::new(&name), true, false).unwrap();
        let mut off = 0u64;
        while off < size {
            let n = (size - off).min(buf.len() as u64) as usize;
            for c in buf[..n].chunks_mut(8) {
                let x = rng.next().to_le_bytes();
                c.copy_from_slice(&x[..c.len()]);
            }
            fs.write(a.ino, off, &buf[..n]).unwrap();
            off += n as u64;
        }
        fs.fsync(a.ino).unwrap();
        fs.release(a.ino);
        files += 1;
        bytes += size;
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!("{files} files, {bytes} bytes, fsync each: {secs:.3} s on the host, {} commits", fs.commits());
    fs.shutdown().unwrap();
    crate::sim::trace::finish();
}

/// Whether the journal header on the image at `path` is ACTIVE.
fn journal_active(path: &Path) -> bool {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len();
    let mut v = crate::volume::Vol::open_boot(f, len, false, false).unwrap();
    let p = super::djournal::locate_raw(&mut v, crate::volume::MAX_DIR_BYTES).unwrap().unwrap();
    super::djournal::header_state(&v, &p) == Some(true)
}

/// The volume at `path` as another system reads it, when VolumeDirty is clear: fsck-clean,
/// and no entry in use after a directory's end-of-directory entry (which fsck.exfat does not
/// look at: a set there is silently lost, and the volume breaks the spec).
fn foreign_check(path: &Path, at: &str) {
    if volume_dirty(path) {
        return; // the other system checks it
    }
    fsck(path);
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len();
    let mut v = crate::volume::Vol::open(f, len, false, false).unwrap_or_else(|e| panic!("{at}: {e:#}"));
    let check = |path: &str, data: &[u8], end: usize| {
        let late = (end..data.len()).step_by(32).find(|&o| data[o] != 0);
        assert!(late.is_none(), "{at}: directory {path:?}: an entry (type {:#x}) after its end", data[late.unwrap()]);
    };
    check("/", &v.dirs[0].data, v.dirs[0].end);
    let mut stack: Vec<(String, crate::volume::Child)> =
        v.dirs[0].children.values().filter(|c| c.is_dir).map(|c| (String::new(), c.clone())).collect();
    while let Some((parent, ch)) = stack.pop() {
        let d = v.read_child(0, &parent, &ch).unwrap_or_else(|e| panic!("{at}: {e:#}"));
        check(&d.path, &d.data, d.end);
        stack.extend(d.children.values().filter(|c| c.is_dir).map(|c| (d.path.clone(), c.clone())));
    }
}

/// VolumeDirty of the image at `path`, as another system would read it.
fn volume_dirty(path: &Path) -> bool {
    use std::os::unix::fs::FileExt;
    let mut b = [0u8; 2];
    File::open(path).unwrap().read_exact_at(&mut b, 106).unwrap();
    u16::from_le_bytes(b) & crate::volume::FLAG_DIRTY != 0
}

/// An application that fsyncs each file (the commit thread running, the journal header left
/// ACTIVE between commits), steps that make light commits (a new file, an append to the file
/// the previous commit wrote, a removal) and full ones (a move). A crash at
/// every device op, under the three loss models: before any recovery, the volume as another
/// system sees it is either marked dirty or fsck-clean; after flashcp's recovery it shows the
/// tree before or after the interrupted fsync.
#[test]
fn crash_fsync_stream() {
    type FStep = fn(&Fs) -> Result<(), i32>;
    let steps: Vec<FStep> = vec![
        |fs| {
            let d = fs.lookup(ROOT, OsStr::new("docs"))?.ino;
            let f = fs.create(d, OsStr::new("f1.txt"), true, false)?;
            // over 4 MB: not rewritten whole when changed
            fs.write(f.ino, 0, &vec![1u8; (4 << 20) + 1000])?;
            fs.fsync(f.ino)?;
            fs.release(f.ino);
            Ok(())
        },
        // an append to the file the previous commit wrote (its journal still ACTIVE): copy-on-
        // write of the last cluster, the file's new FAT chain in `alloc`
        |fs| {
            let d = fs.lookup(ROOT, OsStr::new("docs"))?.ino;
            let f = fs.lookup(d, OsStr::new("f1.txt"))?.ino;
            fs.open(f, true, false)?;
            fs.write(f, (4 << 20) + 1000, &[2u8; 500])?;
            fs.fsync(f)?;
            fs.release(f);
            Ok(())
        },
        |fs| {
            let d = fs.mkdir(ROOT, OsStr::new("new dir"))?;
            let f = fs.create(d.ino, OsStr::new("a file with a name longer than fifteen.txt"), true, false)?;
            fs.write(f.ino, 0, &[3u8; 9000])?;
            fs.fsync(f.ino)?;
            fs.release(f.ino);
            Ok(())
        },
        |fs| {
            let d = fs.lookup(ROOT, OsStr::new("docs"))?.ino;
            fs.rename(d, OsStr::new("old.txt"), ROOT, OsStr::new("moved.txt"), RenameFlags::default())?;
            fs.fsync(ROOT)?;
            Ok(())
        },
        |fs| {
            let d = fs.lookup(ROOT, OsStr::new("docs"))?.ino;
            fs.unlink(d, OsStr::new("f1.txt"))?;
            fs.fsync(ROOT)?;
            Ok(())
        },
    ];
    let opts = || FsOpts {
        auto_commit: true,
        readahead: false,
        idle: std::time::Duration::from_secs(3600),
        close_linger: std::time::Duration::ZERO,
        idle_header: std::time::Duration::from_secs(3600),
        ..manual()
    };
    let run = |path: &Path, fail_at: Option<u64>| {
        let f = File::options().read(true).write(true).open(path).unwrap();
        let dev = Arc::new(CrashDev::new(f, fail_at));
        let fs = open_with(path, dev.clone(), opts()).unwrap();
        let mut snaps = vec![snapshot(&fs)];
        let mut failed = false;
        for s in &steps {
            let r = s(&fs);
            snaps.push(snapshot(&fs));
            if r.is_err() {
                failed = true;
                break;
            }
        }
        let (light, ops) = (fs.light_commits(), dev.ops());
        if !failed {
            assert!(journal_active(path), "the last fsync's journal header was not left ACTIVE");
        }
        let _ = fs.shutdown();
        (snaps, dev, failed, light, ops)
    };
    for cluster in [4096u32, 512] {
        let img = image(&format!("crash-fsync-{cluster}"), 32, cluster);
        {
            let fs = open(&img.path);
            let d = fs.mkdir(ROOT, OsStr::new("docs")).unwrap();
            let f = fs.create(d.ino, OsStr::new("old.txt"), true, false).unwrap();
            fs.write(f.ino, 0, b"committed before").unwrap();
            fs.shutdown().unwrap();
        }
        let template = img.dir.join("template.img");
        std::fs::copy(&img.path, &template).unwrap();
        let (_, dev, failed, light, total) = run(&img.path, None);
        assert!(!failed);
        // light: the append and the new file (the 4 MB file moves PercentInUse of this small
        // volume, and the move needs VolumeDirty: those three are full commits)
        assert!(light >= 2, "only {light} of the 5 fsyncs made light commits");
        drop(dev);
        for k in 0..total {
            for loss in [Loss::None, Loss::All, Loss::Random(k * 17 + 3)] {
                std::fs::copy(&template, &img.path).unwrap();
                let (snaps, dev, failed, _, ops) = run(&img.path, Some(k));
                if !failed {
                    // a directory's times change or not within the clock's 10 ms: a run can take
                    // a write fewer than the first
                    assert!(ops <= k, "op {k} of {total} did not fail");
                    continue;
                }
                dev.crash(loss);
                drop(dev);
                let at = format!("crash at op {k}/{total} ({loss:?}, cluster {cluster})");
                // another system, before flashcp ever sees the volume again
                foreign_check(&img.path, &at);
                let f = File::options().read(true).write(true).open(&img.path).unwrap();
                let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), manual())
                    .unwrap_or_else(|e| panic!("{at}: recovery failed: {e:#}"));
                let got = snapshot(&fs);
                let n = snaps.len();
                assert!(
                    got == snaps[n - 2] || got == snaps[n - 1],
                    "{at}: neither the tree before nor after fsync {}:\nvs before: {}\nvs after: {}",
                    n - 1,
                    diff(&got, &snaps[n - 2]),
                    diff(&got, &snaps[n - 1])
                );
                fs.shutdown().unwrap();
                fsck(&img.path);
            }
        }
    }
}

/// The journal header a commit leaves ACTIVE: marked IDLE after `idle_header` without
/// commits, at once by prepare-eject; and a mount while it is ACTIVE finds the commit
/// finished (nothing replayed, nothing refused).
#[test]
fn deferred_idle_header() {
    let img = image("idle-header", 32, 4096);
    let opts =
        |delay: std::time::Duration| FsOpts { auto_commit: true, readahead: false, idle_header: delay, ..manual() };
    // the delay far away: nothing below depends on how fast the machine is
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let fs =
        open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts(std::time::Duration::from_secs(3600))).unwrap();
    let a = fs.create(ROOT, OsStr::new("a.txt"), true, false).unwrap();
    fs.write(a.ino, 0, b"first").unwrap();
    fs.fsync(a.ino).unwrap();
    assert!(journal_active(&img.path), "the header was marked IDLE at once");
    // a copy of the volume as it is now (as if yanked): mounts, nothing to replay
    let copy = img.dir.join("yanked.img");
    std::fs::copy(&img.path, &copy).unwrap();
    {
        let f = File::options().read(true).write(true).open(&copy).unwrap();
        let len = f.metadata().unwrap().len();
        let dev = FileDev::new(Arc::new(f.try_clone().unwrap()));
        let (_, rec) = open_core(f, len, &dev, &manual()).unwrap();
        assert_eq!(rec, super::djournal::Recovered::Nothing);
        assert!(!journal_active(&copy), "recovery left the finished commit's header ACTIVE");
        let y = open(&copy);
        assert_eq!(y.read(y.lookup(ROOT, OsStr::new("a.txt")).unwrap().ino, 0, 64).unwrap(), b"first");
        y.shutdown().unwrap();
        fsck(&copy);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(journal_active(&img.path), "marked IDLE before the delay");
    fs.write(a.ino, 5, b", second").unwrap();
    fs.fsync(a.ino).unwrap();
    assert!(journal_active(&img.path));
    fs.prepare_eject().unwrap();
    assert!(!journal_active(&img.path), "prepare-eject left the journal ACTIVE");
    fs.resume();
    fs.release(a.ino);
    fs.shutdown().unwrap();
    // a short delay: marked IDLE by itself, with no commit after
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let fs =
        open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts(std::time::Duration::from_millis(20))).unwrap();
    let b = fs.create(ROOT, OsStr::new("b.txt"), true, false).unwrap();
    fs.write(b.ino, 0, b"b").unwrap();
    fs.fsync(b.ino).unwrap();
    let t = std::time::Instant::now();
    while fs.settled_headers() == 0 {
        assert!(t.elapsed() < std::time::Duration::from_secs(30), "the header was not marked IDLE later");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!journal_active(&img.path));
    fs.release(b.ino);
    fs.shutdown().unwrap();
    fsck(&img.path);
}

/// A volume with 512 B clusters whose only free space is single clusters between used ones,
/// so every new directory of two clusters is FAT-chained across non-adjacent clusters.
fn fragmented(name: &str) -> Img {
    let img = image(name, 8, 512);
    let fs = open(&img.path);
    let fill = fs.mkdir(ROOT, OsStr::new("fill")).unwrap();
    fs.flush().unwrap(); // the journal file
    let free = |fs: &Fs| fs.statfs().bfree * fs.statfs().bsize as u64;
    // leave room for 400 small files and the directory's growth
    let mut k = 0;
    while free(&fs) > (128 << 10) + (400 << 10) {
        let f = fs.create(fill.ino, OsStr::new(&format!("big{k:03}")), true, false).unwrap();
        fs.write(f.ino, 0, &vec![1u8; 128 << 10]).unwrap();
        k += 1;
        if k % 8 == 0 {
            fs.flush().unwrap();
        }
    }
    fs.flush().unwrap();
    for k in 0..400 {
        let f = fs.create(fill.ino, OsStr::new(&format!("s{k:03}")), true, false).unwrap();
        fs.write(f.ino, 0, &[2u8; 500]).unwrap();
    }
    fs.flush().unwrap();
    // the rest in one plug file, then every other small file goes
    // what is left, but a few clusters (the plug's own set may grow the directory)
    let left = free(&fs);
    let plug = fs.create(fill.ino, OsStr::new("plug"), true, false).unwrap();
    fs.write(plug.ino, 0, &vec![3u8; left.saturating_sub(4 << 10) as usize]).unwrap();
    fs.flush().unwrap();
    let left = free(&fs);
    if left > 0 {
        let plug2 = fs.create(fill.ino, OsStr::new("plug2"), true, false).unwrap();
        fs.write(plug2.ino, 0, &vec![4u8; left.saturating_sub(512) as usize]).unwrap();
        fs.flush().unwrap();
    }
    eprintln!("fragmented: {} bytes free before the holes", free(&fs));
    for k in (0..400).step_by(2) {
        fs.unlink(fill.ino, OsStr::new(&format!("s{k:03}"))).unwrap();
    }
    fs.flush().unwrap();
    fs.shutdown().unwrap();
    fsck(&img.path);
    img
}

/// Sets that cross a sector boundary between non-adjacent clusters of a FAT-chained
/// directory: removing one, and padding a directory's end in the sector another `link` write
/// edits while the new set goes into a new non-adjacent cluster. Both take full commits. A
/// crash at every device op under the three loss models: the raw volume is dirty or
/// fsck-clean, the recovered tree the one before or after.
#[test]
fn crash_chained_directories() {
    let img = fragmented("crash-chained");
    let fs = open(&img.path);
    // "chained": six 3-entry sets (576 B): two clusters, which the holes make non-adjacent
    let ch = fs.mkdir(ROOT, OsStr::new("chained")).unwrap();
    let mut inos = Vec::new();
    for n in ["a", "b", "c", "d", "e", "f"] {
        let f = fs.create(ch.ino, OsStr::new(n), true, false).unwrap();
        fs.write(f.ino, 0, n.as_bytes()).unwrap();
        inos.push((n, f.ino));
    }
    // "pad": 3 + 3 + 3 + 5 entries = 448 B, one cluster: a new set there crosses 512
    let pd = fs.mkdir(ROOT, OsStr::new("pad")).unwrap();
    for n in ["x", "y", "w", "a file name of forty characters, really"] {
        let f = fs.create(pd.ino, OsStr::new(n), true, false).unwrap();
        fs.write(f.ino, 0, b"pad").unwrap();
    }
    fs.flush().unwrap();
    let cl = fs.dir_clusters(ch.ino).unwrap();
    assert!(cl.len() == 2 && cl[1] != cl[0] + 1, "\"chained\" is not FAT-chained: {cl:?}");
    let across = inos
        .iter()
        .find(|(_, i)| fs.set_place(*i).is_some_and(|(o, l)| o / 512 != (o + l - 1) / 512))
        .map(|(n, _)| n.to_string())
        .expect("a set across the clusters");
    fs.shutdown().unwrap();
    fsck(&img.path);
    let template = img.dir.join("template.img");
    std::fs::copy(&img.path, &template).unwrap();
    let run = |fail_at: Option<u64>| {
        let f = File::options().read(true).write(true).open(&img.path).unwrap();
        let dev = Arc::new(CrashDev::new(f, fail_at));
        let fs = open_with(&img.path, dev.clone(), manual()).unwrap();
        let mut snaps = vec![snapshot(&fs)];
        let ch = fs.lookup(ROOT, OsStr::new("chained")).unwrap().ino;
        let pd = fs.lookup(ROOT, OsStr::new("pad")).unwrap().ino;
        let steps: Vec<Box<dyn Fn() -> Result<(), i32>>> = vec![
            Box::new(|| {
                fs.unlink(ch, OsStr::new(&across))?;
                fs.fsync(ROOT)
            }),
            Box::new(|| {
                let x = fs.lookup(pd, OsStr::new("x"))?.ino;
                fs.write(x, 3, b"+")?; // its set edited in the sector the padding goes into
                let z = fs.create(pd, OsStr::new("z"), true, false)?;
                fs.write(z.ino, 0, b"zz")?;
                fs.fsync(z.ino)
            }),
        ];
        let mut failed = false;
        for s in &steps {
            let r = s();
            snaps.push(snapshot(&fs));
            if r.is_err() {
                failed = true;
                break;
            }
        }
        let (light, ops) = (fs.light_commits(), dev.ops());
        drop(steps);
        let _ = fs.shutdown();
        (snaps, dev, failed, light, ops)
    };
    let (_, _, failed, light, total) = run(None);
    assert!(!failed);
    assert_eq!(light, 0, "a commit of these was light");
    for k in 0..total {
        // several random sector subsets per op: the states these commits must not leave lose
        // one particular sector of three
        let losses = [Loss::None, Loss::All].into_iter().chain((0..8).map(|r| Loss::Random(k * 131 + r * 17 + 5)));
        for loss in losses {
            std::fs::copy(&template, &img.path).unwrap();
            let (snaps, dev, failed, _, ops) = run(Some(k));
            if !failed {
                assert!(ops <= k, "op {k} of {total} did not fail");
                continue;
            }
            dev.crash(loss);
            drop(dev);
            let at = format!("crash at op {k}/{total} ({loss:?})");
            foreign_check(&img.path, &at);
            let f = File::options().read(true).write(true).open(&img.path).unwrap();
            let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), manual())
                .unwrap_or_else(|e| panic!("{at}: recovery failed: {e:#}"));
            let got = snapshot(&fs);
            let n = snaps.len();
            assert!(got == snaps[n - 2] || got == snaps[n - 1], "{at}: neither the tree before nor after");
            fs.shutdown().unwrap();
            fsck(&img.path);
        }
    }
}

/// Renames inside one directory are edited in place: one over a target whose set is in
/// another sector, or an exchange across sectors, takes a full commit (a crash could leave
/// two sets of one name); a rename alone stays light.
#[test]
fn renames_across_sectors_take_full_commits() {
    let img = image("renames", 32, 4096);
    let fs = open(&img.path);
    let d = fs.mkdir(ROOT, OsStr::new("d")).unwrap();
    for k in 0..16 {
        let f = fs.create(d.ino, OsStr::new(&format!("f{k:02}")), true, false).unwrap();
        fs.write(f.ino, 0, &[k as u8; 10]).unwrap();
    }
    fs.flush().unwrap();
    let sector = |n: &str| fs.set_place(fs.lookup(d.ino, OsStr::new(n)).unwrap().ino).map(|(o, _)| o / 512).unwrap();
    assert_ne!(sector("f01"), sector("f11"));
    assert_ne!(sector("f03"), sector("f13"));
    let light = |f: &dyn Fn()| {
        let before = fs.light_commits();
        f();
        fs.flush().unwrap();
        fs.light_commits() > before
    };
    let rn = |a: &str, b: &str, fl: RenameFlags| fs.rename(d.ino, OsStr::new(a), d.ino, OsStr::new(b), fl).unwrap();
    assert!(!light(&|| rn("f01", "f11", RenameFlags::default())), "rename over a target in another sector");
    let ex = RenameFlags { exchange: true, ..Default::default() };
    assert!(!light(&|| rn("f03", "f13", ex)), "exchange across sectors");
    assert!(light(&|| rn("f04", "g04", RenameFlags::default())), "a rename alone");
    let want = snapshot(&fs);
    fs.shutdown().unwrap();
    fsck(&img.path);
    same!(snapshot(&open(&img.path)), want, "after the renames");
}

/// A yank mid-burst (the last fsync's header still ACTIVE), then another system changes a
/// byte that commit journaled: the next mount abandons a light commit (every crash state of
/// it is a valid volume) and mounts; a full commit is still refused.
#[test]
fn foreign_change_after_a_yank() {
    use std::os::unix::fs::FileExt;
    let img = image("foreign", 32, 4096);
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let opts =
        FsOpts { auto_commit: true, readahead: false, idle_header: std::time::Duration::from_secs(3600), ..manual() };
    let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    let d = fs.mkdir(ROOT, OsStr::new("d")).unwrap();
    let e = fs.mkdir(ROOT, OsStr::new("e")).unwrap();
    fs.flush().unwrap();
    for n in ["a", "b"] {
        let x = fs.create(d.ino, OsStr::new(n), true, false).unwrap();
        fs.write(x.ino, 0, n.as_bytes()).unwrap();
        fs.fsync(x.ino).unwrap();
        fs.release(x.ino);
    }
    // the volume as another system finds it after a yank now, with one journaled byte changed
    let yank = |name: &str, fix: bool, pick: &dyn Fn(&[super::commit::JW]) -> (u64, u8)| -> (PathBuf, bool) {
        let copy = img.dir.join(name);
        std::fs::copy(&img.path, &copy).unwrap();
        let rw = File::options().read(true).write(true).open(&copy).unwrap();
        let len = rw.metadata().unwrap().len();
        let mut v = crate::volume::Vol::open_boot(rw.try_clone().unwrap(), len, false, false).unwrap();
        let p = super::djournal::locate_raw(&mut v, crate::volume::MAX_DIR_BYTES).unwrap().unwrap();
        let (light, writes) = super::djournal::pending(&v, &p).expect("an ACTIVE journal");
        let (off, mask) = pick(&writes);
        // the byte changed, and the checksum of the set it is in made right again
        let at = off / 32 * 32;
        let w = writes.iter().find(|w| w.off <= off && off < w.off + w.post.len() as u64).unwrap();
        let k = (at - w.off) as usize;
        let first = (0..=k / 32).rev().map(|e| e * 32).find(|&e| w.post[e] == 0x85).unwrap_or(k);
        let n = (w.post[first + 1] as usize + 1) * 32;
        let base = w.off + first as u64;
        let mut set = vec![0u8; n];
        rw.read_exact_at(&mut set, base).unwrap();
        set[(off - base) as usize] ^= mask;
        if fix && set[0] == 0x85 {
            let c = crate::exfat::checksum16(&set, true);
            set[2..4].copy_from_slice(&c.to_le_bytes());
        }
        rw.write_all_at(&set, base).unwrap();
        (copy, light)
    };
    fn new_set(w: &super::commit::JW) -> Option<usize> {
        (0..w.post.len() / 32).map(|e| e * 32).find(|&e| w.post[e] == 0x85 && w.pre[e] & 0x80 == 0)
    }
    // light (a new file): its set retimed by the other system (checksum recomputed)
    let new_set_ts = |ws: &[super::commit::JW]| {
        let w = ws.iter().find(|w| w.phase == super::commit::P_LINK && new_set(w).is_some()).expect("a new set");
        (w.off + new_set(w).unwrap() as u64 + 12, 0x01) // LastModifiedTimestamp
    };
    // the same byte changed without its checksum (what a torn or broken write leaves): the
    // volume is not valid where the commit wrote, so it is refused, not abandoned
    let (bad, _) = yank("garbled.img", false, &new_set_ts);
    let f = File::options().read(true).write(true).open(&bad).unwrap();
    let len = f.metadata().unwrap().len();
    let err = open_core(f.try_clone().unwrap(), len, &FileDev::new(Arc::new(f)), &manual()).err().expect("refused");
    assert!(err.downcast_ref::<super::djournal::Refused>().is_some(), "{err:#}");
    // the sector of the new set read back as zeros (a torn write): the parser would take it
    // for the end of the directory and hide the files earlier fsyncs made durable: refused
    {
        let zero = img.dir.join("zeroed.img");
        std::fs::copy(&img.path, &zero).unwrap();
        let rw = File::options().read(true).write(true).open(&zero).unwrap();
        let len = rw.metadata().unwrap().len();
        let mut v = crate::volume::Vol::open_boot(rw.try_clone().unwrap(), len, false, false).unwrap();
        let p = super::djournal::locate_raw(&mut v, crate::volume::MAX_DIR_BYTES).unwrap().unwrap();
        let (light, writes) = super::djournal::pending(&v, &p).expect("an ACTIVE journal");
        assert!(light);
        let w = writes.iter().find(|w| w.phase == super::commit::P_LINK && new_set(w).is_some()).unwrap();
        rw.write_all_at(&vec![0u8; w.post.len()], w.off).unwrap();
        let len = rw.metadata().unwrap().len();
        let err =
            open_core(rw.try_clone().unwrap(), len, &FileDev::new(Arc::new(rw)), &manual()).err().expect("refused");
        assert!(err.downcast_ref::<super::djournal::Refused>().is_some(), "{err:#}");
    }
    // a yank after `alloc`, before `link` reached the device (the new set's sector still as
    // before the commit), then the other system retimes `a`, whose set is in that sector: the
    // slot being filled reads 0x00 as it did before, a valid state: abandoned, not refused
    {
        let unl = img.dir.join("unlinked.img");
        std::fs::copy(&img.path, &unl).unwrap();
        let rw = File::options().read(true).write(true).open(&unl).unwrap();
        let len = rw.metadata().unwrap().len();
        let mut v = crate::volume::Vol::open_boot(rw.try_clone().unwrap(), len, false, false).unwrap();
        let p = super::djournal::locate_raw(&mut v, crate::volume::MAX_DIR_BYTES).unwrap().unwrap();
        let (light, writes) = super::djournal::pending(&v, &p).expect("an ACTIVE journal");
        assert!(light);
        let w = writes.iter().find(|w| w.phase == super::commit::P_LINK && new_set(w).is_some()).unwrap();
        let mut sec = w.pre.clone();
        let e = (0..sec.len() / 32).map(|e| e * 32).find(|&e| sec[e] == 0x85).expect("a's set in the sector");
        let n = (sec[e + 1] as usize + 1) * 32;
        sec[e + 12] ^= 0x01;
        let c = crate::exfat::checksum16(&sec[e..e + n], true);
        sec[e + 2..e + 4].copy_from_slice(&c.to_le_bytes());
        rw.write_all_at(&sec, w.off).unwrap();
        let (_, rec) = open_core(rw.try_clone().unwrap(), len, &FileDev::new(Arc::new(rw)), &manual()).unwrap();
        assert_eq!(rec, super::djournal::Recovered::Abandoned);
        let m = open(&unl);
        let dd = m.lookup(ROOT, OsStr::new("d")).unwrap().ino;
        assert_eq!(m.read(m.lookup(dd, OsStr::new("a")).unwrap().ino, 0, 8).unwrap(), b"a");
        assert_eq!(m.lookup(dd, OsStr::new("b")).err(), Some(libc::ENOENT));
        m.shutdown().unwrap();
        fsck(&unl);
    }
    let (copy, light) = yank("light.img", true, &new_set_ts);
    assert!(light);
    let f = File::options().read(true).write(true).open(&copy).unwrap();
    let len = f.metadata().unwrap().len();
    let (_, rec) = open_core(f.try_clone().unwrap(), len, &FileDev::new(Arc::new(f)), &manual()).unwrap();
    assert_eq!(rec, super::djournal::Recovered::Abandoned);
    let m = open(&copy);
    let dd = m.lookup(ROOT, OsStr::new("d")).unwrap().ino;
    assert_eq!(m.read(m.lookup(dd, OsStr::new("b")).unwrap().ino, 0, 8).unwrap(), b"b");
    m.shutdown().unwrap();
    fsck(&copy);
    // full (a move to another directory): refused, nothing written
    fs.rename(d.ino, OsStr::new("a"), e.ino, OsStr::new("a"), RenameFlags::default()).unwrap();
    fs.fsync(e.ino).unwrap();
    let (copy, light) = yank("full.img", true, &|ws| {
        let w = ws.iter().find(|w| w.phase == super::commit::P_LINK && new_set(w).is_some()).expect("the moved set");
        (w.off + new_set(w).unwrap() as u64 + 12, 0x01) // its LastModifiedTimestamp
    });
    assert!(!light);
    let f = File::options().read(true).write(true).open(&copy).unwrap();
    let len = f.metadata().unwrap().len();
    let err = open_core(f.try_clone().unwrap(), len, &FileDev::new(Arc::new(f)), &manual()).err().expect("refused");
    assert!(err.downcast_ref::<super::djournal::Refused>().is_some(), "{err:#}");
    fs.shutdown().unwrap();
}

/// The deferred IDLE header and eject: fsyncs racing prepare-eject (their requests coalesce
/// with eject's) never leave it ACTIVE, nor a directory's times pending, once eject reports
/// safe; a failed IDLE write stays pending (not fatal, reported once), is retried, and makes
/// eject fail.
#[test]
fn settle_header_on_eject_and_errors() {
    let img = image("settle", 32, 4096);
    let opts = || FsOpts {
        auto_commit: true,
        readahead: false,
        idle_header: std::time::Duration::from_secs(3600),
        ..manual()
    };
    // 1: an fsync storm while ejecting
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts()).unwrap();
    let dir = fs.mkdir(ROOT, OsStr::new("storm")).unwrap().ino;
    fs.flush().unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (fs2, stop2) = (fs.clone(), stop.clone());
    let storm = std::thread::spawn(move || {
        let mut k = 0;
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            // files in a directory: every fsync's commit leaves its times
            let Ok(x) = fs2.create(dir, OsStr::new(&format!("s{k}")), true, false) else { break };
            if fs2.write(x.ino, 0, b"storm").is_ok() {
                let _ = fs2.fsync(x.ino);
            }
            fs2.release(x.ino);
            k += 1;
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    fs.prepare_eject().unwrap();
    assert!(!journal_active(&img.path), "prepare-eject reported safe with the journal ACTIVE");
    let probe = open(&img.path);
    let mt = probe.getattr(probe.lookup(ROOT, OsStr::new("storm")).unwrap().ino).unwrap().mtime;
    assert_eq!(mt, fs.getattr(dir).unwrap().mtime, "the directory's times are not on the volume");
    probe.shutdown().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    storm.join().unwrap();
    fs.resume();
    fs.shutdown().unwrap();
    assert!(!journal_active(&img.path));
    fsck(&img.path);
    // 2: the IDLE write fails
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let dev = Arc::new(CrashDev::new(f, None));
    let fs = open_with(&img.path, dev.clone(), FsOpts { idle_header: std::time::Duration::from_millis(50), ..opts() })
        .unwrap();
    let errors = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let e2 = errors.clone();
    fs.on_note(move |n| {
        if matches!(n, super::fs::Note::Error { fatal: false, .. }) {
            e2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let x = fs.create(ROOT, OsStr::new("x"), true, false).unwrap();
    fs.write(x.ino, 0, b"x").unwrap();
    fs.fsync(x.ino).unwrap();
    {
        let mut st = dev.st.lock().unwrap();
        st.fail_at = Some(st.ops); // from now on every write fails
    }
    // attempts after 50 ms, then 1 s, 2 s... later (backing off): one report
    let t = std::time::Instant::now();
    while errors.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(t.elapsed() < std::time::Duration::from_secs(30), "the failed IDLE mark was not reported");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(journal_active(&img.path), "the IDLE mark cannot have been written");
    assert_ne!(fs.status().state, "read_only", "a failed IDLE mark is not fatal");
    assert_eq!(errors.load(std::sync::atomic::Ordering::SeqCst), 1, "a failing IDLE mark was reported more than once");
    {
        let mut st = dev.st.lock().unwrap();
        (st.fail_at, st.failed) = (None, false);
    }
    let t = std::time::Instant::now();
    while journal_active(&img.path) {
        assert!(t.elapsed() < std::time::Duration::from_secs(60), "the IDLE mark was not retried");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // eject while it cannot be written: not safe
    fs.write(x.ino, 1, b"y").unwrap();
    fs.fsync(x.ino).unwrap();
    {
        let mut st = dev.st.lock().unwrap();
        st.fail_at = Some(st.ops);
    }
    // (the 50 ms retry may get there first: either way the header cannot be written)
    assert!(fs.prepare_eject().is_err(), "prepare-eject reported safe without the IDLE mark");
    assert!(journal_active(&img.path));
    // the commit thread's own attempt (for eject's request) is not fatal either
    assert_ne!(fs.status().state, "read_only");
    fs.release(x.ino);
    let _ = fs.shutdown();
}

/// The light-commit rule on hand-built `link` sectors (see `commit::needs_no_dirty`).
#[test]
fn light_commit_rule() {
    use super::commit::{JW, P_LINK, needs_no_dirty};
    // a 3-entry file set (primary, stream extension, one name entry) at entry `e` of `sec`
    let put = |sec: &mut [u8], e: usize, name: u8| {
        sec[e * 32] = 0x85;
        sec[e * 32 + 1] = 2;
        sec[e * 32 + 32] = 0xC0;
        sec[e * 32 + 32 + 3] = 1; // NameLength
        sec[e * 32 + 32 + 4] = name; // NameHash
        sec[e * 32 + 64] = 0xC1;
        sec[e * 32 + 64 + 2] = name;
    };
    let del = |sec: &mut [u8], e: usize, n: usize| {
        for k in e..e + n {
            sec[k * 32] &= 0x7F;
        }
    };
    let jw = |sector: u64, pre: &[u8], post: &[u8]| JW {
        phase: P_LINK,
        off: sector * 512,
        pre: pre.to_vec(),
        post: post.to_vec(),
    };
    let fat = (512, 4096); // the FAT region: sectors 1-7
    let blank = vec![0u8; 512];
    // A: sector 100 holds a set "a" at 0 and "t" at 3; sector 200 holds "b" at 0
    let mut a = blank.clone();
    put(&mut a, 0, b'a');
    put(&mut a, 3, b't');
    let mut b = blank.clone();
    put(&mut b, 0, b'b');
    // a new set into unused slots, alone: light
    let mut a2 = a.clone();
    put(&mut a2, 6, b'n');
    assert!(needs_no_dirty(&[jw(100, &a, &a2)], fat.0, fat.1));
    // ... with an in-place times edit in another sector: light
    let mut b2 = b.clone();
    b2[8] = 0x55;
    b2[2] = 0x66;
    assert!(needs_no_dirty(&[jw(100, &a, &a2), jw(200, &b, &b2)], fat.0, fat.1));
    // a whole set removed: light
    let mut a3 = a.clone();
    del(&mut a3, 3, 3);
    assert!(needs_no_dirty(&[jw(100, &a, &a3)], fat.0, fat.1));
    // rename "b" -> "t" in place (sector 200) over the target "t" removed in sector 100: full
    let mut b3 = b.clone();
    put(&mut b3, 0, b't');
    assert!(!needs_no_dirty(&[jw(100, &a, &a3), jw(200, &b, &b3)], fat.0, fat.1), "rename over a target elsewhere");
    // ... the same inside one sector: light
    let mut a4 = a.clone();
    put(&mut a4, 0, b't');
    del(&mut a4, 3, 3);
    assert!(needs_no_dirty(&[jw(100, &a, &a4)], fat.0, fat.1), "rename over a target in its sector");
    // an exchange of names across two sectors: full
    let mut a5 = a.clone();
    put(&mut a5, 0, b'b');
    let mut b5 = b.clone();
    put(&mut b5, 0, b'a');
    assert!(!needs_no_dirty(&[jw(100, &a, &a5), jw(200, &b, &b5)], fat.0, fat.1), "exchange across sectors");
    // a set across two sectors (primary at entry 15 of sector 300, secondaries at 0-1 of 400,
    // non-adjacent: a FAT-chained directory) removed: full
    let mut c = blank.clone();
    c[15 * 32] = 0x85;
    c[15 * 32 + 1] = 2;
    let mut d = blank.clone();
    d[0] = 0xC0;
    d[32] = 0xC1;
    let mut c2 = c.clone();
    del(&mut c2, 15, 1);
    let mut d2 = d.clone();
    del(&mut d2, 0, 2);
    assert!(!needs_no_dirty(&[jw(300, &c, &c2), jw(400, &d, &d2)], fat.0, fat.1), "a removed set across sectors");
    assert!(!needs_no_dirty(&[jw(400, &d, &d2)], fat.0, fat.1), "secondaries removed without their primary");
    // two sectors with new sets: full; a FAT write in link: full
    let mut b6 = b.clone();
    put(&mut b6, 3, b'm');
    assert!(!needs_no_dirty(&[jw(100, &a, &a2), jw(200, &b, &b6)], fat.0, fat.1));
    assert!(!needs_no_dirty(&[JW { phase: P_LINK, off: 1024, pre: vec![0; 4], post: vec![1; 4] }], fat.0, fat.1));
}

/// The commit's word-at-a-time free-extent scan finds exactly what `add`'s byte scan finds.
#[test]
fn free_extents_word_scan_matches() {
    let mut rng = crate::util::Rng(31);
    for case in 0..400u64 {
        let cc = 1 + rng.next() % 3000;
        let mut bm = vec![0u8; (cc.div_ceil(8) as usize).div_ceil(8) * 8 + 8];
        // runs of free and used clusters, long and short, so whole words and partial ones mix
        let mut c = 0u64;
        while c < cc {
            let len = 1 + rng.next() % if case % 3 == 0 { 7 } else { 300 };
            let used = rng.next().is_multiple_of(2);
            for k in c..(c + len).min(cc) {
                if used {
                    bm[(k / 8) as usize] |= 1 << (k % 8);
                }
            }
            c += len;
        }
        // garbage past the cluster count must be ignored
        let last = cc.div_ceil(8) as usize;
        for b in &mut bm[last..] {
            *b = rng.next() as u8;
        }
        if !cc.is_multiple_of(8) {
            bm[last - 1] |= 0xFFu8 << (cc % 8);
        }
        assert_eq!(super::commit::free_extents(&bm, cc), crate::add::free_extents(&bm, cc), "case {case}, cc {cc}");
    }
}

/// An fsync whose commit must be split (a small journal file): the first part is the
/// directory whose only change is its times. It is written then (a split part never defers),
/// and the fsync returns instead of planning an empty part forever.
#[test]
fn split_fsync_commit_never_defers() {
    let img = image("split-fsync", 32, 4096);
    let fs = open(&img.path);
    super::djournal::TEST_JOURNAL_BYTES.with(|c| c.set(Some(4096)));
    fs.mkdir(ROOT, OsStr::new("d")).unwrap();
    fs.commit_now("t").unwrap(); // creates the small journal
    super::djournal::TEST_JOURNAL_BYTES.with(|c| c.set(None));
    fs.shutdown().unwrap();
    // two new files of many data segments each (one byte every 64 KB): one fits the journal,
    // two do not, so the plan is cut to one inode, the shallowest: the directory
    let fs = open(&img.path);
    let d = fs.lookup(ROOT, OsStr::new("d")).unwrap().ino;
    let mut last = 0;
    for n in ["one", "two"] {
        let f = fs.create(d, OsStr::new(n), true, false).unwrap();
        for k in 0..32u64 {
            fs.write(f.ino, k << 16, b"x").unwrap();
        }
        last = f.ino;
    }
    let before = fs.commits();
    let (tx, rx) = std::sync::mpsc::channel();
    let f2 = fs.clone();
    std::thread::spawn(move || tx.send(f2.fsync(last)).unwrap());
    let r = rx.recv_timeout(std::time::Duration::from_secs(60)).expect("fsync hung (split livelock)");
    assert_eq!(r, Ok(()));
    assert!(fs.commits() > before + 1, "expected a split commit");
    let want = snapshot(&fs);
    fs.shutdown().unwrap();
    fsck(&img.path);
    same!(snapshot(&open(&img.path)), want, "after the split fsync");
}

/// A device that holds its next write, once armed, until released (tests).
struct PauseDev {
    inner: FileDev,
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

impl Dev for PauseDev {
    fn read_at(&self, buf: &mut [u8], off: u64) -> std::io::Result<()> {
        self.inner.read_at(buf, off)
    }
    fn write_at(&self, buf: &[u8], off: u64) -> std::io::Result<()> {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered.wait();
            self.release.wait();
        }
        self.inner.write_at(buf, off)
    }
    fn sync(&self) -> std::io::Result<()> {
        self.inner.sync()
    }
}

/// utimensat on a directory while a commit that writes the directory runs: the directory
/// stays dirty with its explicit times, and the next fsync's commit writes them (explicit
/// times never wait).
#[test]
fn explicit_times_during_a_commit_do_not_wait() {
    let img = image("times-during", 32, 4096);
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let dev = Arc::new(PauseDev {
        inner: FileDev::new(Arc::new(f)),
        armed: false.into(),
        entered: std::sync::Barrier::new(2),
        release: std::sync::Barrier::new(2),
    });
    let fs = open_with(&img.path, dev.clone(), manual()).unwrap();
    let sub = fs.mkdir(ROOT, OsStr::new("sub")).unwrap().ino;
    fs.flush().unwrap();
    let a = fs.create(sub, OsStr::new("a"), true, false).unwrap();
    fs.write(a.ino, 0, b"a").unwrap();
    // a flush (writes the directory's new times) held at its first device write
    dev.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let f2 = fs.clone();
    let flusher = std::thread::spawn(move || f2.flush());
    dev.entered.wait();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    fs.setattr(sub, &SetAttr { mtime: Some(when), ..Default::default() }).unwrap();
    dev.release.wait();
    flusher.join().unwrap().unwrap();
    // an fsync of another file in it: the explicit times go with it
    let b = fs.create(sub, OsStr::new("b"), true, false).unwrap();
    fs.write(b.ino, 0, b"b").unwrap();
    fs.fsync(b.ino).unwrap();
    assert_eq!(fs.status().dirty_files, 0, "the explicitly set times waited");
    // (creating b changed them again: the volume has what the driver has)
    let probe = open(&img.path);
    assert_eq!(
        probe.getattr(probe.lookup(ROOT, OsStr::new("sub")).unwrap().ino).unwrap().mtime,
        fs.getattr(sub).unwrap().mtime
    );
    probe.shutdown().unwrap();
    fs.shutdown().unwrap();
}

/// Times deferred by fsyncs: a directory written after it waited `idle`, and explicitly set
/// times never deferred.
#[test]
fn deferred_directory_times_are_bounded() {
    let img = image("defer-bound", 32, 4096);
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let opts = FsOpts { idle: std::time::Duration::from_millis(40), ..manual() };
    let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    let sub = fs.mkdir(ROOT, OsStr::new("sub")).unwrap().ino;
    fs.flush().unwrap();
    let new_file = |n: &str| {
        let x = fs.create(sub, OsStr::new(n), true, false).unwrap();
        fs.write(x.ino, 0, b"x").unwrap();
        fs.fsync(x.ino).unwrap();
        fs.release(x.ino);
    };
    // (the first deferral of a directory always waits; past `idle`, at least 80 ms later
    // here whatever the machine's speed, the next fsync's commit writes it. The gap first:
    // past the 10 ms resolution of exFAT's times, the directory's times do change)
    std::thread::sleep(std::time::Duration::from_millis(30));
    new_file("a");
    assert_eq!(fs.status().dirty_files, 1, "the directory's times did not wait");
    std::thread::sleep(std::time::Duration::from_millis(80));
    new_file("c");
    assert_eq!(fs.status().dirty_files, 0, "the directory waited longer than idle");
    // explicit times on the directory, then fsync of it: written now
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs.setattr(sub, &SetAttr { mtime: Some(when), ..Default::default() }).unwrap();
    fs.fsync(sub).unwrap();
    assert_eq!(fs.status().dirty_files, 0, "explicitly set times waited");
    let probe = open(&img.path);
    assert_eq!(probe.getattr(probe.lookup(ROOT, OsStr::new("sub")).unwrap().ino).unwrap().mtime, when);
    probe.shutdown().unwrap();
    fs.shutdown().unwrap();
}

/// A batch whose journal would not fit the journal file is committed in parts, each one
/// complete; the tree after the last is exact.
#[test]
fn oversized_batches_split_into_commits() {
    let img = image("split", 64, 4096);
    // The hook (this thread's) is read when a journal file is created: set it only around
    // this volume's first commit, made on this thread.
    let fs = open(&img.path);
    super::djournal::TEST_JOURNAL_BYTES.with(|c| c.set(Some(65536)));
    let d = fs.mkdir(ROOT, OsStr::new("many")).unwrap();
    fs.commit_now("t").unwrap(); // creates the small journal
    super::djournal::TEST_JOURNAL_BYTES.with(|c| c.set(None));
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
    super::djournal::TEST_JOURNAL_BYTES.with(|c| c.set(Some(12288)));
    let nd = fs.mkdir(ROOT, &n("newdir")).unwrap().ino;
    fs.commit_now("t").unwrap();
    super::djournal::TEST_JOURNAL_BYTES.with(|c| c.set(None));
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
    // 2b: prepare-eject vs a write already admitted (held at the gate by a barrier): eject
    //     waits for it, so "safe" includes it; afterwards writes and write-opens get EROFS
    let img3 = image("regress-eject", 32, 4096);
    let fs = open(&img3.path);
    let held = fs.create(ROOT, &n("held"), true, false).unwrap();
    fs.flush().unwrap();
    let (inside, release) = (Arc::new(std::sync::Barrier::new(2)), Arc::new(std::sync::Barrier::new(2)));
    let (i2, r2) = (inside.clone(), release.clone());
    fs.set_hook(Some(Arc::new(move || {
        if std::thread::current().name() == Some("held-writer") {
            i2.wait();
            r2.wait();
        }
    })));
    let fw = fs.clone();
    let writer = std::thread::Builder::new()
        .name("held-writer".into())
        .spawn(move || fw.write(held.ino, 0, b"acknowledged"))
        .unwrap();
    inside.wait(); // the write is admitted and waiting
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (fe, d2) = (fs.clone(), done.clone());
    let ejector = std::thread::spawn(move || {
        let r = fe.prepare_eject();
        d2.store(true, std::sync::atomic::Ordering::SeqCst);
        r
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "prepare-eject finished while a write was in flight");
    release.wait();
    assert_eq!(writer.join().unwrap(), Ok(12));
    ejector.join().unwrap().unwrap();
    fs.set_hook(None);
    assert_eq!(fs.write(held.ino, 0, b"late"), Err(libc::EROFS));
    assert_eq!(fs.open(held.ino, true, false), Err(libc::EROFS));
    assert_eq!(fs.open(held.ino, false, false), Ok(()));
    // "safe" meant durable: a fresh mount of the device as it is now has the write
    let again = open(&img3.path);
    assert_eq!(again.read(again.lookup(ROOT, &n("held")).unwrap().ino, 0, 64).unwrap(), b"acknowledged");
    again.shutdown().unwrap();
    fs.resume();
    fs.shutdown().unwrap();
    // 3: flush returns while another thread keeps writing
    let f = File::options().read(true).write(true).open(&img.path).unwrap();
    let opts = FsOpts { auto_commit: true, idle: std::time::Duration::from_millis(50), readahead: false, ..manual() };
    let fs = open_with(&img.path, Arc::new(FileDev::new(Arc::new(f))), opts).unwrap();
    // a metadata-only change (utimens, chmod) gets the idle commit too
    let tg = fs.lookup(ROOT, &n("target")).unwrap().ino;
    let when2 = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_400_000_000);
    fs.setattr(tg, &SetAttr { mtime: Some(when2), mode: Some(0o444), ..Default::default() }).unwrap();
    let t = std::time::Instant::now();
    while fs.status().dirty_files > 0 {
        assert!(t.elapsed() < std::time::Duration::from_secs(30), "utimens/chmod stayed pending: {:?}", fs.status());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let probe = open(&img.path);
    let a = probe.getattr(probe.lookup(ROOT, &n("target")).unwrap().ino).unwrap();
    assert_eq!((a.mtime, a.read_only), (when2, true));
    probe.shutdown().unwrap();
    fs.setattr(tg, &SetAttr { mode: Some(0o644), ..Default::default() }).unwrap();
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
