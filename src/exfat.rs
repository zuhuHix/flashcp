//! One-pass exFAT volume builder.
//!
//! Instead of asking the kernel's filesystem driver to create files one by one, we plan the
//! entire volume in memory (boot region, FAT, allocation bitmap, up-case table, every
//! directory, every file's clusters) and then write it to the device as ONE strictly
//! ascending stream of large, aligned, O_DIRECT writes. Every file is contiguous, so its
//! stream entry uses NoFatChain, exactly like a file written by a well-behaved driver.
//!
//! The result is a standard exFAT volume with ordinary loose files.
//!
//! Spec: Microsoft "exFAT file system specification" (2019, public).

use crate::util::{AlignedBuf, human_bytes};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

const SECTOR: u64 = 512;
const UPCASE: &[u8] = include_bytes!("upcase.bin");
const UPCASE_CHECKSUM: u32 = 0xE619D30D;
/// Cluster heap is aligned to this many bytes (erase-block friendly).
const ALIGN: u64 = 4 << 20;
const IO_CHUNK: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum FatMode {
    /// Zero the entire FAT, like mkfs.exfat does. Always spec-clean.
    Full,
    /// Write only the FAT entries that are in use. Free-cluster FAT entries are not
    /// meaningful in exFAT (the allocation bitmap is authoritative), so this saves up to
    /// a gigabyte of writes on big drives with small clusters.
    Prefix,
}

#[derive(Clone, Debug)]
pub struct BuildOpts {
    pub cluster: Option<u32>,
    pub label: String,
    pub fat: FatMode,
    pub readers: usize,
    pub dry_run: bool,
}

// ───────────────────────── up-case table & checksums ─────────────────────────

fn upcase_map() -> Vec<u16> {
    let mut map: Vec<u16> = (0..=0xFFFFu32).map(|c| c as u16).collect();
    let words: Vec<u16> = UPCASE.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let (mut i, mut ch) = (0usize, 0usize);
    while i < words.len() && ch < 0x10000 {
        if words[i] == 0xFFFF && i + 1 < words.len() {
            ch += words[i + 1] as usize; // run of identity mappings
            i += 2;
        } else {
            map[ch] = words[i];
            ch += 1;
            i += 1;
        }
    }
    map
}

fn checksum32(data: &[u8], skip: &[usize], mut c: u32) -> u32 {
    for (i, &b) in data.iter().enumerate() {
        if !skip.contains(&i) {
            c = c.rotate_right(1).wrapping_add(b as u32);
        }
    }
    c
}

fn checksum16(data: &[u8], skip_first_23: bool) -> u16 {
    let mut c: u16 = 0;
    for (i, &b) in data.iter().enumerate() {
        if skip_first_23 && (i == 2 || i == 3) {
            continue;
        }
        c = c.rotate_right(1).wrapping_add(b as u16);
    }
    c
}

// ───────────────────────── tree model ─────────────────────────

struct Node {
    name: Vec<u16>,
    src: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: (i64, u32),
    children: Vec<usize>,
    first_cluster: u32,
    clusters: u64,
}

fn validate_name(name: &[u16], upc: &[u16], seen: &mut HashSet<Vec<u16>>, path: &Path) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 255, "name too long for exFAT: {}", path.display());
    for &c in name {
        ensure!(
            c >= 0x20 && !matches!(c, 0x22 | 0x2A | 0x2F | 0x3A | 0x3C | 0x3E | 0x3F | 0x5C | 0x7C),
            "character not allowed on exFAT in {}",
            path.display()
        );
    }
    let up: Vec<u16> = name.iter().map(|&c| upc[c as usize]).collect();
    ensure!(seen.insert(up), "two names differ only by case (exFAT is case-insensitive): {}", path.display());
    Ok(())
}

fn scan_tree(root: &Path, upc: &[u16]) -> Result<Vec<Node>> {
    let mut nodes = vec![Node {
        name: vec![],
        src: root.to_path_buf(),
        is_dir: true,
        size: 0,
        mtime: (0, 0),
        children: vec![],
        first_cluster: 0,
        clusters: 0,
    }];
    // Breadth-first, sorted: all directories at one level are laid out together.
    let mut queue = std::collections::VecDeque::from([0usize]);
    while let Some(di) = queue.pop_front() {
        let mut ents: Vec<_> = fs::read_dir(&nodes[di].src)?.collect::<Result<_, _>>()?;
        ents.sort_by_key(|e| e.file_name());
        let mut seen = HashSet::new();
        for e in ents {
            let md = fs::symlink_metadata(e.path())?;
            let ft = md.file_type();
            if !(ft.is_dir() || ft.is_file()) {
                eprintln!("skipping non-regular file {}", e.path().display());
                continue;
            }
            let name_str = e.file_name().into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 name"))?;
            let name: Vec<u16> = name_str.encode_utf16().collect();
            validate_name(&name, upc, &mut seen, &e.path())?;
            let idx = nodes.len();
            nodes.push(Node {
                name,
                src: e.path(),
                is_dir: ft.is_dir(),
                size: if ft.is_dir() { 0 } else { md.len() },
                mtime: (md.mtime(), md.mtime_nsec() as u32),
                children: vec![],
                first_cluster: 0,
                clusters: 0,
            });
            nodes[di].children.push(idx);
            if ft.is_dir() {
                queue.push_back(idx);
            }
        }
    }
    Ok(nodes)
}

fn set_bytes(n: &Node) -> u64 {
    32 * (2 + n.name.len().div_ceil(15) as u64)
}

// ───────────────────────── geometry & planning ─────────────────────────

#[derive(Debug, Clone, Copy)]
struct Geometry {
    cluster: u64,
    fat_off: u64,  // sectors
    fat_len: u64,  // sectors
    heap_off: u64, // sectors
    cluster_count: u64,
}

/// Largest cluster size the spec allows (SectorsPerClusterShift ≤ 25 − BytesPerSectorShift).
const MAX_CLUSTER: u64 = 32 << 20;

fn valid_cluster(cluster: u64) -> bool {
    cluster.is_power_of_two() && (SECTOR..=MAX_CLUSTER).contains(&cluster)
}

fn geometry(vol_bytes: u64, cluster: u64) -> Option<Geometry> {
    if !valid_cluster(cluster) {
        return None;
    }
    let vol = vol_bytes / SECTOR;
    let fat_off = (1 << 20) / SECTOR;
    let spc = cluster / SECTOR;
    let est = vol.checked_sub(fat_off)? / spc;
    let fat_len = ((est + 2) * 4).div_ceil(SECTOR);
    let heap_off = (fat_off + fat_len).div_ceil(ALIGN / SECTOR) * (ALIGN / SECTOR);
    let cc = vol.checked_sub(heap_off)? / spc;
    if !(16..=0xFFFF_FFF5).contains(&cc) {
        return None;
    }
    Some(Geometry { cluster, fat_off, fat_len, heap_off, cluster_count: cc })
}

struct Plan {
    g: Geometry,
    bitmap_clusters: u64,
    upcase_clusters: u64,
    root_cluster: u32,
    used_end: u64, // first never-used cluster index
    /// Bytes we'll physically write (the cost the cluster-size optimizer minimizes).
    write_bytes: u64,
}

fn plan(nodes: &mut [Node], g: Geometry, fat: FatMode) -> Result<Plan> {
    let cs = g.cluster;
    let bitmap_clusters = g.cluster_count.div_ceil(8).div_ceil(cs);
    let upcase_clusters = (UPCASE.len() as u64).div_ceil(cs);
    let mut next = 2 + bitmap_clusters + upcase_clusters;
    // Directories first (BFS order = node order), all metadata in one region.
    for i in 0..nodes.len() {
        if nodes[i].is_dir {
            let mut bytes: u64 = nodes[i].children.iter().map(|&c| set_bytes(&nodes[c])).sum();
            if i == 0 {
                bytes += 3 * 32; // label, bitmap, up-case entries
            }
            let n = bytes.div_ceil(cs).max(1);
            nodes[i].first_cluster = next as u32;
            nodes[i].clusters = n;
            next += n;
        }
    }
    // Then file data, in directory order, so reading a folder is sequential too.
    let order: Vec<usize> = (0..nodes.len())
        .filter(|&i| nodes[i].is_dir)
        .flat_map(|d| nodes[d].children.clone())
        .filter(|&c| !nodes[c].is_dir)
        .collect();
    for i in order {
        let n = nodes[i].size.div_ceil(cs);
        if n > 0 {
            nodes[i].first_cluster = next as u32;
            nodes[i].clusters = n;
            next += n;
        }
    }
    ensure!(next - 2 <= g.cluster_count, "data does not fit on the volume");
    let fat_bytes = match fat {
        FatMode::Full => g.fat_len * SECTOR,
        FatMode::Prefix => (next * 4).div_ceil(4096) * 4096,
    };
    let write_bytes = 24 * SECTOR + fat_bytes + (next - 2) * cs;
    Ok(Plan { g, bitmap_clusters, upcase_clusters, root_cluster: nodes[0].first_cluster, used_end: next, write_bytes })
}

// ───────────────────────── on-disk structures ─────────────────────────

fn boot_region(g: &Geometry, part_off: u64, vol_bytes: u64, root: u32, serial: u32, pct: u8) -> Vec<u8> {
    let mut r = vec![0u8; 12 * 512];
    let b = &mut r[..512];
    b[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    b[3..11].copy_from_slice(b"EXFAT   ");
    b[64..72].copy_from_slice(&part_off.to_le_bytes());
    b[72..80].copy_from_slice(&(vol_bytes / SECTOR).to_le_bytes());
    b[80..84].copy_from_slice(&(g.fat_off as u32).to_le_bytes());
    b[84..88].copy_from_slice(&(g.fat_len as u32).to_le_bytes());
    b[88..92].copy_from_slice(&(g.heap_off as u32).to_le_bytes());
    b[92..96].copy_from_slice(&(g.cluster_count as u32).to_le_bytes());
    b[96..100].copy_from_slice(&root.to_le_bytes());
    b[100..104].copy_from_slice(&serial.to_le_bytes());
    b[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
    b[108] = 9;
    b[109] = (g.cluster / SECTOR).trailing_zeros() as u8;
    b[110] = 1;
    b[111] = 0x80;
    b[112] = pct;
    b[120..510].fill(0xF4);
    b[510] = 0x55;
    b[511] = 0xAA;
    for s in 1..=8 {
        r[s * 512 + 510] = 0x55;
        r[s * 512 + 511] = 0xAA;
    }
    let c = checksum32(&r[..11 * 512], &[106, 107, 112], 0);
    for i in 0..128 {
        r[11 * 512 + i * 4..11 * 512 + i * 4 + 4].copy_from_slice(&c.to_le_bytes());
    }
    r
}

/// exFAT timestamp (UTC) + 10 ms increment.
fn ts(secs: i64, nsec: u32) -> (u32, u8) {
    // exFAT years run 1980..=2107. Out-of-range times saturate to the nearest end with
    // no sub-second part, matching what the Linux driver writes.
    const MIN: i64 = 315_532_800; // 1980-01-01 00:00:00
    const MAX: i64 = 4_354_819_199; // 2107-12-31 23:59:59
    let (secs, nsec) = if secs < MIN {
        (MIN, 0)
    } else if secs > MAX {
        (MAX, 999_999_999)
    } else {
        (secs, nsec.min(999_999_999))
    };
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    // civil_from_days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let y = y - 1980;
    let (h, mi, s) = (sod / 3600, sod % 3600 / 60, sod % 60);
    let t =
        (y as u32) << 25 | (m as u32) << 21 | (d as u32) << 16 | (h as u32) << 11 | (mi as u32) << 5 | (s as u32 / 2);
    (t, ((s % 2) * 100 + (nsec / 10_000_000) as i64) as u8)
}

fn entry_set(n: &Node, upc: &[u16]) -> Vec<u8> {
    let names = n.name.len().div_ceil(15);
    let mut e = vec![0u8; 32 * (2 + names)];
    let (t, t10) = ts(n.mtime.0, n.mtime.1);
    // File directory entry
    e[0] = 0x85;
    e[1] = (1 + names) as u8;
    e[4..6].copy_from_slice(&(if n.is_dir { 0x10u16 } else { 0x20 }).to_le_bytes());
    for off in [8, 12, 16] {
        e[off..off + 4].copy_from_slice(&t.to_le_bytes());
    }
    e[20] = t10;
    e[21] = t10;
    e[22] = 0x80; // UTC offset valid, +0
    e[23] = 0x80;
    e[24] = 0x80;
    // Stream extension
    let s = &mut e[32..64];
    s[0] = 0xC0;
    let first = n.first_cluster;
    s[1] = if first != 0 { 0x03 } else { 0x01 }; // AllocationPossible | NoFatChain
    s[3] = n.name.len() as u8;
    let mut h: u16 = 0;
    for &c in &n.name {
        let u = upc[c as usize];
        for b in u.to_le_bytes() {
            h = h.rotate_right(1).wrapping_add(b as u16);
        }
    }
    s[4..6].copy_from_slice(&h.to_le_bytes());
    s[20..24].copy_from_slice(&first.to_le_bytes());
    // File name entries
    for (k, chunk) in n.name.chunks(15).enumerate() {
        let f = &mut e[64 + 32 * k..96 + 32 * k];
        f[0] = 0xC1;
        for (j, &c) in chunk.iter().enumerate() {
            f[2 + 2 * j..4 + 2 * j].copy_from_slice(&c.to_le_bytes());
        }
    }
    e
}

fn finish_set(mut e: Vec<u8>, valid_len: u64, data_len: u64) -> Vec<u8> {
    e[32 + 8..32 + 16].copy_from_slice(&valid_len.to_le_bytes());
    e[32 + 24..32 + 32].copy_from_slice(&data_len.to_le_bytes());
    let c = checksum16(&e, true);
    e[2..4].copy_from_slice(&c.to_le_bytes());
    e
}

fn dir_bytes(nodes: &[Node], di: usize, p: &Plan, label: &[u16], upc: &[u16]) -> Vec<u8> {
    let cs = p.g.cluster;
    let mut out = Vec::with_capacity((nodes[di].clusters * cs) as usize);
    if di == 0 {
        let mut l = [0u8; 32];
        l[0] = 0x83;
        l[1] = label.len() as u8;
        for (j, &c) in label.iter().enumerate() {
            l[2 + 2 * j..4 + 2 * j].copy_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&l);
        let mut b = [0u8; 32];
        b[0] = 0x81;
        b[20..24].copy_from_slice(&2u32.to_le_bytes());
        b[24..32].copy_from_slice(&p.g.cluster_count.div_ceil(8).to_le_bytes());
        out.extend_from_slice(&b);
        let mut u = [0u8; 32];
        u[0] = 0x82;
        u[4..8].copy_from_slice(&UPCASE_CHECKSUM.to_le_bytes());
        u[20..24].copy_from_slice(&((2 + p.bitmap_clusters) as u32).to_le_bytes());
        u[24..32].copy_from_slice(&(UPCASE.len() as u64).to_le_bytes());
        out.extend_from_slice(&u);
    }
    for &c in &nodes[di].children {
        let n = &nodes[c];
        let len = if n.is_dir { n.clusters * cs } else { n.size };
        out.extend_from_slice(&finish_set(entry_set(n, upc), len, len));
    }
    out.resize((nodes[di].clusters * cs) as usize, 0);
    out
}

// ───────────────────────── the sequential writer ─────────────────────────

/// Collects bytes into large aligned buffers and hands them to an I/O thread, so building
/// the next chunk overlaps with the device writing the previous one.
struct SeqWriter {
    pos: u64,
    buf: AlignedBuf,
    fill: usize,
    tx: Option<Sender<(u64, AlignedBuf, usize)>>,
    back: Receiver<AlignedBuf>,
    io: Option<std::thread::JoinHandle<Result<()>>>,
    pub written: u64,
}

impl SeqWriter {
    fn new(dev: Arc<File>) -> Self {
        let (tx, rx) = bounded::<(u64, AlignedBuf, usize)>(2);
        let (btx, back) = bounded::<AlignedBuf>(4);
        for _ in 0..2 {
            btx.send(AlignedBuf::new(IO_CHUNK)).unwrap();
        }
        let io = std::thread::spawn(move || -> Result<()> {
            for (off, buf, len) in rx {
                dev.write_all_at(&buf.as_ref()[..len], off).with_context(|| format!("write at {off}"))?;
                let _ = btx.send(buf);
            }
            dev.sync_all()?;
            Ok(())
        });
        SeqWriter { pos: 0, buf: AlignedBuf::new(IO_CHUNK), fill: 0, tx: Some(tx), back, io: Some(io), written: 0 }
    }
    fn flush(&mut self) -> Result<()> {
        if self.fill == 0 {
            return Ok(());
        }
        let len = self.fill.div_ceil(4096) * 4096; // O_DIRECT needs aligned lengths
        self.buf.as_mut()[self.fill..len].fill(0);
        let fresh = self.back.recv().map_err(|_| anyhow::anyhow!("I/O thread died"))?;
        let full = std::mem::replace(&mut self.buf, fresh);
        let off = self.pos;
        self.tx.as_ref().unwrap().send((off, full, len)).map_err(|_| anyhow::anyhow!("I/O thread died"))?;
        self.written += len as u64;
        self.pos += len as u64;
        self.fill = 0;
        Ok(())
    }
    fn cur(&self) -> u64 {
        self.pos + self.fill as u64
    }
    fn write(&mut self, mut d: &[u8]) -> Result<()> {
        while !d.is_empty() {
            let n = d.len().min(IO_CHUNK - self.fill);
            self.buf.as_mut()[self.fill..self.fill + n].copy_from_slice(&d[..n]);
            self.fill += n;
            d = &d[n..];
            if self.fill == IO_CHUNK {
                self.flush()?;
            }
        }
        Ok(())
    }
    /// Read a file straight into the I/O buffers (no intermediate copy).
    fn write_from(&mut self, f: &mut File, mut left: u64) -> Result<()> {
        while left > 0 {
            let n = (IO_CHUNK - self.fill).min(left as usize);
            let dst = &mut self.buf.as_mut()[self.fill..self.fill + n];
            f.read_exact(dst)?;
            self.fill += n;
            left -= n as u64;
            if self.fill == IO_CHUNK {
                self.flush()?;
            }
        }
        Ok(())
    }
    fn zeros_to(&mut self, target: u64) -> Result<()> {
        static Z: [u8; 65536] = [0; 65536];
        while self.cur() < target {
            let n = ((target - self.cur()) as usize).min(Z.len());
            self.write(&Z[..n])?;
        }
        Ok(())
    }
    /// Skip forward without writing (only used for regions whose contents don't matter).
    fn skip_to(&mut self, target: u64) -> Result<()> {
        assert!(self.cur().is_multiple_of(4096) && target.is_multiple_of(4096));
        self.flush()?;
        assert!(target >= self.pos);
        self.pos = target;
        Ok(())
    }
    fn finish(mut self) -> Result<u64> {
        self.flush()?;
        drop(self.tx.take());
        self.io.take().unwrap().join().unwrap()?;
        Ok(self.written)
    }
}

// ───────────────────────── ordered parallel reader ─────────────────────────

enum Item {
    Data(Vec<u8>),
    Stream,
}

type Budget = Arc<(Mutex<(usize, u64)>, Condvar)>;

/// Reads files in parallel; the consumer reorders by index. Budget is granted in claim
/// order, so the next file the consumer needs is always already claimed: no deadlock.
fn ordered_reader(
    paths: Vec<(PathBuf, u64)>,
    readers: usize,
    budget: u64,
    stream_over: u64,
) -> (Receiver<(usize, Item)>, Budget) {
    let (tx, rx) = bounded(8192);
    let paths = Arc::new(paths);
    let state: Budget = Arc::new((Mutex::new((0usize, budget)), Condvar::new()));
    for _ in 0..readers.max(1) {
        let (tx, paths, state) = (tx.clone(), paths.clone(), state.clone());
        std::thread::spawn(move || {
            loop {
                let idx = {
                    let (m, cv) = &*state;
                    let mut g = m.lock().unwrap();
                    loop {
                        if g.0 >= paths.len() {
                            return;
                        }
                        let sz = paths[g.0].1;
                        let cost = if sz > stream_over { 0 } else { sz.min(budget) };
                        if cost <= g.1 {
                            g.1 -= cost;
                            g.0 += 1;
                            break g.0 - 1;
                        }
                        g = cv.wait(g).unwrap();
                    }
                };
                let (p, sz) = &paths[idx];
                let item = if *sz > stream_over {
                    Item::Stream
                } else {
                    let mut v = Vec::with_capacity(*sz as usize);
                    match File::open(p).and_then(|mut f| f.read_to_end(&mut v)) {
                        Ok(_) => Item::Data(v),
                        Err(_) => Item::Data(Vec::new()), // size check downstream reports it
                    }
                };
                if tx.send((idx, item)).is_err() {
                    return;
                }
            }
        });
    }
    (rx, state)
}

fn release(b: &Budget, bytes: u64) {
    b.0.lock().unwrap().1 += bytes;
    b.1.notify_all();
}

// ───────────────────────── target device ─────────────────────────

struct Target {
    file: File,
    bytes: u64,
    part_off: u64,
    direct: bool,
}

fn open_target(path: &Path) -> Result<Target> {
    let md = fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    let open = |direct: bool| {
        let mut o = OpenOptions::new();
        o.write(true).read(true);
        if direct {
            o.custom_flags(libc::O_DIRECT);
        }
        o.open(path)
    };
    let (file, direct) = match open(true) {
        Ok(f) => (f, true),
        Err(_) => (open(false)?, false),
    };
    if md.file_type().is_block_device() {
        let mut size: u64 = 0;
        // BLKGETSIZE64 = _IOR(0x12, 114, size_t)
        let r = unsafe { libc::ioctl(std::os::unix::io::AsRawFd::as_raw_fd(&file), 0x80081272, &mut size) };
        ensure!(r == 0, "BLKGETSIZE64 failed");
        let (maj, min) = (libc::major(md.rdev()), libc::minor(md.rdev()));
        let start = fs::read_to_string(format!("/sys/dev/block/{maj}:{min}/start"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0u64);
        // Refuse to touch anything that is mounted: the device itself, any of its
        // partitions (a whole disk whose /dev/sdX1 is mounted), or a holder (dm, md).
        let mounts = fs::read_to_string("/proc/self/mountinfo")?;
        if let Some((dev, at)) = mounted_dev(&mounts, &related_devs(maj, min)) {
            bail!("{} is in use: device {dev} is mounted at {at}; unmount it first", path.display());
        }
        Ok(Target { file, bytes: size, part_off: start, direct })
    } else {
        ensure!(md.len() > 0, "image file is empty; create it first, e.g. truncate -s 8G image.img");
        Ok(Target { file, bytes: md.len(), part_off: 0, direct })
    }
}

/// "maj:min" of a block device, its partitions and its holders, read from sysfs.
fn related_devs(maj: u32, min: u32) -> Vec<String> {
    let base = PathBuf::from(format!("/sys/dev/block/{maj}:{min}"));
    let mut v = vec![format!("{maj}:{min}")];
    let mut add = |dir: PathBuf, need_partition: bool| {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if need_partition && !p.join("partition").exists() {
                continue;
            }
            if let Ok(d) = fs::read_to_string(p.join("dev")) {
                v.push(d.trim().to_string());
            }
        }
    };
    add(base.clone(), true);
    add(base.join("holders"), false);
    v
}

/// First mountinfo line whose device ("maj:min", field 3) is in `devs`: (device, mount point).
fn mounted_dev(mountinfo: &str, devs: &[String]) -> Option<(String, String)> {
    mountinfo.lines().find_map(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        let dev = *f.get(2)?;
        devs.iter().any(|d| d == dev).then(|| (dev.to_string(), f.get(4).unwrap_or(&"?").to_string()))
    })
}

// ───────────────────────── build ─────────────────────────

pub struct BuildStats {
    pub files: usize,
    pub dirs: usize,
    pub data_bytes: u64,
    pub written: u64,
    pub cluster: u64,
    pub secs: f64,
}

pub fn build(src: &Path, target: &Path, o: &BuildOpts) -> Result<BuildStats> {
    let t0 = Instant::now();
    let upc = upcase_map();
    let label: Vec<u16> = o.label.encode_utf16().collect();
    ensure!(label.len() <= 11, "label is at most 11 characters");
    let mut nodes = scan_tree(src, &upc)?;
    let tgt = open_target(target)?;

    // Cluster-size optimizer: pick the size that minimizes bytes physically written
    // (FAT + bitmap + per-file slack). Small files want small clusters; huge drives in
    // Full FAT mode want bigger ones.
    let candidates: Vec<u64> = match o.cluster {
        Some(c) => {
            ensure!(
                valid_cluster(c as u64),
                "cluster size must be a power of two between 512 bytes and 32 MB, got {c}"
            );
            vec![c as u64]
        }
        None => (12..=17).map(|s| 1u64 << s).collect(), // 4K … 128K
    };
    let mut best: Option<Plan> = None;
    for c in candidates {
        let Some(g) = geometry(tgt.bytes, c) else { continue };
        let Ok(p) = plan(&mut nodes, g, o.fat) else { continue };
        if best.as_ref().is_none_or(|b| p.write_bytes < b.write_bytes) {
            best = Some(p);
        }
    }
    let Some(best) = best else { bail!("data does not fit, or volume size is unsupported") };
    let p = plan(&mut nodes, best.g, o.fat)?; // re-plan so nodes hold the winner's layout
    let g = p.g;
    let cs = g.cluster;
    let files = nodes.iter().filter(|n| !n.is_dir).count();
    let dirs = nodes.len() - files;
    let data: u64 = nodes.iter().map(|n| n.size).sum();
    println!(
        "plan: {} files, {} dirs, {} data │ cluster {} │ {} clusters │ FAT {} ({:?}) │ will write {}",
        files,
        dirs,
        human_bytes(data as f64),
        human_bytes(cs as f64),
        g.cluster_count,
        human_bytes((g.fat_len * SECTOR) as f64),
        o.fat,
        human_bytes(p.write_bytes as f64)
    );
    if o.dry_run {
        return Ok(BuildStats {
            files,
            dirs,
            data_bytes: data,
            written: 0,
            cluster: cs,
            secs: t0.elapsed().as_secs_f64(),
        });
    }

    let file_order: Vec<usize> = (0..nodes.len())
        .filter(|&i| nodes[i].is_dir)
        .flat_map(|d| nodes[d].children.clone())
        .filter(|&c| !nodes[c].is_dir && nodes[c].size > 0)
        .collect();
    let reads: Vec<(PathBuf, u64)> = file_order.iter().map(|&i| (nodes[i].src.clone(), nodes[i].size)).collect();
    let (rx, budget) = ordered_reader(reads, o.readers, 256 << 20, 16 << 20);
    let mut pending: std::collections::BTreeMap<usize, Item> = Default::default();

    let dev = Arc::new(tgt.file);
    let mut w = SeqWriter::new(dev);
    let serial = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u32;
    let pct = ((p.used_end - 2) * 100 / g.cluster_count).min(100) as u8;

    // 1. Boot region + backup.
    let boot = boot_region(&g, tgt.part_off, tgt.bytes / SECTOR * SECTOR, p.root_cluster, serial, pct);
    w.write(&boot)?;
    w.write(&boot)?;
    // 2. FAT: every allocated object is one contiguous chain.
    w.zeros_to(g.fat_off * SECTOR)?;
    let mut fat = Vec::with_capacity((p.used_end * 4) as usize);
    fat.extend_from_slice(&0xFFFF_FFF8u32.to_le_bytes());
    fat.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let mut chains: Vec<(u64, u64)> = vec![(2, p.bitmap_clusters), (2 + p.bitmap_clusters, p.upcase_clusters)];
    chains.extend(nodes.iter().filter(|n| n.clusters > 0).map(|n| (n.first_cluster as u64, n.clusters)));
    chains.sort();
    let mut expect = 2;
    for (start, len) in chains {
        ensure!(start == expect, "internal layout error");
        for c in start..start + len {
            let v = if c == start + len - 1 { 0xFFFF_FFFF } else { c as u32 + 1 };
            fat.extend_from_slice(&v.to_le_bytes());
        }
        expect = start + len;
    }
    w.write(&fat)?;
    match o.fat {
        FatMode::Full => w.zeros_to(g.heap_off * SECTOR)?,
        FatMode::Prefix => {
            let aligned = w.cur().div_ceil(4096) * 4096;
            w.zeros_to(aligned)?;
            w.skip_to(g.heap_off * SECTOR)?;
        }
    }
    // 3. Allocation bitmap (all used clusters are 2..used_end).
    let used = p.used_end - 2;
    let mut bm = vec![0u8; (p.bitmap_clusters * cs) as usize];
    for i in 0..used as usize {
        bm[i / 8] |= 1 << (i % 8);
    }
    w.write(&bm)?;
    // 4. Up-case table.
    w.write(UPCASE)?;
    w.zeros_to(g.heap_off * SECTOR + (2 + p.bitmap_clusters + p.upcase_clusters - 2) * cs)?;
    // 5. All directories.
    for i in 0..nodes.len() {
        if nodes[i].is_dir {
            debug_assert_eq!(w.cur(), g.heap_off * SECTOR + (nodes[i].first_cluster as u64 - 2) * cs);
            let d = dir_bytes(&nodes, i, &p, &label, &upc);
            w.write(&d)?;
        }
    }
    // 6. File data, in the exact order the readers deliver it.
    for (k, &i) in file_order.iter().enumerate() {
        let n = &nodes[i];
        let at = g.heap_off * SECTOR + (n.first_cluster as u64 - 2) * cs;
        ensure!(w.cur() == at, "internal layout error at {}", n.src.display());
        while !pending.contains_key(&k) {
            let (idx, item) = rx.recv().context("reader stopped")?;
            pending.insert(idx, item);
        }
        let item = pending.remove(&k).unwrap();
        match item {
            Item::Data(v) => {
                ensure!(v.len() as u64 == n.size, "{} changed size while copying", n.src.display());
                w.write(&v)?;
                release(&budget, n.size.min(256 << 20));
            }
            Item::Stream => {
                let mut f = File::open(&n.src)?;
                crate::util::fadvise_sequential(&f);
                w.write_from(&mut f, n.size)?;
            }
        }
        w.zeros_to(at + n.clusters * cs)?;
    }
    let written = w.finish()?;
    if !tgt.direct {
        eprintln!("note: target does not support O_DIRECT, used buffered writes");
    }
    Ok(BuildStats { files, dirs, data_bytes: data, written, cluster: cs, secs: t0.elapsed().as_secs_f64() })
}
