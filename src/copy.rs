//! Loose-file copiers.
//!
//! `naive_copy` behaves like a file manager: one file at a time, open → copy → close.
//!
//! `smart_copy` is the pipelined version:
//!   1. scan the whole tree first and create every directory in one pass,
//!   2. N reader threads pull files from the (fast) source into memory in parallel,
//!   3. W writer threads each own a contiguous, directory-ordered slice of the file list
//!      and write it strictly in order, so the filesystem allocates clusters back to back
//!      and directory metadata is touched in batches,
//!   4. each file is preallocated to its final size, never fsync'd individually,
//!   5. one syncfs at the very end flushes everything to the device.
//!
//! Memory is bounded by a byte budget. Budget is granted and indices are claimed under one
//! lock, so every writer's next file is always either in flight or claimable: no deadlock.

use crate::util::{Entry, fadvise_sequential, preallocate, scan, syncfs};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

pub struct Stats {
    pub files: usize,
    pub bytes: u64,
    pub secs: f64,
}

pub fn naive_copy(src: &Path, dst: &Path) -> Result<Stats> {
    let t = Instant::now();
    let (_, files) = scan(src)?;
    let mut bytes = 0;
    for e in &files {
        let to = dst.join(&e.rel);
        if let Some(p) = to.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(src.join(&e.rel), &to).with_context(|| format!("copy {}", e.rel.display()))?;
        bytes += e.size;
    }
    syncfs(dst)?;
    Ok(Stats { files: files.len(), bytes, secs: t.elapsed().as_secs_f64() })
}

#[derive(Clone, Copy)]
pub struct SmartOpts {
    pub readers: usize,
    pub writers: usize,
    pub budget: u64,
    /// Files above this size are streamed by the writer instead of buffered by a reader.
    pub stream_threshold: u64,
    pub preallocate: bool,
}

impl Default for SmartOpts {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        SmartOpts {
            readers: cpus.clamp(2, 16),
            writers: 1,
            budget: 256 << 20,
            stream_threshold: 16 << 20,
            preallocate: true,
        }
    }
}

enum Payload {
    Data(Vec<u8>),
    Stream,
}

struct Sched {
    budget_left: u64,
    /// Next unclaimed index per writer, and each writer's end index.
    next: Vec<usize>,
    end: Vec<usize>,
}

pub fn smart_copy(src: &Path, dst: &Path, o: SmartOpts) -> Result<Stats> {
    let t = Instant::now();
    let (dirs, files) = scan(src)?;
    fs::create_dir_all(dst)?;
    for d in &dirs {
        fs::create_dir_all(dst.join(d))?;
    }
    let total: u64 = files.iter().map(|e| e.size).sum();
    let files = Arc::new(files);

    // Split the ordered list into W contiguous slices of roughly equal bytes.
    let w = o.writers.max(1).min(files.len().max(1));
    let mut bounds = vec![0usize];
    let mut acc = 0u64;
    for (i, e) in files.iter().enumerate() {
        acc += e.size.max(4096);
        if bounds.len() < w && acc >= total.max(1) / w as u64 * bounds.len() as u64 {
            bounds.push(i + 1);
        }
    }
    while bounds.len() < w {
        bounds.push(files.len());
    }
    bounds.push(files.len());
    let starts: Vec<usize> = bounds[..w].to_vec();
    let ends: Vec<usize> = bounds[1..=w].to_vec();

    let sched = Arc::new((
        Mutex::new(Sched { budget_left: o.budget, next: starts.clone(), end: ends.clone() }),
        Condvar::new(),
    ));
    let (txs, rxs): (Vec<Sender<(usize, Payload)>>, Vec<Receiver<(usize, Payload)>>) =
        (0..w).map(|_| bounded(4096)).unzip();
    let error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));

    std::thread::scope(|s| {
        for _ in 0..o.readers.max(1) {
            let (sched, files, txs, error, starts) = (sched.clone(), files.clone(), txs.clone(), error.clone(), starts.clone());
            s.spawn(move || {
                loop {
                    // Claim the next file of the least-advanced writer, with budget, atomically.
                    let (wi, idx, cost) = {
                        let (m, cv) = &*sched;
                        let mut g = m.lock().unwrap();
                        loop {
                            let pick = (0..g.next.len())
                                .filter(|&k| g.next[k] < g.end[k])
                                .min_by_key(|&k| g.next[k] - starts[k]);
                            let Some(k) = pick else { return };
                            let e = &files[g.next[k]];
                            let cost = if e.size > o.stream_threshold { 0 } else { e.size.min(o.budget) };
                            if cost <= g.budget_left {
                                g.budget_left -= cost;
                                let idx = g.next[k];
                                g.next[k] += 1;
                                break (k, idx, cost);
                            }
                            g = cv.wait(g).unwrap();
                        }
                    };
                    let _ = cost;
                    let e = &files[idx];
                    let payload = if e.size > o.stream_threshold {
                        Payload::Stream
                    } else {
                        let mut v = Vec::with_capacity(e.size as usize);
                        match File::open(src.join(&e.rel)).and_then(|mut f| f.read_to_end(&mut v)) {
                            Ok(_) => Payload::Data(v),
                            Err(err) => {
                                *error.lock().unwrap() = Some(anyhow::anyhow!("read {}: {err}", e.rel.display()));
                                Payload::Data(Vec::new())
                            }
                        }
                    };
                    if txs[wi].send((idx, payload)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(txs);

        for (wi, rx) in rxs.into_iter().enumerate() {
            let (sched, files, error) = (sched.clone(), files.clone(), error.clone());
            let (start, end) = (starts[wi], ends[wi]);
            s.spawn(move || {
                let mut pending: BTreeMap<usize, Payload> = BTreeMap::new();
                let mut next = start;
                let mut big = vec![0u8; 8 << 20];
                while next < end {
                    while !pending.contains_key(&next) {
                        match rx.recv() {
                            Ok((i, p)) => {
                                pending.insert(i, p);
                            }
                            Err(_) => return,
                        }
                    }
                    let p = pending.remove(&next).unwrap();
                    let e: &Entry = &files[next];
                    let r = write_one(src, dst, e, p, o.preallocate, &mut big);
                    if let Err(err) = r {
                        *error.lock().unwrap() = Some(err);
                    }
                    if e.size <= o.stream_threshold {
                        let (m, cv) = &*sched;
                        m.lock().unwrap().budget_left += e.size.min(o.budget);
                        cv.notify_all();
                    }
                    next += 1;
                }
            });
        }
    });

    if let Some(e) = error.lock().unwrap().take() {
        return Err(e);
    }
    syncfs(dst)?;
    Ok(Stats { files: files.len(), bytes: total, secs: t.elapsed().as_secs_f64() })
}

fn write_one(src: &Path, dst: &Path, e: &Entry, p: Payload, prealloc: bool, big: &mut [u8]) -> Result<()> {
    let to: PathBuf = dst.join(&e.rel);
    let mut out = File::create(&to).with_context(|| format!("create {}", to.display()))?;
    if prealloc {
        preallocate(&out, e.size);
    }
    match p {
        Payload::Data(v) => out.write_all(&v)?,
        Payload::Stream => {
            let mut inp = File::open(src.join(&e.rel))?;
            fadvise_sequential(&inp);
            loop {
                let n = inp.read(big)?;
                if n == 0 {
                    break;
                }
                out.write_all(&big[..n])?;
            }
        }
    }
    Ok(())
}

/// Byte-for-byte comparison of every file in `src` against `dst`.
pub fn verify(src: &Path, dst: &Path) -> Result<()> {
    let (_, files) = scan(src)?;
    let (_, dfiles) = scan(dst)?;
    anyhow::ensure!(files.len() == dfiles.len(), "file count mismatch: {} vs {}", files.len(), dfiles.len());
    let mut a = vec![0u8; 1 << 20];
    let mut b = vec![0u8; 1 << 20];
    for e in &files {
        let mut fa = File::open(src.join(&e.rel))?;
        let mut fb = File::open(dst.join(&e.rel)).with_context(|| format!("missing {}", e.rel.display()))?;
        anyhow::ensure!(fb.metadata()?.len() == e.size, "size mismatch {}", e.rel.display());
        loop {
            let n = fa.read(&mut a)?;
            if n == 0 {
                break;
            }
            fb.read_exact(&mut b[..n])?;
            anyhow::ensure!(a[..n] == b[..n], "content mismatch {}", e.rel.display());
        }
    }
    Ok(())
}
