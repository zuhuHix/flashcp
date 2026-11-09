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

use crate::events::{Event, Op, Sink, Ticker};
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

pub(crate) const SECTOR: u64 = 512;
pub(crate) const UPCASE: &[u8] = include_bytes!("upcase.bin");
const UPCASE_CHECKSUM: u32 = 0xE619D30D;
/// Cluster heap is aligned to this many bytes (erase-block friendly).
const ALIGN: u64 = 4 << 20;
pub(crate) const IO_CHUNK: usize = 8 << 20;
/// VolumeFlags bit 1.
const FLAG_VOLUME_DIRTY: u16 = 2;

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

pub(crate) fn upcase_map() -> Vec<u16> {
    upcase_from(UPCASE)
}

/// Expands an up-case table (compressed or not) into a full 65536-entry map.
pub(crate) fn upcase_from(table: &[u8]) -> Vec<u16> {
    let mut map: Vec<u16> = (0..=0xFFFFu32).map(|c| c as u16).collect();
    let words: Vec<u16> = table.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
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

pub(crate) fn checksum32(data: &[u8], skip: &[usize], mut c: u32) -> u32 {
    for (i, &b) in data.iter().enumerate() {
        if !skip.contains(&i) {
            c = c.rotate_right(1).wrapping_add(b as u32);
        }
    }
    c
}

pub(crate) fn checksum16(data: &[u8], skip_first_23: bool) -> u16 {
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

pub(crate) struct Node {
    pub(crate) name: Vec<u16>,
    pub(crate) src: PathBuf,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) mtime: (i64, u32),
    pub(crate) children: Vec<usize>,
    pub(crate) first_cluster: u32,
    pub(crate) clusters: u64,
}

pub(crate) fn validate_name(name: &[u16], upc: &[u16], seen: &mut HashSet<Vec<u16>>, path: &Path) -> Result<()> {
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

pub(crate) fn scan_tree(root: &Path, upc: &[u16]) -> Result<Vec<Node>> {
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

pub(crate) fn set_bytes(n: &Node) -> u64 {
    32 * (2 + n.name.len().div_ceil(15) as u64)
}

// ───────────────────────── geometry & planning ─────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub(crate) cluster: u64,
    pub(crate) fat_off: u64,  // sectors
    pub(crate) fat_len: u64,  // sectors
    pub(crate) heap_off: u64, // sectors
    pub(crate) cluster_count: u64,
}

/// Largest cluster size the spec allows (SectorsPerClusterShift ≤ 25 − BytesPerSectorShift).
const MAX_CLUSTER: u64 = 32 << 20;

fn valid_cluster(cluster: u64) -> bool {
    cluster.is_power_of_two() && (SECTOR..=MAX_CLUSTER).contains(&cluster)
}

/// Volume layout for `vol_bytes` and `cluster`. `dev_off` is the volume's byte offset on the
/// whole DEVICE (partition start; 0 for an image or a superfloppy): the cluster heap is aligned
/// to [`ALIGN`] in absolute device terms, so clusters line up with the flash's erase blocks
/// even when the partition itself starts at, say, 1 MiB. An offset that is not a multiple of
/// 4 KB is ignored (the heap is then aligned within the partition).
pub(crate) fn geometry(vol_bytes: u64, cluster: u64, dev_off: u64) -> Option<Geometry> {
    if !valid_cluster(cluster) {
        return None;
    }
    let vol = vol_bytes / SECTOR;
    let fat_off = (1 << 20) / SECTOR;
    let spc = cluster / SECTOR;
    let est = vol.checked_sub(fat_off)? / spc;
    let fat_len = ((est + 2) * 4).div_ceil(SECTOR);
    let base = if dev_off.is_multiple_of(4096) { dev_off / SECTOR } else { 0 };
    let heap_off = (base + fat_off + fat_len).div_ceil(ALIGN / SECTOR) * (ALIGN / SECTOR) - base;
    let cc = vol.checked_sub(heap_off)? / spc;
    if !(16..=0xFFFF_FFF5).contains(&cc) {
        return None;
    }
    Some(Geometry { cluster, fat_off, fat_len, heap_off, cluster_count: cc })
}

pub(crate) struct Plan {
    pub(crate) g: Geometry,
    pub(crate) bitmap_clusters: u64,
    pub(crate) upcase_clusters: u64,
    pub(crate) root_cluster: u32,
    pub(crate) used_end: u64, // first never-used cluster index
    /// Bytes we'll physically write (the cost the cluster-size optimizer minimizes).
    pub(crate) write_bytes: u64,
}

pub(crate) fn plan(nodes: &mut [Node], g: Geometry, fat: FatMode) -> Result<Plan> {
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

/// Main boot region (12 sectors). `flags` is VolumeFlags (excluded from the checksum, so the
/// backup region written with other flags still matches).
fn boot_region(g: &Geometry, part_off: u64, vol_bytes: u64, root: u32, serial: u32, pct: u8, flags: u16) -> Vec<u8> {
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
    b[106..108].copy_from_slice(&flags.to_le_bytes());
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
pub(crate) fn ts(secs: i64, nsec: u32) -> (u32, u8) {
    // exFAT years run 1980..=2107. Earlier times become exactly 1980-01-01 00:00:00 (what
    // the Linux driver writes); later ones become the last representable instant.
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

pub(crate) fn entry_set(n: &Node, upc: &[u16]) -> Vec<u8> {
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

pub(crate) fn finish_set(mut e: Vec<u8>, valid_len: u64, data_len: u64) -> Vec<u8> {
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
///
/// The stream may skip forward ([`SeqWriter::skip_to`]) without breaking the buffer: a buffer
/// then holds several spans for different device offsets, which the I/O side writes with
/// several requests in flight (a scattered tail of small files is IOPS-bound, not
/// bandwidth-bound). A purely sequential stream is one span per buffer, i.e. one big write.
pub(crate) struct SeqWriter {
    buf: AlignedBuf,
    fill: usize,
    /// (offset in buf, device offset) where each span of the current buffer starts.
    spans: Vec<(usize, u64)>,
    tx: Option<Sender<Batch>>,
    back: Receiver<AlignedBuf>,
    io: Option<std::thread::JoinHandle<Result<()>>>,
    pub written: u64,
    /// Write lengths and skips are rounded to this (4096 for O_DIRECT, 1 for buffered I/O,
    /// where padding would overwrite bytes that are not ours).
    align: usize,
    /// Device offset of stream offset 0 (see [`SeqWriter::set_device_offset`]).
    base: u64,
}

/// A full buffer and its spans: (offset in buf, device offset, length).
type Batch = (AlignedBuf, Vec<(usize, u64, usize)>);

/// Concurrent writes per buffer when it holds many spans.
const IO_WORKERS: usize = 8;

fn write_batch(dev: &File, buf: &AlignedBuf, spans: &[(usize, u64, usize)]) -> Result<()> {
    let one = |&(bo, off, len): &(usize, u64, usize)| -> Result<()> {
        dev.write_all_at(&buf.as_ref()[bo..bo + len], off).with_context(|| format!("write at {off}"))
    };
    if spans.len() == 1 {
        return one(&spans[0]);
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..IO_WORKERS.min(spans.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= spans.len() {
                        return;
                    }
                    if let Err(e) = one(&spans[i]) {
                        first_err.lock().unwrap().get_or_insert(e);
                        next.store(spans.len(), std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
    });
    match first_err.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

impl SeqWriter {
    pub(crate) fn new(dev: Arc<File>) -> Self {
        Self::with_align(dev, 4096)
    }
    pub(crate) fn with_align(dev: Arc<File>, align: usize) -> Self {
        let (tx, rx) = bounded::<Batch>(2);
        let (btx, back) = bounded::<AlignedBuf>(4);
        for _ in 0..2 {
            btx.send(AlignedBuf::new(IO_CHUNK)).unwrap();
        }
        let io = std::thread::spawn(move || -> Result<()> {
            for (buf, spans) in rx {
                write_batch(&dev, &buf, &spans)?;
                let _ = btx.send(buf);
            }
            dev.sync_all()?;
            Ok(())
        });
        SeqWriter {
            buf: AlignedBuf::new(IO_CHUNK),
            fill: 0,
            spans: vec![(0, 0)],
            tx: Some(tx),
            back,
            io: Some(io),
            written: 0,
            align,
            base: 0,
        }
    }
    /// Makes buffer flushes (the big writes) end on absolute DEVICE multiples of [`IO_CHUNK`]
    /// when the stream's offset 0 sits at byte `dev_off` of the whole device (a partition's
    /// start). Ignored unless `dev_off` keeps every flush aligned (a multiple of `align`).
    pub(crate) fn set_device_offset(&mut self, dev_off: u64) {
        if dev_off.is_multiple_of(self.align as u64) && self.fill == 0 {
            self.base = dev_off;
        }
    }
    /// Bytes that fit before the buffer is full or the next absolute IO_CHUNK boundary.
    fn room(&self) -> usize {
        let to_boundary = IO_CHUNK - ((self.cur() + self.base) % IO_CHUNK as u64) as usize;
        (IO_CHUNK - self.fill).min(to_boundary)
    }
    fn at_boundary(&self) -> bool {
        self.fill == IO_CHUNK || (self.fill > 0 && (self.cur() + self.base).is_multiple_of(IO_CHUNK as u64))
    }
    fn flush(&mut self) -> Result<()> {
        if self.fill == 0 {
            return Ok(());
        }
        let len = self.fill.div_ceil(self.align) * self.align; // O_DIRECT needs aligned lengths
        self.buf.as_mut()[self.fill..len].fill(0);
        let mut spans = Vec::with_capacity(self.spans.len());
        for (k, &(bo, off)) in self.spans.iter().enumerate() {
            let end = self.spans.get(k + 1).map_or(len, |n| n.0);
            if end > bo {
                spans.push((bo, off, end - bo));
            }
        }
        let end_pos = self.spans.last().map(|&(bo, off)| off + (len - bo) as u64).unwrap();
        // The I/O thread drops its ends of both channels when it stops (error or panic), so
        // neither call can block forever; either failure surfaces the thread's own error.
        let Ok(fresh) = self.back.recv() else { return Err(self.io_error()) };
        let full = std::mem::replace(&mut self.buf, fresh);
        let Some(tx) = self.tx.as_ref() else { return Err(self.io_error()) };
        if tx.send((full, spans)).is_err() {
            return Err(self.io_error());
        }
        self.written += len as u64;
        self.fill = 0;
        self.spans.clear();
        self.spans.push((0, end_pos));
        Ok(())
    }
    pub(crate) fn cur(&self) -> u64 {
        let &(bo, off) = self.spans.last().unwrap();
        off + (self.fill - bo) as u64
    }
    pub(crate) fn write(&mut self, mut d: &[u8]) -> Result<()> {
        while !d.is_empty() {
            let n = d.len().min(self.room());
            self.buf.as_mut()[self.fill..self.fill + n].copy_from_slice(&d[..n]);
            self.fill += n;
            d = &d[n..];
            if self.at_boundary() {
                self.flush()?;
            }
        }
        Ok(())
    }
    /// Read a file straight into the I/O buffers (no intermediate copy).
    pub(crate) fn write_from(&mut self, f: &mut File, mut left: u64) -> Result<()> {
        while left > 0 {
            let n = self.room().min(left as usize);
            let dst = &mut self.buf.as_mut()[self.fill..self.fill + n];
            f.read_exact(dst)?;
            self.fill += n;
            left -= n as u64;
            if self.at_boundary() {
                self.flush()?;
            }
        }
        Ok(())
    }
    pub(crate) fn zeros_to(&mut self, target: u64) -> Result<()> {
        static Z: [u8; 65536] = [0; 65536];
        while self.cur() < target {
            let n = ((target - self.cur()) as usize).min(Z.len());
            self.write(&Z[..n])?;
        }
        Ok(())
    }
    /// Skip forward without writing (only used for regions whose contents don't matter).
    pub(crate) fn skip_to(&mut self, target: u64) -> Result<()> {
        let a = self.align as u64;
        let cur = self.cur();
        assert!(cur.is_multiple_of(a) && target.is_multiple_of(a));
        assert!(target >= cur);
        let last = self.spans.last_mut().unwrap();
        if last.0 == self.fill {
            last.1 = target; // empty span: just move it
        } else {
            self.spans.push((self.fill, target));
        }
        Ok(())
    }
    /// Joins the stopped I/O thread and returns its original error.
    fn io_error(&mut self) -> anyhow::Error {
        drop(self.tx.take());
        match self.io.take().map(|h| h.join()) {
            Some(Ok(Err(e))) => e,
            Some(Err(_)) => anyhow::anyhow!("I/O thread panicked"),
            Some(Ok(Ok(()))) | None => anyhow::anyhow!("I/O thread stopped"),
        }
    }
    pub(crate) fn finish(mut self) -> Result<u64> {
        self.flush()?;
        drop(self.tx.take());
        match self.io.take().map(|h| h.join()) {
            Some(Ok(r)) => r?,
            Some(Err(_)) => bail!("I/O thread panicked"),
            None => bail!("I/O thread stopped"),
        }
        Ok(self.written)
    }
}

// ───────────────────────── ordered parallel reader ─────────────────────────

pub(crate) enum Item {
    Data(Vec<u8>),
    Stream,
}

pub(crate) type Budget = Arc<(Mutex<(usize, u64)>, Condvar)>;

/// Reads files in parallel; the consumer reorders by index. Budget is granted in claim
/// order, so the next file the consumer needs is always already claimed: no deadlock.
pub(crate) fn ordered_reader(
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

pub(crate) fn release(b: &Budget, bytes: u64) {
    b.0.lock().unwrap().1 += bytes;
    b.1.notify_all();
}

// ───────────────────────── target device ─────────────────────────

pub(crate) struct Target {
    pub(crate) file: File,
    pub(crate) bytes: u64,
    pub(crate) part_off: u64,
    /// O_DIRECT is available on this file (and currently enabled; see [`set_direct`]).
    pub(crate) direct: bool,
    /// Restores the open file description's original status flags when the run ends, on
    /// every path (success, error, unwind). Matters for --target-fd, whose description is
    /// shared with the helper that opened it.
    pub(crate) _flags: FlagGuard,
}

/// Snapshot of an open file description's status flags (F_GETFL), restored on Drop. Holds its
/// own dup of the fd (same description), so it works whoever closes the original first.
pub(crate) struct FlagGuard {
    file: File,
    flags: libc::c_int,
}

impl FlagGuard {
    pub(crate) fn new(f: &File) -> Result<FlagGuard> {
        use std::os::unix::io::AsRawFd;
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
        ensure!(flags >= 0, "F_GETFL failed: {}", std::io::Error::last_os_error());
        Ok(FlagGuard { file: f.try_clone().context("dup target fd")?, flags })
    }
}

impl Drop for FlagGuard {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_SETFL, self.flags) };
    }
}

/// Where to write: a path flashcp opens itself, or an inherited file descriptor that a
/// privileged helper already opened (O_RDWR|O_EXCL, or O_RDONLY for `info`) on a BLOCK DEVICE.
/// An fd is never reopened by path; flashcp itself may run unprivileged.
#[derive(Clone, Debug)]
pub enum TargetSpec {
    Path(PathBuf),
    Fd(std::os::unix::io::RawFd),
}

impl TargetSpec {
    pub fn describe(&self) -> String {
        match self {
            TargetSpec::Path(p) => p.display().to_string(),
            TargetSpec::Fd(fd) => format!("fd {fd}"),
        }
    }
    /// Opens (or adopts) the target for writing, with every safety check applied.
    pub(crate) fn open_rw(&self) -> Result<Target> {
        match self {
            TargetSpec::Path(p) => open_target(p),
            TargetSpec::Fd(fd) => open_target_fd(*fd, true),
        }
    }
}

impl From<&Path> for TargetSpec {
    fn from(p: &Path) -> Self {
        TargetSpec::Path(p.to_path_buf())
    }
}

/// Turns O_DIRECT on or off on an open file description (F_SETFL). Returns false if the file
/// does not support it (EINVAL); the caller then uses buffered I/O.
pub(crate) fn set_direct(f: &File, on: bool) -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = f.as_raw_fd();
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if fl < 0 {
        return false;
    }
    let want = if on { fl | libc::O_DIRECT } else { fl & !libc::O_DIRECT };
    want == fl || unsafe { libc::fcntl(fd, libc::F_SETFL, want) } == 0
}

/// Block device size in bytes (BLKGETSIZE64).
pub(crate) fn blk_size(f: &File) -> Result<u64> {
    let mut size: u64 = 0;
    let r = unsafe { libc::ioctl(std::os::unix::io::AsRawFd::as_raw_fd(f), BLKGETSIZE64 as _, &mut size) };
    ensure!(r == 0, "BLKGETSIZE64 failed: {}", std::io::Error::last_os_error());
    Ok(size)
}

pub(crate) fn open_target(path: &Path) -> Result<Target> {
    // Open first, then trust only what the opened fd says (fstat), so a path that changes
    // underneath us (a file swapped for a device node, a /dev/disk/by-* link retargeted)
    // cannot make the checks below refer to a different object than the one we write.
    //
    // O_EXCL is always passed, so the claim is atomic with the open. Without O_CREAT, Linux
    // treats it as an exclusive claim on block devices: it fails with EBUSY if the device, a
    // partition or a holder is mounted or claimed (LVM, dm, md), and nobody can mount it while
    // we hold the fd (for the whole build). For regular files Linux ignores it; those get an
    // exclusive flock instead.
    let busy = |e: std::io::Error| -> anyhow::Error {
        if e.raw_os_error() == Some(libc::EBUSY) {
            // We are refusing anyway; a path-based lookup only improves the message.
            if let Some((dev, at)) = mount_of(path) {
                return anyhow::anyhow!(
                    "{} is in use: device {dev} is mounted at {at}; unmount it first",
                    path.display()
                );
            }
            anyhow::anyhow!("{} is in use (mounted or held by LVM/dm/md); unmount it first", path.display())
        } else {
            anyhow::Error::new(e).context(format!("open {}", path.display()))
        }
    };
    let file = OpenOptions::new().write(true).read(true).custom_flags(libc::O_EXCL).open(path).map_err(busy)?;
    let md = file.metadata().with_context(|| format!("fstat {}", path.display()))?;
    let ft = md.file_type();
    if ft.is_block_device() {
        block_target(file, &md, &path.display().to_string())
    } else if ft.is_file() {
        if let Some(dev) = crate::add::loop_device_of(&md) {
            bail!(
                "{} is attached to loop device {dev} (it may be mounted); unmount and detach it first",
                path.display()
            );
        }
        // Held until the file is closed (end of the run): two flashcp runs on one image, or
        // any other flock-respecting writer, cannot interleave.
        let r = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            ensure!(
                e.raw_os_error() != Some(libc::EWOULDBLOCK),
                "{} is locked by another process (another flashcp run?)",
                path.display()
            );
            return Err(anyhow::Error::new(e).context(format!("flock {}", path.display())));
        }
        ensure!(md.len() > 0, "image file is empty; create it first, e.g. truncate -s 8G image.img");
        let _flags = FlagGuard::new(&file)?;
        let direct = set_direct(&file, true);
        Ok(Target { file, bytes: md.len(), part_off: 0, direct, _flags })
    } else {
        bail!("{} is neither a block device nor a regular image file", path.display())
    }
}

/// Adopts an inherited fd. It must be open (F_GETFD), O_RDWR when `write` (any read mode
/// otherwise), and a block device. For writing, the same post-open checks as
/// [`open_target`] apply (mounted/holders, size, partition start); only our own O_EXCL open
/// is skipped, because the inherited open file description already holds that claim.
pub(crate) fn open_target_fd(fd: std::os::unix::io::RawFd, write: bool) -> Result<Target> {
    use std::os::unix::io::FromRawFd;
    ensure!(
        fd >= 0 && unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1,
        "--target-fd {fd} is not an open file descriptor"
    );
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(fl >= 0, "--target-fd {fd}: F_GETFL failed");
    let acc = fl & libc::O_ACCMODE;
    if write {
        ensure!(acc == libc::O_RDWR, "--target-fd {fd} must be opened read-write (O_RDWR)");
    } else {
        ensure!(acc == libc::O_RDONLY || acc == libc::O_RDWR, "--target-fd {fd} is not readable");
    }
    // Only now take ownership (it is closed when the run ends).
    let file = unsafe { File::from_raw_fd(fd) };
    let md = file.metadata().with_context(|| format!("fstat fd {fd}"))?;
    ensure!(
        md.file_type().is_block_device(),
        "--target-fd {fd} is not a block device (only block devices are accepted)"
    );
    let name = format!("fd {fd}");
    if write {
        block_target(file, &md, &name)
    } else {
        let bytes = blk_size(&file)?;
        let _flags = FlagGuard::new(&file)?;
        Ok(Target { file, bytes, part_off: 0, direct: false, _flags })
    }
}

/// Post-open checks and geometry for a block device we hold (exclusively claimed).
fn block_target(file: File, md: &fs::Metadata, name: &str) -> Result<Target> {
    // Friendlier message naming the mount point. Sound after the open: the O_EXCL claim
    // already blocks new mounts, so this cannot go stale.
    let (maj, min) = (libc::major(md.rdev()), libc::minor(md.rdev()));
    let devs = related_devs(Path::new(SYS_BLOCK), &format!("{maj}:{min}"))?;
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    if let Some((dev, at)) = mounted_dev(&mounts, &devs) {
        bail!("{name} is in use: device {dev} is mounted at {at}; unmount it first");
    }
    let size = blk_size(&file)?;
    let start = fs::read_to_string(format!("{SYS_BLOCK}/{maj}:{min}/start"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0u64);
    let _flags = FlagGuard::new(&file)?;
    let direct = set_direct(&file, true);
    Ok(Target { file, bytes: size, part_off: start, direct, _flags })
}

/// Best-effort (device, mount point) of a mounted block device at `path`, its partitions or
/// holders. Only used to word an error message, never to decide whether to write.
fn mount_of(path: &Path) -> Option<(String, String)> {
    let md = fs::metadata(path).ok().filter(|m| m.file_type().is_block_device())?;
    let devs =
        related_devs(Path::new(SYS_BLOCK), &format!("{}:{}", libc::major(md.rdev()), libc::minor(md.rdev()))).ok()?;
    mounted_dev(&fs::read_to_string("/proc/self/mountinfo").ok()?, &devs)
}

/// BLKGETSIZE64 = _IOR(0x12, 114, size_t). The ioctl request type is c_ulong on glibc but
/// c_int on musl, so the literal is cast at the call site (`as _`) instead of typed.
pub(crate) const BLKGETSIZE64: u32 = 0x8008_1272;

pub(crate) const SYS_BLOCK: &str = "/sys/dev/block";

/// Every "maj:min" related to `target`: itself, its partitions and its holders, recursively
/// (a partition's LVM PV → dm-0 → dm-crypt …), with cycle protection. `sys` is the
/// /sys/dev/block directory (a parameter so tests can use a fake tree). Fails closed: any
/// sysfs read error is an error, except a node that has no `holders` directory.
pub(crate) fn related_devs(sys: &Path, target: &str) -> Result<Vec<String>> {
    let read_dev = |p: &Path| -> Result<String> {
        Ok(fs::read_to_string(p.join("dev"))
            .with_context(|| format!("reading {}", p.join("dev").display()))?
            .trim()
            .to_string())
    };
    let mut seen: Vec<String> = Vec::new();
    let mut stack = vec![target.to_string()];
    while let Some(dev) = stack.pop() {
        if seen.contains(&dev) {
            continue;
        }
        let node = sys.join(&dev);
        // Partitions are real subdirectories (not symlinks) that contain a `partition` file.
        for e in fs::read_dir(&node).with_context(|| format!("cannot inspect {} in sysfs", node.display()))? {
            let e = e.with_context(|| format!("reading {}", node.display()))?;
            let p = e.path();
            if !e.file_type().with_context(|| format!("reading {}", p.display()))?.is_dir() {
                continue;
            }
            if is_partition(&p)? {
                stack.push(read_dev(&p)?);
            }
        }
        match fs::read_dir(node.join("holders")) {
            Ok(rd) => {
                for e in rd {
                    let p = e.with_context(|| format!("reading {}/holders", node.display()))?.path();
                    stack.push(read_dev(&p)?);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}/holders", node.display())),
        }
        seen.push(dev);
    }
    Ok(seen)
}

/// Whether a sysfs block-node subdirectory is a partition (has a `partition` file). Fails
/// closed: only "not found" means no; any other error (EACCES, EIO, …) is returned.
fn is_partition(p: &Path) -> Result<bool> {
    match fs::symlink_metadata(p.join("partition")) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("checking {}", p.join("partition").display())),
    }
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

/// Builds a whole volume from `src` onto `target`, reporting progress through `sink`.
pub fn build(src: &Path, target: &Path, o: &BuildOpts, sink: Sink<'_>) -> Result<BuildStats> {
    build_to(src, &TargetSpec::from(target), o, sink)
}

/// [`build`] onto a [`TargetSpec`] (a path, or an inherited block-device fd).
pub fn build_to(src: &Path, target: &TargetSpec, o: &BuildOpts, sink: Sink<'_>) -> Result<BuildStats> {
    let label: Vec<u16> = o.label.encode_utf16().collect();
    ensure!(label.len() <= 11, "label is at most 11 characters");
    let nodes = scan_tree(src, &upcase_map())?;
    let tgt = target.open_rw()?;
    build_target(nodes, tgt, o, sink)
}

/// Test hook: abort the process (as if killed) once this many bytes have been handed to the
/// device in the data stream. Used by the `optimize` crash tests.
const ABORT_AT_ENV: &str = "FLASHCP_TEST_BUILD_ABORT_AT";

/// Volume bytes used on a target of `bytes`: rounded down to 4 KB, so that no write (O_DIRECT
/// pads to 4 KB) can reach past the end. An image file therefore never changes size.
pub(crate) fn usable_bytes(bytes: u64) -> u64 {
    bytes / 4096 * 4096
}

/// The target's absolute byte offset on its whole device (partition start), or 0.
pub(crate) fn device_offset(tgt: &Target) -> u64 {
    tgt.part_off * SECTOR
}

/// Cluster-size optimizer: plans every candidate cluster size and returns the cheapest plan
/// (with `nodes` laid out for it). An explicit `o.cluster` is the only candidate.
pub(crate) fn choose_cluster(nodes: &mut [Node], tgt: &Target, o: &BuildOpts) -> Result<Plan> {
    let vol_bytes = usable_bytes(tgt.bytes);
    let dev_off = device_offset(tgt);
    // Pick the size that minimizes bytes physically written (FAT + bitmap + per-file slack).
    // Small files want small clusters; huge drives in Full FAT mode want bigger ones.
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
        let Some(g) = geometry(vol_bytes, c, dev_off) else { continue };
        let Ok(p) = plan(nodes, g, o.fat) else { continue };
        if best.as_ref().is_none_or(|b| p.write_bytes < b.write_bytes) {
            best = Some(p);
        }
    }
    let Some(best) = best else { bail!("data does not fit, or volume size is unsupported") };
    plan(nodes, best.g, o.fat) // re-plan so nodes hold the winner's layout
}

/// [`build`] of an already scanned tree onto an already opened (exclusively held) target.
pub(crate) fn build_target(mut nodes: Vec<Node>, tgt: Target, o: &BuildOpts, sink: Sink<'_>) -> Result<BuildStats> {
    let t0 = Instant::now();
    let mut tick = Ticker::new(sink);
    let upc = upcase_map();
    let label: Vec<u16> = o.label.encode_utf16().collect();
    ensure!(label.len() <= 11, "label is at most 11 characters");
    let p = choose_cluster(&mut nodes, &tgt, o)?;
    let vol_bytes = usable_bytes(tgt.bytes);
    let g = p.g;
    let cs = g.cluster;
    let files = nodes.iter().filter(|n| !n.is_dir).count();
    let dirs = nodes.len() - files;
    let data: u64 = nodes.iter().map(|n| n.size).sum();
    let detail = format!(
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
    tick.emit(Event::Plan {
        op: Op::Build,
        files: files as u64,
        dirs: dirs as u64,
        bytes: data,
        write_bytes: p.write_bytes,
        detail,
        strategy: None,
    });
    if o.dry_run {
        tick.emit(Event::Done { files: files as u64, bytes: data, secs: t0.elapsed().as_secs_f64() });
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
    let mut w = SeqWriter::new(dev.clone());
    w.set_device_offset(tgt.part_off * SECTOR);
    let abort_at: Option<u64> = std::env::var(ABORT_AT_ENV).ok().and_then(|v| v.parse().ok());
    let check_abort = |w: &SeqWriter| {
        if abort_at.is_some_and(|a| w.written >= a) {
            std::process::abort();
        }
    };
    let serial = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u32;
    let pct = ((p.used_end - 2) * 100 / g.cluster_count).min(100) as u8;

    // 1. Boot region + backup. The main boot sector says VolumeDirty until the very end, so a
    // build that is interrupted leaves a volume every checker (and `optimize`) sees as unfinished.
    let boot = boot_region(&g, tgt.part_off, vol_bytes, p.root_cluster, serial, pct, 0);
    let dirty_boot = boot_region(&g, tgt.part_off, vol_bytes, p.root_cluster, serial, pct, FLAG_VOLUME_DIRTY);
    w.write(&dirty_boot)?;
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
    let mut bytes_done = 0u64;
    let empty_files = files - file_order.len();
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
        bytes_done += n.size;
        tick.progress((empty_files + k + 1) as u64, bytes_done, false);
        check_abort(&w);
    }
    let written = w.finish()?; // syncs
    // Everything is on the device: now the main boot sector may say "clean".
    let mut head = AlignedBuf::new(4096);
    head.as_mut()[..4096].copy_from_slice(&boot[..4096]);
    dev.write_all_at(head.as_ref(), 0).context("write boot sector")?;
    dev.sync_all().context("sync boot sector")?;
    tick.progress(files as u64, data, true);
    tick.emit(Event::Done { files: files as u64, bytes: data, secs: t0.elapsed().as_secs_f64() });
    if !tgt.direct {
        eprintln!("note: target does not support O_DIRECT, used buffered writes");
    }
    Ok(BuildStats { files, dirs, data_bytes: data, written, cluster: cs, secs: t0.elapsed().as_secs_f64() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: &[u8] = include_bytes!("testdata/mkfs_boot.bin");
    const Y1980: i64 = 315_532_800;
    const KERNEL_FILE_SET: &str = "850213b4200000004d5a695da86d5d58a86d5d589e8980808000000000000000c003000f86fd0000050000000000000000000000060000000500000000000000c1004800e9006c006c006f0020005700f60072006c0064002e00740078007400";
    const KERNEL_DIR_SET: &str = "85023e63100000004d5a695d00002100000021009f0080808000000000000000c003000688890000001000000000000000000000070000000010000000000000c1004400690072005f00c600d800000000000000000000000000000000000000";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    fn node(name: &str, is_dir: bool, size: u64, mtime: (i64, u32), first: u32, clusters: u64) -> Node {
        Node {
            name: utf16(name),
            src: PathBuf::from(name),
            is_dir,
            size,
            mtime,
            children: vec![],
            first_cluster: first,
            clusters,
        }
    }

    /// Root with children [big.bin (10000 B), empty.txt (0 B), sub/ (empty dir)].
    fn sample_tree() -> Vec<Node> {
        let mut root = node("", true, 0, (0, 0), 0, 0);
        root.children = vec![1, 2, 3];
        vec![
            root,
            node("big.bin", false, 10_000, (0, 0), 0, 0),
            node("empty.txt", false, 0, (0, 0), 0, 0),
            node("sub", true, 0, (0, 0), 0, 0),
        ]
    }

    /// (year-1980, month, day, hour, minute, seconds/2, 10 ms) of an exFAT timestamp.
    fn parts(t: (u32, u8)) -> [u32; 7] {
        let v = t.0;
        [v >> 25, (v >> 21) & 0xF, (v >> 16) & 0x1F, (v >> 11) & 0x1F, (v >> 5) & 0x3F, v & 0x1F, t.1 as u32]
    }

    /// Two entry sets must agree on everything except the checksum and the create/access
    /// times; our own checksum must be valid.
    fn assert_sets_match(ours: &[u8], kernel: &[u8]) {
        assert_eq!(ours.len(), kernel.len());
        assert_eq!(ours[0..2], kernel[0..2]);
        assert_eq!(ours[4..6], kernel[4..6]);
        assert_eq!(ours[12..16], kernel[12..16]);
        assert_eq!(ours[21..25], kernel[21..25]);
        assert_eq!(ours[32..], kernel[32..]);
        assert_eq!(u16::from_le_bytes([ours[2], ours[3]]), checksum16(ours, true));
    }

    /// Every byte except `skip` must influence the checksum, and the skipped ones must not.
    fn assert_checksum_skips(data: &[u8], skip: &[usize]) {
        let base = checksum32(data, skip, 0);
        for i in 0..data.len() {
            let mut m = data.to_vec();
            m[i] = m[i].wrapping_add(1);
            let changed = checksum32(&m, skip, 0) != base;
            assert_eq!(changed, !skip.contains(&i), "byte {i}");
        }
    }

    fn validate(name: &str, seen: &mut HashSet<Vec<u16>>, upc: &[u16]) -> Result<()> {
        validate_name(&utf16(name), upc, seen, Path::new("f"))
    }

    fn assert_rejected(name: &str, needle: &str) {
        let upc = upcase_map();
        let err = validate(name, &mut HashSet::new(), &upc).expect_err("name should be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains(needle), "message was: {msg}");
    }

    // ───── inherited target fd ─────

    fn fd_err(fd: i32, write: bool) -> String {
        format!("{:#}", open_target_fd(fd, write).err().expect("fd should be refused"))
    }

    #[test]
    fn target_fd_validation() {
        use std::os::unix::io::IntoRawFd;
        // Closed / never-opened descriptors.
        assert!(fd_err(-1, true).contains("not an open file descriptor"));
        assert!(fd_err(1_000_000, false).contains("not an open file descriptor"));
        let dir = std::env::temp_dir().join(format!("flashcp-fd-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img");
        File::create(&p).unwrap().set_len(1 << 20).unwrap();
        // Read-only fd: refused for build/add before anything else is looked at.
        let ro = File::open(&p).unwrap().into_raw_fd();
        assert!(fd_err(ro, true).contains("O_RDWR"));
        unsafe { libc::close(ro) };
        // Read-write regular file: refused, only block devices are accepted (for info too).
        let rw = File::options().read(true).write(true).open(&p).unwrap().into_raw_fd();
        assert!(fd_err(rw, true).contains("not a block device")); // consumed (closed) by the call
        let ro = File::open(&p).unwrap().into_raw_fd();
        assert!(fd_err(ro, false).contains("not a block device"));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn getfl(f: &File) -> libc::c_int {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) }
    }

    /// The flags of the shared open file description come back exactly as they were, whether
    /// the run succeeds, fails, or panics. `observer` is a dup (same description), like the
    /// helper's own fd under --target-fd.
    #[test]
    fn target_flags_restored_on_every_path() {
        let dir = std::env::temp_dir().join(format!("flashcp-flags-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img");
        File::create(&p).unwrap().set_len(1 << 20).unwrap();
        let run = |fail: bool, panic: bool| -> (libc::c_int, libc::c_int) {
            let t = open_target(&p).unwrap(); // enables O_DIRECT where supported
            let observer = t.file.try_clone().unwrap();
            let before = t._flags.flags;
            // Toggle something every filesystem supports too, so a change is always visible.
            let toggled = getfl(&t.file) ^ libc::O_APPEND;
            assert_eq!(
                unsafe { libc::fcntl(std::os::unix::io::AsRawFd::as_raw_fd(&t.file), libc::F_SETFL, toggled) },
                0
            );
            assert_ne!(getfl(&observer), before, "flags did not change during the run");
            let body = std::panic::AssertUnwindSafe(move || -> Result<()> {
                let _t = t;
                if panic {
                    panic!("injected panic");
                }
                if fail {
                    bail!("injected error");
                }
                Ok(())
            });
            let r = std::panic::catch_unwind(body);
            assert_eq!(r.is_err(), panic);
            if let Ok(res) = r {
                assert_eq!(res.is_err(), fail);
            }
            (before, getfl(&observer))
        };
        for (fail, panic) in [(false, false), (true, false), (false, true)] {
            let (before, after) = run(fail, panic);
            assert_eq!(after, before, "flags not restored (fail={fail}, panic={panic})");
            assert_eq!(after & libc::O_DIRECT, 0, "O_DIRECT left on (fail={fail}, panic={panic})");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    // ───── sequential writer ─────

    /// A target that rejects every write (opened read-only): the error must come back from
    /// write()/finish() promptly, with the I/O thread's own message, never a hang.
    #[test]
    fn seq_writer_propagates_write_failure_without_hanging() {
        let dir = std::env::temp_dir().join(format!("flashcp-seqw-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ro.img");
        File::create(&p).unwrap().set_len(64 << 20).unwrap();
        let ro = Arc::new(File::open(&p).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut w = SeqWriter::with_align(ro, 1);
            let chunk = vec![0xA5u8; 1 << 20];
            let mut res = Ok(());
            for _ in 0..48 {
                res = w.write(&chunk);
                if res.is_err() {
                    break;
                }
            }
            let res = res.and_then(|_| w.finish().map(|_| ()));
            let _ = tx.send(res.map_err(|e| format!("{e:#}")));
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(20)).expect("SeqWriter hung on a failing target");
        let msg = got.expect_err("writes to a read-only target succeeded");
        assert!(msg.contains("write at"), "lost the original I/O error: {msg}");
        fs::remove_dir_all(&dir).unwrap();
    }

    // ───── checksums and up-case table ─────

    #[test]
    fn boot_checksum_matches_mkfs_reference() {
        let c = checksum32(&BOOT[..11 * 512], &[106, 107, 112], 0);
        assert_eq!(c, 0xc224_8af6);
        assert!(BOOT[11 * 512..].as_chunks::<4>().0.iter().all(|w| *w == c.to_le_bytes()));
        assert_eq!(BOOT[11 * 512..].len(), 512);
    }

    #[test]
    fn boot_checksum_ignores_only_flags_and_percent() {
        assert_checksum_skips(&BOOT[..11 * 512], &[106, 107, 112]);
    }

    #[test]
    fn checksum16_reproduces_kernel_entry_checksums() {
        let file = hex(KERNEL_FILE_SET);
        assert_eq!(checksum16(&file, true), 0xb413);
        let dir = hex(KERNEL_DIR_SET);
        assert_eq!(checksum16(&dir, true), 0x633e);
    }

    #[test]
    fn upcase_table_matches_spec_checksum() {
        assert_eq!(UPCASE.len(), 5836);
        assert_eq!(checksum32(UPCASE, &[], 0), UPCASE_CHECKSUM);
    }

    #[test]
    fn upcase_map_maps_letters() {
        let m = upcase_map();
        assert_eq!(m.len(), 0x10000);
        for c in b'a'..=b'z' {
            assert_eq!(m[c as usize], (c - 32) as u16);
        }
        assert_eq!(m[0xE9], 0xC9);
        assert_eq!(m[0x3C9], 0x3A9);
        assert_eq!(m[0x430], 0x410);
        for c in ['A', '0', ' '] {
            assert_eq!(m[c as usize], c as u16);
        }
        assert_eq!(m[0xD83D], 0xD83D);
    }

    // ───── boot region ─────

    #[test]
    fn boot_region_layout_and_checksum() {
        let g = geometry(32 << 20, 4096, 0).unwrap();
        let r = boot_region(&g, 0, 32 << 20, 5, 0x1234, 0, 0);
        assert_eq!(r.len(), 12 * 512);
        assert_eq!(&r[3..11], b"EXFAT   ");
        for s in 0..=8 {
            assert_eq!(r[s * 512 + 510..s * 512 + 512], [0x55, 0xAA]);
        }
        let c = checksum32(&r[..11 * 512], &[106, 107, 112], 0);
        let sector = &r[11 * 512..];
        assert_eq!(sector.len(), 512);
        assert!(sector.as_chunks::<4>().0.iter().all(|w| *w == c.to_le_bytes()));
        assert_eq!(u64_at(&r, 64), 0);
        assert_eq!(u64_at(&r, 72), (32 << 20) / 512);
        assert_eq!(u32_at(&r, 80) as u64, g.fat_off);
        assert_eq!(u32_at(&r, 84) as u64, g.fat_len);
        assert_eq!(u32_at(&r, 88) as u64, g.heap_off);
        assert_eq!(u32_at(&r, 92) as u64, g.cluster_count);
        assert_eq!(u32_at(&r, 96), 5);
        assert_checksum_skips(&r[..11 * 512], &[106, 107, 112]);
    }

    // ───── timestamps ─────

    #[test]
    fn ts_kernel_timestamp() {
        assert_eq!(ts(1_709_214_317, 370_000_000), (0x585d_6da8, 137));
        assert_eq!(parts(ts(1_709_214_317, 370_000_000)), [44, 2, 29, 13, 45, 8, 137]);
    }

    #[test]
    fn ts_epoch_and_pre_1980_clamp_to_epoch() {
        assert_eq!(ts(Y1980, 0), (0x0021_0000, 0));
        for (s, n) in [(0, 0), (-1, 999_999_999), (-1_000_000_000, 5), (Y1980 - 1, 999_999_999)] {
            assert_eq!(ts(s, n), (0x0021_0000, 0), "secs {s}");
        }
    }

    #[test]
    fn ts_saturates_after_2107() {
        const MAX: i64 = 4_354_819_199;
        let expect = [127, 12, 31, 23, 59, 29, 199];
        assert_eq!(parts(ts(MAX, 999_999_999)), expect);
        assert_eq!(parts(ts(MAX + 1, 0)), expect);
        assert_eq!(parts(ts(i64::MAX, 0)), expect);
    }

    #[test]
    fn ts_leap_days_and_century_rule() {
        assert_eq!(parts(ts(951_782_400, 0)), [20, 2, 29, 0, 0, 0, 0]); // 2000-02-29
        assert_eq!(parts(ts(1_582_934_400, 0)), [40, 2, 29, 0, 0, 0, 0]); // 2020-02-29
        // 2100 is not a leap year: 02-28 is followed directly by 03-01.
        assert_eq!(parts(ts(4_107_456_000, 0)), [120, 2, 28, 0, 0, 0, 0]);
        assert_eq!(parts(ts(4_107_542_400, 0)), [120, 3, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn ts_millennium_rollover() {
        assert_eq!(parts(ts(946_684_799, 0)), [19, 12, 31, 23, 59, 29, 100]);
        assert_eq!(parts(ts(946_684_800, 0)), [20, 1, 1, 0, 0, 0, 0]);
    }

    // ───── entry sets ─────

    #[test]
    fn entry_set_matches_kernel_file() {
        let kernel = hex(KERNEL_FILE_SET);
        assert_eq!(kernel.len(), 96);
        assert_eq!(u16::from_le_bytes([kernel[36], kernel[37]]), 0xfd86);
        assert_eq!(u32_at(&kernel, 12), 0x585d_6da8);
        assert_eq!(kernel[21], 137);
        let upc = upcase_map();
        let n = node("Héllo Wörld.txt", false, 5, (1_709_214_317, 370_000_000), 6, 1);
        let ours = finish_set(entry_set(&n, &upc), 5, 5);
        assert_sets_match(&ours, &kernel);
    }

    #[test]
    fn entry_set_matches_kernel_dir() {
        let kernel = hex(KERNEL_DIR_SET);
        assert_eq!(kernel.len(), 96);
        assert_eq!(u16::from_le_bytes([kernel[36], kernel[37]]), 0x8988);
        assert_eq!(u32_at(&kernel, 12), 0x0021_0000);
        assert_eq!(kernel[21], 0);
        let upc = upcase_map();
        let n = node("Dir_ÆØ", true, 4096, (157_766_400, 0), 7, 1);
        let ours = finish_set(entry_set(&n, &upc), 4096, 4096);
        assert_sets_match(&ours, &kernel);
    }

    #[test]
    fn set_bytes_matches_entry_set_length() {
        let upc = upcase_map();
        for len in [1, 15, 16, 30, 31, 255] {
            let n = node(&"a".repeat(len), false, 0, (0, 0), 0, 0);
            assert_eq!(set_bytes(&n), entry_set(&n, &upc).len() as u64, "name length {len}");
        }
    }

    // ───── names ─────

    #[test]
    fn name_length_limits() {
        let upc = upcase_map();
        assert!(validate(&"a".repeat(255), &mut HashSet::new(), &upc).is_ok());
        assert_rejected(&"a".repeat(256), "too long");
        assert_rejected("", "too long");
        assert_rejected(&"😀".repeat(128), "too long");
        let s = format!("{}a", "😀".repeat(127));
        assert_eq!(utf16(&s).len(), 255);
        assert!(validate(&s, &mut HashSet::new(), &upc).is_ok());
    }

    #[test]
    fn name_forbidden_characters() {
        for c in ['"', '*', '/', ':', '<', '>', '?', '\\', '|'] {
            assert_rejected(&format!("a{c}b"), "not allowed");
        }
        for c in 0u8..=0x1F {
            assert_rejected(&format!("a{}b", char::from(c)), "not allowed");
        }
    }

    #[test]
    fn name_unicode_accepted() {
        let upc = upcase_map();
        assert!(validate("naïve résumé 日本語", &mut HashSet::new(), &upc).is_ok());
    }

    #[test]
    fn name_case_insensitive_duplicates_rejected_per_dir() {
        let upc = upcase_map();
        let mut seen = HashSet::new();
        validate("README.txt", &mut seen, &upc).unwrap();
        let err = validate("readme.TXT", &mut seen, &upc).unwrap_err();
        assert!(format!("{err:#}").contains("differ only by case"));

        let mut seen = HashSet::new();
        validate("é.txt", &mut seen, &upc).unwrap();
        let err = validate("É.TXT", &mut seen, &upc).unwrap_err();
        assert!(format!("{err:#}").contains("differ only by case"));

        // A different directory has its own namespace.
        validate("README.txt", &mut HashSet::new(), &upc).unwrap();
    }

    // ───── geometry ─────

    #[test]
    fn geometry_rejects_bad_clusters_and_small_volumes() {
        for c in [0, 100, 256, 3000, 64 << 20] {
            assert!(geometry(1 << 30, c, 0).is_none(), "cluster {c}");
        }
        for v in [0, 512, 1 << 20, 4 << 20] {
            assert!(geometry(v, 512, 0).is_none(), "volume {v}");
            assert!(geometry(v, 4096, 0).is_none(), "volume {v}");
        }
        assert!(geometry(1 << 30, 512, 0).is_some());
        assert!(geometry(32 << 20, 512, 0).is_some());
        assert!(geometry(1 << 30, 32 << 20, 0).is_some());
    }

    #[test]
    fn geometry_invariants_hold_across_sweep() {
        let vols = [5u64 << 20, 8 << 20, 64 << 20, 1 << 30, 1 << 40, 64 << 40];
        let clusters = [512u64, 4096, 32 << 10, 128 << 10, 32 << 20];
        let mut some = 0;
        for &v in &vols {
            for &c in &clusters {
                let Some(g) = geometry(v, c, 0) else { continue };
                some += 1;
                let cc = g.cluster_count;
                assert_eq!(g.fat_off * SECTOR, 1 << 20);
                assert_eq!((g.heap_off * SECTOR) % ALIGN, 0);
                assert!(g.heap_off >= g.fat_off + g.fat_len);
                assert!(g.fat_len * SECTOR >= (cc + 2) * 4);
                assert!(g.heap_off * SECTOR + cc * c <= v, "vol {v} cluster {c}");
                assert!((16..=0xFFFF_FFF5).contains(&cc));
            }
        }
        assert!(some >= 15, "sweep produced only {some} geometries");
    }

    #[test]
    fn geometry_huge_volume() {
        let four_pib = 4u64 << 50;
        assert!(geometry(four_pib, 512, 0).is_none());
        assert!(geometry(four_pib, 32 << 20, 0).is_some());
    }

    // ───── planning ─────

    #[test]
    fn plan_lays_out_dirs_then_files() {
        let g = geometry(64 << 20, 4096, 0).unwrap();
        let mut nodes = sample_tree();
        let p = plan(&mut nodes, g, FatMode::Prefix).unwrap();
        assert_eq!(p.bitmap_clusters, 1);
        assert_eq!(p.upcase_clusters, 2);
        let base = 2 + p.bitmap_clusters + p.upcase_clusters;
        assert_eq!(p.root_cluster as u64, base);
        assert_eq!(nodes[0].first_cluster as u64, base);
        assert_eq!(nodes[0].clusters, 1);
        assert_eq!(nodes[3].first_cluster as u64, base + nodes[0].clusters);
        assert_eq!(nodes[3].clusters, 1);
        let big = &nodes[1];
        assert_eq!(big.clusters, 3);
        assert_eq!(big.first_cluster as u64, base + nodes[0].clusters + nodes[3].clusters);
        assert_eq!((nodes[2].first_cluster, nodes[2].clusters), (0, 0));
        assert_eq!(p.used_end, base + 5);
        assert_eq!(p.used_end, big.first_cluster as u64 + big.clusters);
    }

    #[test]
    fn plan_write_bytes_depend_on_fat_mode() {
        let g = geometry(64 << 20, 4096, 0).unwrap();
        let mut nodes = sample_tree();
        let prefix = plan(&mut nodes, g, FatMode::Prefix).unwrap();
        let full = plan(&mut nodes, g, FatMode::Full).unwrap();
        assert!(full.write_bytes > prefix.write_bytes);
        let used = prefix.used_end;
        assert_eq!(prefix.write_bytes, 24 * SECTOR + (used * 4).div_ceil(4096) * 4096 + (used - 2) * 4096);
    }

    #[test]
    fn plan_rejects_data_that_does_not_fit() {
        let g = geometry(64 << 20, 4096, 0).unwrap();
        let mut nodes = sample_tree();
        nodes[1].size = 64 << 20;
        let err = plan(&mut nodes, g, FatMode::Prefix).err().expect("should not fit");
        assert!(format!("{err:#}").contains("does not fit"));
    }

    #[test]
    fn dir_bytes_root_has_label_bitmap_upcase_entries() {
        let upc = upcase_map();
        let g = geometry(64 << 20, 4096, 0).unwrap();
        let mut nodes = sample_tree();
        let p = plan(&mut nodes, g, FatMode::Prefix).unwrap();
        let label = utf16("TEST");
        let out = dir_bytes(&nodes, 0, &p, &label, &upc);
        assert_eq!(out.len() as u64, nodes[0].clusters * g.cluster);

        assert_eq!((out[0], out[1]), (0x83, 4));
        let want: Vec<u8> = label.iter().flat_map(|c| c.to_le_bytes()).collect();
        assert_eq!(out[2..10], want[..]);

        assert_eq!(out[32], 0x81);
        assert_eq!(u32_at(&out, 32 + 20), 2);
        assert_eq!(u64_at(&out, 32 + 24), g.cluster_count.div_ceil(8));

        assert_eq!(out[64], 0x82);
        assert_eq!(u32_at(&out, 64 + 4), UPCASE_CHECKSUM);
        assert_eq!(u32_at(&out, 64 + 20) as u64, 2 + p.bitmap_clusters);
        assert_eq!(u64_at(&out, 64 + 24), 5836);

        assert_eq!(out[96], 0x85, "first child entry set");
    }

    // ───── mounts and sysfs ─────

    #[test]
    fn mounted_dev_matches_listed_devices() {
        let info = "22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
                    30 22 7:3 / /media/x rw - exfat /dev/loop3 rw\n";
        let devs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(mounted_dev(info, &devs(&["8:0", "8:1"])), Some(("8:1".into(), "/".into())));
        assert_eq!(mounted_dev(info, &devs(&["7:3"])), Some(("7:3".into(), "/media/x".into())));
        assert_eq!(mounted_dev(info, &devs(&["8:0"])), None);
        assert_eq!(mounted_dev("", &devs(&["8:0"])), None);
    }

    /// Read-only sysfs probe: the first disk that has a partition, as ("maj:min" of disk, "maj:min" of partition).
    fn partitioned_disk() -> Option<(String, String)> {
        for disk in fs::read_dir("/sys/block").ok()?.flatten().map(|e| e.path()) {
            let Ok(disk_dev) = fs::read_to_string(disk.join("dev")) else { continue };
            for sub in fs::read_dir(&disk).ok()?.flatten().map(|e| e.path()) {
                if !sub.join("partition").exists() {
                    continue;
                }
                if let Ok(part_dev) = fs::read_to_string(sub.join("dev")) {
                    return Some((disk_dev.trim().to_string(), part_dev.trim().to_string()));
                }
            }
        }
        None
    }

    #[test]
    fn related_devs_includes_disk_and_partition() {
        let Some((disk, part)) = partitioned_disk() else { return };
        let devs = related_devs(Path::new(SYS_BLOCK), &disk).unwrap();
        assert!(devs.contains(&disk), "{devs:?}");
        assert!(devs.contains(&part), "{devs:?}");
    }

    /// A fake /sys/dev/block: node "maj:min" dirs, partitions as subdirs with a `partition`
    /// file, holders as `holders/<name>/dev`.
    struct FakeSys(PathBuf);

    impl FakeSys {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("flashcp-fakesys-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            FakeSys(root)
        }
        fn node(&self, dev: &str) -> &Self {
            fs::create_dir_all(self.0.join(dev)).unwrap();
            fs::write(self.0.join(dev).join("dev"), format!("{dev}\n")).unwrap();
            self
        }
        fn partition(&self, disk: &str, name: &str, dev: &str) -> &Self {
            let p = self.0.join(disk).join(name);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("partition"), "1\n").unwrap();
            fs::write(p.join("dev"), format!("{dev}\n")).unwrap();
            self.node(dev)
        }
        fn holder(&self, dev: &str, name: &str, holder: &str) -> &Self {
            let p = self.0.join(dev).join("holders").join(name);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("dev"), format!("{holder}\n")).unwrap();
            self.node(holder)
        }
        fn related(&self, dev: &str) -> Result<Vec<String>> {
            related_devs(&self.0, dev).map(|mut v| {
                v.sort();
                v
            })
        }
    }

    impl Drop for FakeSys {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sorted(v: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn related_devs_follows_holders_of_partitions_recursively() {
        // sda → sda1 → LVM PV held by dm-0 → dm-crypt dm-1; sda2 plain; sda itself held by md0.
        let f = FakeSys::new("nested");
        f.node("8:0").partition("8:0", "sda1", "8:1").partition("8:0", "sda2", "8:2");
        f.holder("8:1", "dm-0", "253:0").holder("253:0", "dm-1", "253:1").holder("8:0", "md0", "9:0");
        assert_eq!(f.related("8:0").unwrap(), sorted(&["8:0", "8:1", "8:2", "253:0", "253:1", "9:0"]));
        // From the partition: only its own stack, not the sibling or the disk.
        assert_eq!(f.related("8:1").unwrap(), sorted(&["8:1", "253:0", "253:1"]));
        // The friendly mount check now sees a mounted dm-1 on top of sda1.
        let info = "40 1 253:1 / /mnt/secret rw - ext4 /dev/mapper/x rw\n";
        assert_eq!(mounted_dev(info, &f.related("8:0").unwrap()), Some(("253:1".into(), "/mnt/secret".into())));
    }

    #[test]
    fn related_devs_survives_cycles() {
        let f = FakeSys::new("cycle");
        f.node("8:0").partition("8:0", "sda1", "8:1");
        f.holder("8:1", "dm-0", "253:0").holder("253:0", "dm-1", "253:1");
        // Pathological: dm-1 claims to be held by the disk and by dm-0 again.
        f.holder("253:1", "sda", "8:0").holder("253:1", "dm-0", "253:0");
        assert_eq!(f.related("8:0").unwrap(), sorted(&["8:0", "8:1", "253:0", "253:1"]));
    }

    #[test]
    fn related_devs_fails_closed() {
        let f = FakeSys::new("closed");
        // Target node missing from sysfs: must be an error, not "nothing related".
        let e = f.related("8:16").unwrap_err();
        assert!(format!("{e:#}").contains("8:16"), "{e:#}");
        // A holder entry without a readable `dev` file is an error too.
        f.node("8:0");
        fs::create_dir_all(f.0.join("8:0/holders/dm-0")).unwrap();
        assert!(f.related("8:0").is_err());
        // An unreadable partition marker must not be taken as "not a partition".
        f.node("8:48").partition("8:48", "sdd1", "8:49");
        let part = f.0.join("8:48/sdd1");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&part, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = fs::symlink_metadata(part.join("partition")).is_err(); // false when running as root
        let r = f.related("8:48");
        fs::set_permissions(&part, fs::Permissions::from_mode(0o755)).unwrap();
        if denied {
            let e = r.unwrap_err();
            assert!(format!("{e:#}").contains("sdd1/partition"), "{e:#}");
        }
        // A node with no holders dir at all is fine.
        f.node("8:32");
        assert_eq!(f.related("8:32").unwrap(), sorted(&["8:32"]));
    }
}
