//! Content manifests (size + hash of every file) of an exFAT volume and of a local tree, used
//! by `optimize` to prove that a copy is complete before and after rebuilding the volume.
//!
//! The hash of a file is order-independent: the wrapping sum, over every 512-byte block `k` of
//! its logical content (bytes at or past ValidDataLength read as zeros, the last block short),
//! of `xxh3_128_with_seed(block, k)`. Because a block carries its index in the seed, swapped or
//! shifted blocks still change the sum, but the blocks may be hashed in any order. That lets
//! `pull` hash every extent as it copies it (in whatever order its reads complete), so
//! `optimize` reads the original data only once, and lets [`hash_volume`] read in PHYSICAL
//! order with pull's own planner and per-device strategy ([`crate::pull::plan_reads`],
//! [`crate::pull::read_strategy`]: aligned reads of up to 8 MB, reading through only the gaps
//! that cost less than a request, as many in flight as the transport likes). The local side
//! reads each file front to back. 512 bytes divides every exFAT cluster size, so an extent
//! always starts on a block boundary and both volumes (old and new cluster size) hash
//! identically.
//!
//! The volume walk ([`walk`]) applies the same untrusted-volume checks as `pull` (strict
//! UTF-16 names, single safe components, cross-link / loop / cycle detection through one
//! visited-cluster set, DataLength bounded by the heap, entry and depth limits).

use crate::exfat::set_direct;
use crate::pull::{Ext, MAX_DEPTH, MAX_ENTRIES, MAX_EXTENTS, READ_BUDGET, Strategy, plan_reads};
use crate::util::AlignedBuf;
use crate::volume::*;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use xxhash_rust::xxh3::{xxh3_64, xxh3_128_with_seed};

/// Hash granularity (divides every cluster size of every exFAT volume).
pub(crate) const BLOCK: u64 = 512;
/// Largest device read; extents are split to fit one read with alignment slack.
const CHUNK: usize = 8 << 20;
/// Alignment of every device read (O_DIRECT needs the logical block size; 4 KB covers it).
const ALIGN: u64 = 4096;
/// Test hook (shared with pull): print every data read ("flashcp-read OFFSET LEN") to stderr.
const TRACE_READS_ENV: &str = "FLASHCP_TEST_TRACE_READS";
/// Threads hashing local files.
const LOCAL_READERS: usize = 4;
const MANIFEST_MAGIC: &str = "flashcp-manifest 1";

/// Progress counters a long hash run updates (read by the caller's progress loop).
#[derive(Default)]
pub(crate) struct Progress {
    pub(crate) files: AtomicU64,
    pub(crate) bytes: AtomicU64,
}

/// A file in a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileSum {
    pub(crate) len: u64,
    pub(crate) hash: u128,
    /// Modification time (UTC secs, nanos), None when the volume's timestamp is undecodable.
    pub(crate) mtime: Option<(i64, u32)>,
}

/// Every file and directory of a tree, by relative path ("a/b/c.txt").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) files: BTreeMap<String, FileSum>,
    pub(crate) dirs: BTreeMap<String, Option<(i64, u32)>>,
}

impl Manifest {
    pub(crate) fn bytes(&self) -> u64 {
        self.files.values().map(|f| f.len).sum()
    }

    /// The same manifest with every hash 0 (to compare listings only).
    pub(crate) fn without_hashes(&self) -> Manifest {
        let mut m = self.clone();
        m.files.values_mut().for_each(|f| f.hash = 0);
        m
    }

    /// Every difference between `self` (named `a`) and `other` (named `b`); empty when they
    /// hold the same files with the same contents and the same directories. Times are compared
    /// only where both sides have one.
    pub(crate) fn diff(&self, other: &Manifest, a: &str, b: &str) -> Vec<String> {
        let mut out = Vec::new();
        let t = |x: &Option<(i64, u32)>, y: &Option<(i64, u32)>| x.is_some() && y.is_some() && x != y;
        for (p, f) in &self.files {
            match other.files.get(p) {
                None => out.push(format!("{p}: missing in {b}")),
                Some(g) if g.len != f.len => out.push(format!("{p}: {} bytes in {a}, {} in {b}", f.len, g.len)),
                Some(g) if g.hash != f.hash => out.push(format!("{p}: contents differ between {a} and {b}")),
                Some(g) if t(&f.mtime, &g.mtime) => out.push(format!("{p}: modification time differs")),
                Some(_) => {}
            }
        }
        out.extend(other.files.keys().filter(|p| !self.files.contains_key(*p)).map(|p| format!("{p}: missing in {a}")));
        for (p, m) in &self.dirs {
            match other.dirs.get(p) {
                None => out.push(format!("{p}/: directory missing in {b}")),
                Some(n) if t(m, n) => out.push(format!("{p}/: directory modification time differs")),
                Some(_) => {}
            }
        }
        out.extend(
            other
                .dirs
                .keys()
                .filter(|p| !self.dirs.contains_key(*p))
                .map(|p| format!("{p}/: directory missing in {a}")),
        );
        out
    }

    /// Text form, one entry per line, ending with a line that checksums everything before it
    /// (so a truncated or damaged manifest is refused, never half-trusted). Paths contain no
    /// control characters (exFAT and `flashcp build` forbid them; checked before saving).
    pub(crate) fn encode(&self) -> String {
        let t = |m: &Option<(i64, u32)>| m.map_or("-".to_string(), |(s, n)| format!("{s}.{n:09}"));
        let mut s = format!("{MANIFEST_MAGIC}\n");
        for (p, m) in &self.dirs {
            s += &format!("D\t{}\t{p}\n", t(m));
        }
        for (p, f) in &self.files {
            s += &format!("F\t{}\t{:032x}\t{}\t{p}\n", f.len, f.hash, t(&f.mtime));
        }
        let sum = xxh3_64(s.as_bytes());
        s += &format!("end\t{}\t{sum:016x}\n", self.dirs.len() + self.files.len());
        s
    }

    pub(crate) fn decode(s: &str) -> Result<Manifest> {
        let body_end =
            s.trim_end_matches('\n').rfind('\n').map(|i| i + 1).ok_or_else(|| anyhow!("manifest is empty"))?;
        let (body, end) = s.split_at(body_end);
        let f: Vec<&str> = end.trim_end_matches('\n').split('\t').collect();
        ensure!(f.len() == 3 && f[0] == "end", "manifest is truncated (no end line)");
        ensure!(
            u64::from_str_radix(f[2], 16).ok() == Some(xxh3_64(body.as_bytes())),
            "manifest checksum mismatch (the file is damaged)"
        );
        let mut lines = body.lines();
        ensure!(lines.next() == Some(MANIFEST_MAGIC), "not a flashcp manifest");
        let t = |v: &str| -> Result<Option<(i64, u32)>> {
            if v == "-" {
                return Ok(None);
            }
            let (s, n) = v.split_once('.').ok_or_else(|| anyhow!("bad time {v:?}"))?;
            Ok(Some((s.parse()?, n.parse()?)))
        };
        let mut m = Manifest::default();
        for l in lines {
            let c: Vec<&str> = l.splitn(5, '\t').collect();
            match c.as_slice() {
                ["D", mt, p] if !p.is_empty() => {
                    m.dirs.insert(p.to_string(), t(mt)?);
                }
                ["F", len, h, mt, p] if !p.is_empty() => {
                    let fs = FileSum { len: len.parse()?, hash: u128::from_str_radix(h, 16)?, mtime: t(mt)? };
                    m.files.insert(p.to_string(), fs);
                }
                _ => bail!("bad manifest line {l:?}"),
            }
        }
        ensure!(f[1].parse::<usize>().ok() == Some(m.dirs.len() + m.files.len()), "manifest entry count mismatch");
        Ok(m)
    }
}

// ───────────────────────── block hashing ─────────────────────────

fn block_hash(b: &[u8], k: u64) -> u128 {
    xxh3_128_with_seed(b, k)
}

/// Sum over the blocks of `data`, whose first byte is block `k0`'s first byte. Every block is
/// whole except possibly the last.
fn hash_blocks(data: &[u8], k0: u64) -> u128 {
    data.chunks(BLOCK as usize).enumerate().fold(0u128, |a, (i, b)| a.wrapping_add(block_hash(b, k0 + i as u64)))
}

/// The hash of a file fed front to back in pieces of any length (what `build` reads for the
/// device, so `optimize` can check the very bytes it writes against the manifest). Equal to
/// what [`hash_tree`] computes for the same content.
#[derive(Clone)]
pub(crate) struct BlockSum {
    /// Index of the block being filled.
    k: u64,
    part: [u8; BLOCK as usize],
    plen: usize,
    sum: u128,
    pub(crate) len: u64,
}

impl Default for BlockSum {
    fn default() -> Self {
        BlockSum { k: 0, part: [0; BLOCK as usize], plen: 0, sum: 0, len: 0 }
    }
}

impl BlockSum {
    pub(crate) fn update(&mut self, mut d: &[u8]) {
        self.len += d.len() as u64;
        if self.plen > 0 {
            let n = (BLOCK as usize - self.plen).min(d.len());
            self.part[self.plen..self.plen + n].copy_from_slice(&d[..n]);
            self.plen += n;
            d = &d[n..];
            if self.plen < BLOCK as usize {
                return;
            }
            self.sum = self.sum.wrapping_add(block_hash(&self.part, self.k));
            self.k += 1;
            self.plen = 0;
        }
        let whole = d.len() / BLOCK as usize * BLOCK as usize;
        self.sum = self.sum.wrapping_add(hash_blocks(&d[..whole], self.k));
        self.k += (whole / BLOCK as usize) as u64;
        let rest = &d[whole..];
        self.part[..rest.len()].copy_from_slice(rest);
        self.plen = rest.len();
    }

    /// (bytes fed, hash).
    pub(crate) fn finish(&self) -> (u64, u128) {
        let mut h = self.sum;
        if self.plen > 0 {
            h = h.wrapping_add(block_hash(&self.part[..self.plen], self.k));
        }
        (self.len, h)
    }
}

/// Sum over the all-zero blocks of a file of `len` bytes from byte `valid` (ValidDataLength) on.
/// The block holding `valid` itself (if it is mid-block) is not included: it is hashed with the
/// extent that ends there, padded with zeros.
pub(crate) fn zero_tail(valid: u64, len: u64) -> u128 {
    static ZERO: [u8; BLOCK as usize] = [0; BLOCK as usize];
    (valid.div_ceil(BLOCK)..len.div_ceil(BLOCK)).fold(0u128, |a, k| {
        let n = (len - k * BLOCK).min(BLOCK) as usize;
        a.wrapping_add(block_hash(&ZERO[..n], k))
    })
}

/// Sum over one extent's bytes at file offset `foff` (a multiple of BLOCK) of a file of `len`
/// bytes. A short last block ends at ValidDataLength; it is padded with the zeros the file
/// holds there.
pub(crate) fn hash_extent(data: &[u8], foff: u64, len: u64) -> u128 {
    debug_assert!(foff.is_multiple_of(BLOCK));
    let whole = data.len() / BLOCK as usize * BLOCK as usize;
    let mut h = hash_blocks(&data[..whole], foff / BLOCK);
    if whole < data.len() {
        let k = (foff + whole as u64) / BLOCK;
        let mut b = [0u8; BLOCK as usize];
        let n = (len - k * BLOCK).min(BLOCK) as usize;
        b[..data.len() - whole].copy_from_slice(&data[whole..]);
        h = h.wrapping_add(block_hash(&b[..n], k));
    }
    h
}

// ───────────────────────── the volume side ─────────────────────────

/// A directory of the volume (index 0 is the root).
pub(crate) struct VDir {
    pub(crate) path: String,
    pub(crate) name: Vec<u16>,
    pub(crate) first: u32,
    pub(crate) len: u64,
    pub(crate) no_fat_chain: bool,
    pub(crate) mtime: Option<(i64, u32)>,
    pub(crate) dirs: Vec<u32>,
    pub(crate) files: Vec<u32>,
}

pub(crate) struct VFile {
    pub(crate) path: String,
    pub(crate) name: Vec<u16>,
    pub(crate) len: u64,
    pub(crate) valid: u64,
    pub(crate) first: u32,
    pub(crate) no_fat_chain: bool,
    pub(crate) mtime: Option<(i64, u32)>,
}

/// Everything a volume holds, as walked and checked; enough to hash it without the parser.
pub(crate) struct VolTree {
    pub(crate) dirs: Vec<VDir>,
    pub(crate) files: Vec<VFile>,
    pub(crate) root_clusters: Vec<u32>,
    exts: Vec<Ext>,
}

impl VolTree {
    pub(crate) fn data_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.len).sum()
    }

    /// The walk as a manifest without contents (every hash 0): paths, sizes and times only.
    pub(crate) fn listing(&self) -> Manifest {
        let mut m = Manifest::default();
        for f in &self.files {
            m.files.insert(f.path.clone(), FileSum { len: f.len, hash: 0, mtime: f.mtime });
        }
        for d in self.dirs.iter().skip(1) {
            m.dirs.insert(d.path.clone(), d.mtime);
        }
        m
    }
}

/// A volume name as one safe path component: strict UTF-16 (a name that is not valid Unicode
/// would not survive the round trip through a local file name), no '/', NUL, "." or "..".
fn component(name: &[u16], parent: &str) -> Result<String> {
    let s = String::from_utf16(name).map_err(|_| {
        anyhow!("{parent}/: a name is not valid Unicode (UTF-16); it cannot be copied losslessly (run fsck.exfat)")
    })?;
    ensure!(
        !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\0']),
        "{parent}/: entry name {s:?} cannot be used as a file name"
    );
    Ok(s)
}

/// Pushes the extents of `bytes` bytes at `phys` (file offset `foff`), split at `max`.
fn push_run(exts: &mut Vec<Ext>, file: u32, mut phys: u64, mut foff: u64, mut bytes: u64, max: u64) {
    while bytes > 0 {
        let n = bytes.min(max);
        exts.push(Ext { phys, foff, len: n as u32, file });
        (phys, foff, bytes) = (phys + n, foff + n, bytes - n);
    }
}

/// Walks the whole volume from the root `Vol::open` parsed, checking every allocation, and
/// lists every directory, file and data extent (up to each file's ValidDataLength).
pub(crate) fn walk(v: &mut Vol) -> Result<VolTree> {
    // A multiple of BLOCK, so a split never cuts a block; aligning a read adds < 2 × ALIGN.
    let max = CHUNK as u64 - 2 * ALIGN;
    let capacity = v.cc * v.cs;
    let mut seen = cluster_set(v.cc);
    let root = v.dirs.first().ok_or_else(|| anyhow!("no root directory"))?;
    let root_clusters = root.clusters.clone();
    let start: Vec<Child> = root.children.values().cloned().collect();
    let system = [(&root_clusters, "the root directory"), (&v.bitmap_clusters, "the allocation bitmap")];
    for (clusters, what) in system.into_iter().chain([(&v.upcase_clusters, "the up-case table")]) {
        for &c in clusters {
            ensure!(mark(&mut seen, c), "cluster {c} of {what} is used twice (cross-linked); run fsck.exfat");
        }
    }
    let mut t = VolTree {
        dirs: vec![VDir {
            path: String::new(),
            name: Vec::new(),
            first: root_clusters[0],
            len: root_clusters.len() as u64 * v.cs,
            no_fat_chain: false,
            mtime: None,
            dirs: Vec::new(),
            files: Vec::new(),
        }],
        files: Vec::new(),
        root_clusters,
        exts: Vec::new(),
    };
    let mut stack: Vec<(Vec<Child>, u32, usize)> = vec![(start, 0, 0)];
    let mut entries = 0u64;
    while let Some((kids, di, depth)) = stack.pop() {
        let parent = t.dirs[di as usize].path.clone();
        for ch in kids {
            entries += 1;
            ensure!(entries <= MAX_ENTRIES, "the volume has more than {MAX_ENTRIES} files and directories");
            let name = component(&ch.name, &parent)?;
            let path = if parent.is_empty() { name } else { format!("{parent}/{name}") };
            ensure!(
                ch.len <= capacity,
                "{path}: DataLength {} is larger than the volume's cluster heap ({capacity}); run fsck.exfat",
                ch.len
            );
            let mtime = decode_ts(ch.mtime.0, ch.mtime.1, ch.mtime.2);
            if ch.is_dir {
                ensure!(depth < MAX_DEPTH, "{path}: directories are nested more than {MAX_DEPTH} deep");
                let sub = v.read_child(0, &parent, &ch)?;
                for &c in &sub.clusters {
                    ensure!(
                        mark(&mut seen, c),
                        "{path}: cluster {c} is used twice (cross-linked, or a directory cycle); run fsck.exfat"
                    );
                }
                let idx = u32::try_from(t.dirs.len()).context("too many directories")?;
                t.dirs[di as usize].dirs.push(idx);
                t.dirs.push(VDir {
                    path,
                    name: ch.name.clone(),
                    first: ch.first,
                    len: ch.len,
                    no_fat_chain: ch.no_fat_chain,
                    mtime,
                    dirs: Vec::new(),
                    files: Vec::new(),
                });
                stack.push((sub.children.into_values().collect(), idx, depth + 1));
                continue;
            }
            let fi = u32::try_from(t.files.len()).context("too many files")?;
            // The whole allocation is walked and marked; only clusters up to ValidDataLength
            // become extents (the rest reads as zeros).
            let total = ch.len.div_ceil(v.cs);
            let need = ch.valid_len.div_ceil(v.cs);
            if total > 0 && ch.no_fat_chain {
                contiguous_run(ch.first, total, v.cc)
                    .with_context(|| format!("{path}: contiguous extent runs past the end of the volume"))?;
                mark_range(&mut seen, ch.first, total)
                    .map_err(|c| anyhow!("{path}: cluster {c} is used twice (cross-linked); run fsck.exfat"))?;
                push_run(&mut t.exts, fi, v.cluster_off(ch.first), 0, ch.valid_len, max);
            } else if total > 0 {
                let (mut c, mut run_first, mut run_n, mut foff) = (ch.first, ch.first, 0u64, 0u64);
                for k in 0..total {
                    ensure!(v.valid(c), "{path}: FAT chain points at invalid cluster {c:#x}");
                    ensure!(
                        mark(&mut seen, c),
                        "{path}: cluster {c} is used twice (FAT loop or cross-link); run fsck.exfat"
                    );
                    if k < need {
                        if run_n > 0 && c as u64 != run_first as u64 + run_n {
                            let bytes = run_n * v.cs;
                            push_run(&mut t.exts, fi, v.cluster_off(run_first), foff, bytes, max);
                            (foff, run_n) = (foff + bytes, 0);
                        }
                        if run_n == 0 {
                            run_first = c;
                        }
                        run_n += 1;
                    }
                    let next = v.fat(c)?;
                    if k + 1 == total {
                        ensure!(next == EOC, "{path}: FAT chain is longer than its DataLength");
                    } else {
                        ensure!(next != EOC, "{path}: FAT chain is shorter than its DataLength");
                        c = next;
                    }
                }
                if need > 0 {
                    push_run(&mut t.exts, fi, v.cluster_off(run_first), foff, ch.valid_len - foff, max);
                }
            }
            ensure!(t.exts.len() <= MAX_EXTENTS, "the volume's files have more than {MAX_EXTENTS} fragments");
            t.dirs[di as usize].files.push(fi);
            t.files.push(VFile {
                path,
                name: ch.name.clone(),
                len: ch.len,
                valid: ch.valid_len,
                first: ch.first,
                no_fat_chain: ch.no_fat_chain,
                mtime,
            });
        }
    }
    Ok(t)
}

/// Reads `buf.len()` bytes at `off`, or fewer at the end of the device. Returns bytes read.
fn read_full(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], off + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Hashes every file of the walked volume on `dev`, reading in physical order with pull's
/// planner and the read strategy `s` (gap, reads in flight, O_DIRECT).
pub(crate) fn hash_volume(dev: &File, t: &VolTree, s: &Strategy, prog: &Progress) -> Result<Manifest> {
    let mut exts = t.exts.clone();
    exts.sort_unstable_by_key(|e| e.phys);
    let chunks = plan_reads(&exts, CHUNK, s.gap);
    // Each reader holds one CHUNK buffer; as in pull, at most READ_BUDGET of them.
    let readers = s.inflight.clamp(1, READ_BUDGET / CHUNK).min(chunks.len().max(1));
    let trace = std::env::var_os(TRACE_READS_ENV).is_some();
    // Bytes still to read per file; a file is done when its count reaches zero.
    let left: Vec<AtomicU64> = t.files.iter().map(|f| AtomicU64::new(f.valid)).collect();
    prog.files.fetch_add(t.files.iter().filter(|f| f.valid == 0).count() as u64, Ordering::Relaxed);
    let direct = AtomicBool::new(s.direct && set_direct(dev, true));
    if !s.direct {
        set_direct(dev, false);
        unsafe { libc::posix_fadvise(std::os::fd::AsRawFd::as_raw_fd(dev), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    // Each reader collects (file, partial sum) pairs; summed per file at the end.
    let parts: Mutex<Vec<(u32, u128)>> = Mutex::new(Vec::with_capacity(exts.len()));
    std::thread::scope(|sc| {
        for _ in 0..readers {
            sc.spawn(|| {
                let mut buf = AlignedBuf::new(CHUNK);
                let mut mine = Vec::new();
                let r = (|| -> Result<()> {
                    while !failed.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(c) = chunks.get(i) else { return Ok(()) };
                        let need = exts[c.exts.clone()].iter().map(|e| e.phys + e.len as u64).max().unwrap_or(c.start)
                            - c.start;
                        let b = &mut buf.as_mut()[..c.len];
                        if trace {
                            eprintln!("flashcp-read {} {}", c.start, c.len);
                        }
                        let mut r = read_full(dev, b, c.start);
                        // A device that refuses this alignment (or a short direct read at the
                        // very end): fall back to buffered reads for the rest of the run.
                        if (r.is_err() || r.as_ref().is_ok_and(|&n| (n as u64) < need))
                            && direct.swap(false, Ordering::Relaxed)
                        {
                            set_direct(dev, false);
                            r = read_full(dev, b, c.start);
                        }
                        let n = r.with_context(|| format!("read {} bytes at byte {} of the drive", c.len, c.start))?;
                        ensure!(n as u64 >= need, "short read at byte {}: {n} of {need} bytes", c.start);
                        for e in &exts[c.exts.clone()] {
                            let at = (e.phys - c.start) as usize;
                            let f = &t.files[e.file as usize];
                            mine.push((e.file, hash_extent(&b[at..at + e.len as usize], e.foff, f.len)));
                            prog.bytes.fetch_add(e.len as u64, Ordering::Relaxed);
                            if left[e.file as usize].fetch_sub(e.len as u64, Ordering::Relaxed) == e.len as u64 {
                                prog.files.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Ok(())
                })();
                parts.lock().unwrap().append(&mut mine);
                if let Err(e) = r {
                    failed.store(true, Ordering::Relaxed);
                    first_err.lock().unwrap().get_or_insert(e);
                }
            });
        }
    });
    set_direct(dev, false);
    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    let mut sums = vec![0u128; t.files.len()];
    for (f, h) in parts.into_inner().unwrap() {
        sums[f as usize] = sums[f as usize].wrapping_add(h);
    }
    let mut m = Manifest::default();
    for (f, h) in t.files.iter().zip(sums) {
        let hash = h.wrapping_add(zero_tail(f.valid, f.len));
        prog.bytes.fetch_add(f.len - f.valid, Ordering::Relaxed);
        m.files.insert(f.path.clone(), FileSum { len: f.len, hash, mtime: f.mtime });
    }
    for d in t.dirs.iter().skip(1) {
        m.dirs.insert(d.path.clone(), d.mtime);
    }
    Ok(m)
}

// ───────────────────────── the local side ─────────────────────────

fn mtime_of(md: &fs::Metadata) -> Option<(i64, u32)> {
    Some((md.mtime(), md.mtime_nsec() as u32))
}

/// Hashes every file under `root` (regular files and directories only: anything else means the
/// copy is not what `pull` wrote, and is refused).
pub(crate) fn hash_tree(root: &Path, prog: &Progress) -> Result<Manifest> {
    let mut m = Manifest::default();
    let mut todo: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for e in fs::read_dir(&dir).with_context(|| format!("read directory {}", dir.display()))? {
            let e = e.with_context(|| format!("read directory {}", dir.display()))?;
            let p = e.path();
            let name = e.file_name().into_string().map_err(|_| anyhow!("{}: name is not UTF-8", p.display()))?;
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let md = fs::symlink_metadata(&p).with_context(|| format!("stat {}", p.display()))?;
            if md.is_dir() {
                m.dirs.insert(r.clone(), mtime_of(&md));
                stack.push((p, r));
            } else if md.is_file() {
                m.files.insert(r.clone(), FileSum { len: md.len(), hash: 0, mtime: mtime_of(&md) });
                todo.push((r, p, md.len()));
            } else {
                bail!("{} is neither a regular file nor a directory", p.display());
            }
        }
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let sums: Mutex<Vec<(usize, u64, u128)>> = Mutex::new(Vec::with_capacity(todo.len()));
    std::thread::scope(|s| {
        for _ in 0..LOCAL_READERS.min(todo.len().max(1)) {
            s.spawn(|| {
                let mut buf = vec![0u8; 1 << 20];
                while !failed.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((_, p, _)) = todo.get(i) else { return };
                    match hash_file(p, &mut buf, prog) {
                        Ok((len, h)) => {
                            sums.lock().unwrap().push((i, len, h));
                            prog.files.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            failed.store(true, Ordering::Relaxed);
                            first_err.lock().unwrap().get_or_insert(e);
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    for (i, len, h) in sums.into_inner().unwrap() {
        let f = m.files.get_mut(&todo[i].0).expect("listed above");
        // The length actually read counts, not the one listed (a file that changed shows up).
        f.len = len;
        f.hash = h;
    }
    Ok(m)
}

/// (bytes read, hash) of one local file, read front to back in whole blocks.
fn hash_file(p: &Path, buf: &mut [u8], prog: &Progress) -> Result<(u64, u128)> {
    let mut f = File::open(p).with_context(|| format!("open {}", p.display()))?;
    crate::util::fadvise_sequential(&f);
    let (mut off, mut h) = (0u64, 0u128);
    loop {
        // Fill the buffer completely (short reads would misalign the blocks).
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
            }
        }
        if n == 0 {
            return Ok((off, h));
        }
        h = h.wrapping_add(hash_blocks(&buf[..n], off / BLOCK));
        off += n as u64;
        prog.bytes.fetch_add(n as u64, Ordering::Relaxed);
        if n < buf.len() {
            return Ok((off, h));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference definition: every block of the logical content, in order.
    fn reference(content: &[u8]) -> u128 {
        hash_blocks(content, 0)
    }

    #[test]
    fn extent_and_tail_hashing_matches_reference() {
        // A 3000-byte file with ValidDataLength 1300: bytes 1300.. are zeros.
        let mut content = vec![0u8; 3000];
        for (i, b) in content[..1300].iter_mut().enumerate() {
            *b = (i * 7 + 1) as u8;
        }
        let want = reference(&content);
        // Two extents (cluster 1024), physical order reversed, then the zero tail.
        let h = hash_extent(&content[1024..1300], 1024, 3000)
            .wrapping_add(hash_extent(&content[..1024], 0, 3000))
            .wrapping_add(zero_tail(1300, 3000));
        assert_eq!(h, want);
        // Valid ends on a block boundary, length not a block multiple.
        let h = hash_extent(&content[..1024], 0, 1100).wrapping_add(zero_tail(1024, 1100));
        let mut c = content[..1100].to_vec();
        c[1024..].fill(0);
        assert_eq!(h, reference(&c));
        assert_eq!(zero_tail(0, 0), 0);
        assert_eq!(hash_extent(&content[..700], 0, 700), reference(&content[..700]));
    }

    #[test]
    fn block_sum_in_any_pieces_matches_the_file_hash() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 31 + i / 7) as u8).collect();
        let mut buf = vec![0u8; 1024];
        let p = std::env::temp_dir().join(format!("flashcp-blocksum-{}", std::process::id()));
        for len in [0usize, 1, 511, 512, 513, 1024, 4999, 5000] {
            fs::write(&p, &data[..len]).unwrap();
            let want = hash_file(&p, &mut buf, &Progress::default()).unwrap();
            assert_eq!(want.1, reference(&data[..len]));
            for step in [1usize, 7, 300, 512, 700, 4096] {
                let mut s = BlockSum::default();
                data[..len].chunks(step).for_each(|c| s.update(c));
                assert_eq!(s.finish(), want, "len {len} step {step}");
            }
        }
        fs::remove_file(&p).unwrap();
    }

    #[test]
    fn hash_depends_on_block_positions() {
        let a: Vec<u8> = (0..1024).map(|i| (i / 512) as u8 + 1).collect();
        let mut b = a[512..].to_vec();
        b.extend_from_slice(&a[..512]);
        assert_ne!(reference(&a), reference(&b));
        assert_ne!(reference(&[]), reference(&[0]));
    }

    #[test]
    fn manifest_round_trips_and_refuses_damage() {
        let mut m = Manifest::default();
        m.dirs.insert("a".into(), Some((1_700_000_000, 10_000_000)));
        m.dirs.insert("a/é 😀".into(), None);
        m.files.insert("a/x.bin".into(), FileSum { len: 5, hash: u128::MAX - 3, mtime: None });
        m.files.insert("top".into(), FileSum { len: 0, hash: 0, mtime: Some((315_532_800, 0)) });
        let s = m.encode();
        assert_eq!(Manifest::decode(&s).unwrap(), m);
        assert!(Manifest::decode(&s[..s.len() - 10]).is_err());
        assert!(Manifest::decode(&s.replace("x.bin", "y.bin")).is_err());
        assert!(Manifest::decode("").is_err());
        let mut other = m.clone();
        other.files.get_mut("a/x.bin").unwrap().hash ^= 1;
        other.dirs.remove("a/é 😀");
        other.files.insert("new".into(), FileSum { len: 1, hash: 1, mtime: None });
        let d = m.diff(&other, "A", "B");
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(m.diff(&m, "A", "B").is_empty());
    }
}
