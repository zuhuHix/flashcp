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
use crate::sim::trace::{self, Op as TOp};
use crate::util::{AlignedBuf, human_bytes};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
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
    /// Volume serial number to write (default: derived from the current time).
    pub serial: Option<u32>,
    /// `optimize`'s job id, written after [`JOB_TAG`] into the boot code area of both boot
    /// sectors (covered by the boot checksum like any other byte; readers ignore it), so a
    /// half-written build is recognized as this very job's, not by the 32-bit serial alone.
    pub job: Option<[u8; 16]>,
}

/// Marks the job id in the boot code area (boot sector bytes 120..): `JOB_TAG` + 16 bytes.
pub(crate) const JOB_TAG: &[u8; 11] = b"flashcp-job";
/// Offset of [`JOB_TAG`] in the boot sector.
pub(crate) const JOB_TAG_OFF: usize = 120;

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
#[allow(clippy::too_many_arguments)]
fn boot_region(
    g: &Geometry,
    part_off: u64,
    vol_bytes: u64,
    root: u32,
    serial: u32,
    pct: u8,
    flags: u16,
    job: Option<&[u8; 16]>,
) -> Vec<u8> {
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
    if let Some(id) = job {
        b[JOB_TAG_OFF..JOB_TAG_OFF + JOB_TAG.len()].copy_from_slice(JOB_TAG);
        b[JOB_TAG_OFF + JOB_TAG.len()..JOB_TAG_OFF + JOB_TAG.len() + 16].copy_from_slice(id);
    }
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
    /// Writes in flight for scattered spans, read by the I/O thread ([`SeqWriter::set_inflight`]).
    inflight: Arc<AtomicUsize>,
}

/// A full buffer and its spans: (offset in buf, device offset, length).
type Batch = (AlignedBuf, Vec<(usize, u64, usize)>);

/// Writes in flight for scattered spans unless the caller sets it ([`SeqWriter::set_inflight`]).
const IO_WORKERS: usize = crate::devprofile::DEFAULT_WRITE_QD;
/// A span this big is written alone, with nothing else in flight: big writes are the
/// sequential stream, and two of them interleaved would make it two streams.
const SOLO_SPAN: usize = 1 << 20;

/// A batch's buffer, shared by the jobs that write its spans. When the last one is done (the
/// last Arc drops), the buffer goes back to the producer.
struct Shared {
    buf: Option<AlignedBuf>,
    back: Sender<AlignedBuf>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            let _ = self.back.send(b); // never blocks: that channel can hold every buffer there is
        }
    }
}

fn write_span(dev: &File, sh: &Shared, bo: usize, off: u64, len: usize) -> Result<()> {
    let buf = sh.buf.as_ref().expect("buffer present until drop").as_ref();
    dev.write_all_at(&buf[bo..bo + len], off).with_context(|| format!("write at {off}"))
}

/// One small span for the pool: (buffer, offset in it, device offset, length).
type Job = (Arc<Shared>, usize, u64, usize);
/// (jobs submitted and not finished, first error), its condvar, and "some job failed".
type PoolState = Arc<(Mutex<(usize, Option<anyhow::Error>)>, Condvar, AtomicBool)>;

/// A fixed set of threads writing small spans, `n` in flight. It is fed across buffer
/// boundaries: a buffer is cut at every 8 MB device boundary, so a scattered stream may hold
/// only a few dozen spans per buffer, and waiting for each buffer's last span before starting
/// the next one would leave the queue nearly empty at every boundary (worse the deeper it is).
struct Pool {
    tx: Option<Sender<Job>>,
    state: PoolState,
    workers: Vec<std::thread::JoinHandle<()>>,
}

/// Marks a pool job finished when dropped, also when the write panicked, so that
/// [`Pool::wait_idle`] can never hang.
struct Done<'a>(&'a PoolState, Option<Result<()>>);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        let r = self.1.take().unwrap_or_else(|| Err(anyhow::anyhow!("I/O worker panicked")));
        let (m, cv, failed) = &**self.0;
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        g.0 -= 1;
        if let Err(e) = r {
            failed.store(true, Relaxed);
            g.1.get_or_insert(e);
        }
        cv.notify_all();
    }
}

impl Pool {
    fn new(dev: Arc<File>, n: usize) -> Pool {
        let (tx, rx) = bounded::<Job>(n);
        let state: PoolState = Arc::new((Mutex::new((0, None)), Condvar::new(), AtomicBool::new(false)));
        let grp = trace::group();
        let workers = (0..n)
            .map(|_| {
                let (rx, dev, st) = (rx.clone(), dev.clone(), state.clone());
                std::thread::spawn(move || {
                    for (sh, bo, off, len) in rx {
                        let mut done = Done(&st, None);
                        // After a failure the remaining jobs are drained without writing.
                        done.1 = Some(if st.2.load(Relaxed) {
                            Ok(())
                        } else {
                            // seq = offset: spans are submitted in ascending order.
                            trace::io(TOp::Write, off, len as u64, n, grp, off, || write_span(&dev, &sh, bo, off, len))
                        });
                        drop(sh); // the buffer may go back before the job is marked done
                    }
                })
            })
            .collect();
        Pool { tx: Some(tx), state, workers }
    }
    fn submit(&self, sh: &Arc<Shared>, bo: usize, off: u64, len: usize) -> Result<()> {
        {
            let mut g = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = g.1.take() {
                return Err(e);
            }
            g.0 += 1;
        }
        let tx = self.tx.as_ref().expect("pool is open");
        tx.send((sh.clone(), bo, off, len)).map_err(|_| anyhow::anyhow!("I/O workers stopped"))
    }
    /// Waits until every submitted span has been written, then reports the first error.
    fn wait_idle(&self) -> Result<()> {
        let (m, cv, _) = &*self.state;
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        while g.0 > 0 {
            g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        g.1.take().map_or(Ok(()), Err)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        drop(self.tx.take());
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

/// The I/O thread. Big spans are written one at a time, in order; small spans go through the
/// pool, which keeps `inflight` of them going. A big span first waits for every small one
/// before it (the stream stays ascending), and the final sync waits for all of them, so
/// finish() returning Ok means every byte is on the device.
fn io_loop(dev: Arc<File>, rx: Receiver<Batch>, btx: Sender<AlignedBuf>, inflight: Arc<AtomicUsize>) -> Result<()> {
    let mut pool: Option<Pool> = None;
    for (buf, spans) in rx {
        let sh = Arc::new(Shared { buf: Some(buf), back: btx.clone() });
        for &(bo, off, len) in &spans {
            if spans.len() == 1 || len >= SOLO_SPAN {
                if let Some(p) = &pool {
                    p.wait_idle()?;
                }
                trace::serial(TOp::Write, off, len as u64, || write_span(&dev, &sh, bo, off, len))?;
            } else {
                let n = inflight.load(Relaxed).clamp(1, crate::devprofile::MAX_WRITE_QD);
                pool.get_or_insert_with(|| Pool::new(dev.clone(), n)).submit(&sh, bo, off, len)?;
            }
        }
    }
    if let Some(p) = &pool {
        p.wait_idle()?;
    }
    trace::sync(|| dev.sync_all())?;
    Ok(())
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
        let inflight = Arc::new(AtomicUsize::new(IO_WORKERS));
        let n = inflight.clone();
        let io = std::thread::spawn(move || io_loop(dev, rx, btx, n));
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
            inflight,
        }
    }
    /// Writes in flight for scattered spans (from the device profile's write_qd). Takes effect
    /// if called before the first scattered buffer is flushed.
    pub(crate) fn set_inflight(&mut self, n: usize) {
        self.inflight.store(n.clamp(1, crate::devprofile::MAX_WRITE_QD), Relaxed);
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
    /// Read a file straight into the I/O buffers (no intermediate copy). With `sum`, every byte
    /// read is also hashed (see [`crate::verify::BlockSum`]).
    pub(crate) fn write_from(
        &mut self,
        f: &mut File,
        mut left: u64,
        mut sum: Option<&mut crate::verify::BlockSum>,
    ) -> Result<()> {
        while left > 0 {
            let n = self.room().min(left as usize);
            let dst = &mut self.buf.as_mut()[self.fill..self.fill + n];
            f.read_exact(dst)?;
            if let Some(s) = sum.as_deref_mut() {
                s.update(dst);
            }
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
    /// Stops without writing what is still buffered; waits until the I/O thread has finished
    /// the writes already handed to it, so nothing reaches the device after this returns.
    pub(crate) fn abandon(mut self) {
        drop(self.tx.take());
        if let Some(h) = self.io.take() {
            let _ = h.join();
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
    let ok = want == fl || unsafe { libc::fcntl(fd, libc::F_SETFL, want) } == 0;
    if ok && want != fl {
        trace::direct(on);
    }
    ok
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
        let part_off = crate::sim::device::sim_device().map_or(0, |d| d.part_start_sectors);
        Ok(Target { file, bytes: md.len(), part_off, direct, _flags })
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
pub(crate) fn mounted_dev(mountinfo: &str, devs: &[String]) -> Option<(String, String)> {
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
    // Test hook: keep a sector-aligned but 4 KB-unaligned length, so tests can craft volumes
    // whose end is unaligned (other tools make them); such a build may extend an image.
    if std::env::var_os(UNALIGNED_ENV).is_some() {
        return bytes / SECTOR * SECTOR;
    }
    bytes / 4096 * 4096
}

const UNALIGNED_ENV: &str = "FLASHCP_TEST_UNALIGNED_VOLUME";

/// The target's absolute byte offset on its whole device (partition start), or 0.
pub(crate) fn device_offset(tgt: &Target) -> u64 {
    tgt.part_off * SECTOR
}

/// Clusters below 4 KB (512 B, 1 KB, 2 KB) may be chosen at all. They are valid exFAT, but
/// the choice depends on other systems accepting them (Windows chkdsk, macOS fsck_exfat; the
/// xos CI checks exactly that): if CI rejects them, set this to false.
const SMALL_CLUSTERS: bool = true;
/// Sub-4 KB clusters only on volumes up to this size, where FAT and bitmap stay small.
const SMALL_MAX_VOLUME: u64 = 32 << 30;

/// What the sub-4 KB gate looks at, measured on the plan at 4 KB clusters.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClusterStats {
    pub(crate) vol_bytes: u64,
    /// Heap clusters the 4 KB plan uses (u64::MAX if the data does not even fit at 4 KB).
    pub(crate) used_4k: u64,
    pub(crate) heap_4k: u64,
    /// File data bytes, and the cluster slack those files have at 4 KB.
    pub(crate) data: u64,
    pub(crate) slack_4k: u64,
}

/// Whether sub-4 KB clusters are worth considering; Err says why not. All three must hold:
/// (a) flashcp fills the volume: the 4 KB plan uses ≥ 50 % of the heap. Little free space is
///     left for other systems to write big files into, which tiny clusters would slow down.
/// (b) the data is dominated by tiny files: the slack at 4 KB is ≥ 15 % of the data bytes.
///     Slack is exactly what smaller clusters save (bytes written and space); a median file
///     size would ignore how the bytes are distributed (a few big files make tiny clusters
///     pointless however many tiny ones there are).
/// (c) the volume is ≤ 32 GB, so FAT and bitmap (8× larger at 512 B) stay small.
pub(crate) fn small_clusters_gate(s: &ClusterStats) -> std::result::Result<String, String> {
    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 };
    if !SMALL_CLUSTERS {
        return Err("sub-4K disabled".into());
    }
    if s.vol_bytes > SMALL_MAX_VOLUME {
        return Err("sub-4K: volume > 32 GB".into());
    }
    let full = if s.used_4k == u64::MAX { 100.0 } else { pct(s.used_4k, s.heap_4k) };
    if full < 50.0 {
        return Err(format!("sub-4K: data fills {full:.0} % < 50 %"));
    }
    let slack = pct(s.slack_4k, s.data);
    if s.data == 0 || slack < 15.0 {
        return Err(format!("sub-4K: slack at 4K is {slack:.0} % of data < 15 %"));
    }
    Ok(format!("sub-4K allowed: data fills {full:.0} %, slack at 4K {slack:.0} % of data"))
}

/// Estimated seconds to write plan `p`: its bytes at sequential speed plus one request per
/// skip (Prefix FAT mode skips once, from the FAT's used prefix to the heap).
pub(crate) fn plan_cost(p: &Plan, fat: FatMode, bps: f64, skip_s: f64) -> f64 {
    let skips = if fat == FatMode::Prefix { 1.0 } else { 0.0 };
    p.write_bytes as f64 / bps + skips * skip_s
}

/// Cluster-size optimizer: plans every candidate cluster size and returns the cheapest plan
/// (with `nodes` laid out for it) and why it was chosen. Cost is estimated time, from the
/// target device's cached profile (default constants otherwise): written bytes (FAT + bitmap +
/// per-file slack) at sequential speed plus per-request costs. Small files want small
/// clusters; huge drives in Full FAT mode want bigger ones. Candidates are 4 KB … 128 KB, and
/// 512 B … 2 KB too when [`small_clusters_gate`] allows. An explicit `o.cluster` is the only
/// candidate.
pub(crate) fn choose_cluster(nodes: &mut [Node], tgt: &Target, o: &BuildOpts) -> Result<(Plan, String)> {
    let vol_bytes = usable_bytes(tgt.bytes);
    let dev_off = device_offset(tgt);
    if let Some(c) = o.cluster {
        ensure!(valid_cluster(c as u64), "cluster size must be a power of two between 512 bytes and 32 MB, got {c}");
        let g = geometry(vol_bytes, c as u64, dev_off).context("data does not fit, or volume size is unsupported")?;
        return Ok((plan(nodes, g, o.fat)?, "set by --cluster".into()));
    }
    let profile = crate::devprofile::block_dev_of(&tgt.file).and_then(crate::devprofile::load);
    let (bps, skip_s) = profile.clone().unwrap_or_default().write_cost();
    let model = if profile.is_some_and(|p| p.has_write_model()) { "cached profile" } else { "default model" };
    let g4 = geometry(vol_bytes, 4096, dev_off);
    let used_4k = g4.and_then(|g| plan(nodes, g, o.fat).ok()).map_or(u64::MAX, |p| p.used_end - 2);
    let files = nodes.iter().filter(|n| !n.is_dir);
    let stats = ClusterStats {
        vol_bytes,
        used_4k,
        heap_4k: g4.map_or(0, |g| g.cluster_count),
        data: files.clone().map(|n| n.size).sum(),
        slack_4k: files.map(|n| n.size.div_ceil(4096) * 4096 - n.size).sum(),
    };
    let gate = small_clusters_gate(&stats);
    let low = if gate.is_ok() { 9 } else { 12 };
    // Lifetime cap: the cluster size outlives this build. On a small removable stick every
    // later add and pull pays for big clusters (slack written and read per file), so even a
    // tree of big files never gets more than 4 KB there.
    let capped = vol_bytes <= LIFETIME_CAP_BYTES && is_removable(&tgt.file);
    let high = if capped { 12 } else { 17 };
    let mut best: Option<(f64, Plan)> = None;
    for c in (low..=high).map(|s| 1u64 << s) {
        let Some(g) = geometry(vol_bytes, c, dev_off) else { continue };
        let Ok(p) = plan(nodes, g, o.fat) else { continue };
        let cost = plan_cost(&p, o.fat, bps, skip_s);
        if best.as_ref().is_none_or(|b| cost < b.0) {
            best = Some((cost, p));
        }
    }
    let Some((cost, best)) = best else { bail!("data does not fit, or volume size is unsupported") };
    let why = format!(
        "auto: est. {cost:.1} s, {model}; {}{}",
        gate.unwrap_or_else(|e| e),
        if capped { "; ≤ 4 KB on a small removable drive" } else { "" }
    );
    Ok((plan(nodes, best.g, o.fat)?, why)) // re-plan so nodes hold the winner's layout
}

/// Removable drives up to this size never get clusters above 4 KB (see [`choose_cluster`]).
const LIFETIME_CAP_BYTES: u64 = 64 << 30;

/// Whether `f` is a block device on a removable or USB disk (sysfs `removable`, or the
/// usb-storage / uas driver: many USB sticks and card readers report removable = 0).
fn is_removable(f: &File) -> bool {
    if let Some(d) = crate::sim::device::sim_device() {
        return d.removable;
    }
    let Some(dev) = crate::devprofile::block_dev_of(f) else { return false };
    let info = crate::probe::device_info_of(dev);
    let usb = matches!(info.transport.as_deref(), Some("usb-storage" | "uas"));
    usb || info.sysfs.is_some_and(|p| fs::read_to_string(p.join("removable")).is_ok_and(|r| r.trim() == "1"))
}

/// What `optimize` adds to a build ([`build_target_guarded`]).
pub(crate) struct Guard<'a> {
    /// Per node (same index as the nodes): the (size, [`crate::verify::BlockSum`] hash) every
    /// file's content must have; directory entries are ignored.
    pub(crate) expect: Vec<(u64, u128)>,
    /// Called after each file was handed to the device stream, with the number so far (a
    /// test hook).
    pub(crate) after_file: Option<Box<dyn FnMut(usize) + 'a>>,
}

/// A file read for a guarded build is not what it must be (see [`Guard::expect`]).
#[derive(Debug)]
pub(crate) struct SourceChanged(pub(crate) PathBuf);

impl std::fmt::Display for SourceChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} does not hold what was verified (its contents changed); the build was stopped", self.0.display())
    }
}

impl std::error::Error for SourceChanged {}

/// [`build`] of an already scanned tree onto an already opened (exclusively held) target.
pub(crate) fn build_target(nodes: Vec<Node>, tgt: Target, o: &BuildOpts, sink: Sink<'_>) -> Result<BuildStats> {
    build_target_guarded(nodes, tgt, o, sink, None)
}

/// [`build_target`], and with a [`Guard`] (`optimize`):
/// - every file is hashed AS IT IS READ for the device stream (the very bytes written) and
///   must match `guard.expect`; on a mismatch the build stops at once with [`SourceChanged`],
///   writing nothing more (the volume stays unmountable, see below);
/// - the main boot region is written LAST. First the backup boot region (our serial, but its
///   checksum sector zeroed, so no fsck can "restore" a half-written volume from it), sync,
///   then the main boot region zeroed, sync, then everything else, sync, then the valid
///   backup, sync, then the main boot region, sync. An interrupted build is never a volume
///   anything mounts; `optimize` still recognizes it by the serial, job id and geometry in the backup.
///
/// Without a guard (plain `build`) nothing is hashed, and the main boot sector says
/// VolumeDirty until the end instead.
pub(crate) fn build_target_guarded(
    mut nodes: Vec<Node>,
    tgt: Target,
    o: &BuildOpts,
    sink: Sink<'_>,
    mut guard: Option<Guard<'_>>,
) -> Result<BuildStats> {
    if let Some(g) = &guard {
        ensure!(g.expect.len() == nodes.len(), "internal error: expected hashes do not match the tree");
    }
    let t0 = Instant::now();
    let mut tick = Ticker::new(sink);
    let upc = upcase_map();
    let label: Vec<u16> = o.label.encode_utf16().collect();
    ensure!(label.len() <= 11, "label is at most 11 characters");
    let (p, why) = choose_cluster(&mut nodes, &tgt, o)?;
    let vol_bytes = usable_bytes(tgt.bytes);
    let g = p.g;
    let cs = g.cluster;
    let files = nodes.iter().filter(|n| !n.is_dir).count();
    let dirs = nodes.len() - files;
    let data: u64 = nodes.iter().map(|n| n.size).sum();
    let detail = format!(
        "plan: {} files, {} dirs, {} data │ cluster {} ({why}) │ {} clusters │ FAT {} ({:?}) │ will write {}",
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
        sync: None,
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
    // Writes what is queued (so the device really holds a partial volume), then dies.
    let abort_if_due = |w: SeqWriter| -> SeqWriter {
        if abort_at.is_some_and(|a| w.written >= a) {
            let _ = w.finish();
            std::process::abort();
        }
        w
    };
    let serial = match o.serial {
        Some(s) => s,
        None => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u32,
    };
    let pct = ((p.used_end - 2) * 100 / g.cluster_count).min(100) as u8;

    // 1. Boot region + backup. Written and synced BEFORE anything else, so no device write
    // cache can store the new FAT/heap while the old (clean) boot sector still says the old
    // volume is intact.
    let boot = boot_region(&g, tgt.part_off, vol_bytes, p.root_cluster, serial, pct, 0, o.job.as_ref());
    let bl = boot.len();
    trace::phase("boot");
    let mut head = AlignedBuf::new(2 * bl);
    if guard.is_some() {
        // Boot last (see build_target_guarded): the backup first (checksum sector zeroed),
        // the old main region kept until that is on the medium, then the main region zeroed.
        trace::serial(TOp::Read, 0, head.as_ref().len() as u64, || dev.read_exact_at(head.as_mut(), 0))
            .context("read boot regions")?;
        head.as_mut()[bl..bl + 11 * SECTOR as usize].copy_from_slice(&boot[..11 * SECTOR as usize]);
        head.as_mut()[bl + 11 * SECTOR as usize..].fill(0);
        trace::serial(TOp::Write, 0, head.as_ref().len() as u64, || dev.write_all_at(head.as_ref(), 0))
            .context("write backup boot region")?;
        trace::sync(|| dev.sync_all()).context("sync backup boot region")?;
        head.as_mut()[..bl].fill(0);
        trace::serial(TOp::Write, 0, head.as_ref().len() as u64, || dev.write_all_at(head.as_ref(), 0))
            .context("clear main boot region")?;
        trace::sync(|| dev.sync_all()).context("sync boot regions")?;
    } else {
        // The main boot sector says VolumeDirty until the very end, so a build that is
        // interrupted leaves a volume every checker sees as unfinished.
        let dirty_boot =
            boot_region(&g, tgt.part_off, vol_bytes, p.root_cluster, serial, pct, FLAG_VOLUME_DIRTY, o.job.as_ref());
        head.as_mut()[..bl].copy_from_slice(&dirty_boot);
        head.as_mut()[bl..].copy_from_slice(&boot);
        trace::serial(TOp::Write, 0, head.as_ref().len() as u64, || dev.write_all_at(head.as_ref(), 0))
            .context("write boot regions")?;
        trace::sync(|| dev.sync_all()).context("sync boot regions")?;
    }
    trace::phase("stream");
    w.skip_to(head.as_ref().len() as u64)?;
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
        let want = guard.as_ref().map(|g| g.expect[i]);
        let changed = || anyhow::Error::new(SourceChanged(n.src.clone()));
        match item {
            Item::Data(v) => {
                ensure!(v.len() as u64 == n.size, "{} changed size while copying", n.src.display());
                if let Some(want) = want {
                    let mut s = crate::verify::BlockSum::default();
                    s.update(&v);
                    if s.finish() != want {
                        w.abandon(); // not one byte of it reaches the device
                        return Err(changed());
                    }
                }
                w.write(&v)?;
                release(&budget, n.size.min(256 << 20));
            }
            Item::Stream => {
                let mut f = File::open(&n.src)?;
                crate::util::fadvise_sequential(&f);
                let mut sum = want.map(|_| crate::verify::BlockSum::default());
                w.write_from(&mut f, n.size, sum.as_mut())?;
                if let (Some(want), Some(s)) = (want, sum)
                    && s.finish() != want
                {
                    w.abandon(); // nothing more is written (the main boot region is still zeros)
                    return Err(changed());
                }
            }
        }
        w.zeros_to(at + n.clusters * cs)?;
        bytes_done += n.size;
        tick.progress((empty_files + k + 1) as u64, bytes_done, false);
        if let Some(f) = guard.as_mut().and_then(|g| g.after_file.as_mut()) {
            f(k + 1);
        }
        w = abort_if_due(w);
    }
    let written = w.finish()?; // syncs
    trace::phase("boot-final");
    if guard.is_some() {
        // Everything is on the device: the valid backup boot region, then the main one.
        head.as_mut()[..bl].fill(0);
        head.as_mut()[bl..].copy_from_slice(&boot);
        trace::serial(TOp::Write, 0, head.as_ref().len() as u64, || dev.write_all_at(head.as_ref(), 0))
            .context("write backup boot region")?;
        trace::sync(|| dev.sync_all()).context("sync backup boot region")?;
        head.as_mut()[..bl].copy_from_slice(&boot);
        trace::serial(TOp::Write, 0, head.as_ref().len() as u64, || dev.write_all_at(head.as_ref(), 0))
            .context("write boot region")?;
        trace::sync(|| dev.sync_all()).context("sync boot region")?;
    } else {
        // Everything is on the device: now the main boot sector may say "clean".
        let mut head = AlignedBuf::new(4096);
        head.as_mut()[..4096].copy_from_slice(&boot[..4096]);
        trace::serial(TOp::Write, 0, head.as_ref().len() as u64, || dev.write_all_at(head.as_ref(), 0))
            .context("write boot sector")?;
        trace::sync(|| dev.sync_all()).context("sync boot sector")?;
    }
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

    /// Scattered small spans (the pool) across many buffers, a big solo span in between, then
    /// more small ones: every byte lands where it belongs and nothing else is written.
    #[test]
    fn seq_writer_scattered_spans_land_exactly() {
        let dir = std::env::temp_dir().join(format!("flashcp-seqw-scatter-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.img");
        let f = File::create(&p).unwrap();
        f.set_len(64 << 20).unwrap();
        for qd in [1usize, 3, 32] {
            let dev = Arc::new(OpenOptions::new().read(true).write(true).open(&p).unwrap());
            dev.write_all_at(&vec![0xEEu8; 64 << 20], 0).unwrap();
            let mut w = SeqWriter::with_align(dev.clone(), 1);
            w.set_inflight(qd);
            let mut want = vec![0xEEu8; 64 << 20];
            let mut put = |w: &mut SeqWriter, at: u64, len: usize, v: u8| {
                w.skip_to(at).unwrap();
                w.write(&vec![v; len]).unwrap();
                want[at as usize..at as usize + len].fill(v);
            };
            for k in 0..1500u64 {
                put(&mut w, k * 12288, 4096 + (k as usize % 3) * 1000, (k % 251) as u8);
            }
            put(&mut w, 20 << 20, 3 << 20, 0x77); // solo: waits for the small ones
            for k in 0..900u64 {
                put(&mut w, (24 << 20) + k * 40960, 8192, (k % 7) as u8 + 1);
            }
            w.finish().unwrap();
            let mut got = vec![0u8; 64 << 20];
            dev.read_exact_at(&mut got, 0).unwrap();
            assert!(got == want, "qd {qd}: device content differs");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Scattered spans to a target that rejects every write: the pool's error must come back
    /// from write()/finish() with the original message, never a hang.
    #[test]
    fn seq_writer_scattered_failure_does_not_hang() {
        let dir = std::env::temp_dir().join(format!("flashcp-seqw-scatter-ro-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ro.img");
        File::create(&p).unwrap().set_len(256 << 20).unwrap();
        let ro = Arc::new(File::open(&p).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut w = SeqWriter::with_align(ro, 1);
            w.set_inflight(8);
            let chunk = vec![0xA5u8; 8192];
            let mut res = Ok(());
            for k in 0..20_000u64 {
                res = w.skip_to(k * 12288).and_then(|_| w.write(&chunk));
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

    // ───── cluster-size policy ─────

    fn stats(vol_gb: u64, used_pct: u64, slack_pct: u64) -> ClusterStats {
        ClusterStats {
            vol_bytes: vol_gb << 30,
            used_4k: used_pct * 1000,
            heap_4k: 100_000,
            data: 1_000_000,
            slack_4k: slack_pct * 10_000,
        }
    }

    #[test]
    fn small_clusters_only_when_full_tiny_and_small() {
        assert!(small_clusters_gate(&stats(8, 50, 15)).is_ok(), "all three hold at the limits");
        assert!(small_clusters_gate(&stats(32, 90, 80)).is_ok());
        // (a) fails: the volume stays mostly empty.
        assert!(small_clusters_gate(&stats(8, 49, 80)).unwrap_err().contains("fills 49 %"));
        // (b) fails: the data is not tiny files.
        assert!(small_clusters_gate(&stats(8, 90, 14)).unwrap_err().contains("slack"));
        // (c) fails: big volume.
        assert!(small_clusters_gate(&stats(33, 90, 80)).unwrap_err().contains("32 GB"));
        // No data at all, and data that does not even fit at 4 KB.
        assert!(small_clusters_gate(&ClusterStats { data: 0, slack_4k: 0, ..stats(8, 90, 0) }).is_err());
        assert!(small_clusters_gate(&ClusterStats { used_4k: u64::MAX, ..stats(8, 0, 50) }).is_ok());
    }

    /// The optimizer on plan statistics alone: tiny files filling a small volume get a sub-4K
    /// cluster; the same files on a volume 64× bigger get 4 KB.
    #[test]
    fn choose_cluster_goes_below_4k_only_when_the_gate_allows() {
        let tree = |n: usize| {
            let mut v = vec![node("", true, 0, (0, 0), 0, 0)];
            v[0].children = (1..=n).collect();
            v.extend((0..n).map(|i| node(&format!("f{i}"), false, 300 + (i as u64 % 700), (0, 0), 0, 0)));
            v
        };
        let dir = std::env::temp_dir().join(format!("flashcp-choose-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let o = BuildOpts {
            cluster: None,
            label: String::new(),
            fat: FatMode::Prefix,
            readers: 1,
            dry_run: true,
            serial: None,
            job: None,
        };
        let pick = |bytes: u64, n: usize| {
            let p = dir.join("i.img");
            let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
            f.set_len(bytes).unwrap();
            let tgt = Target { _flags: FlagGuard::new(&f).unwrap(), file: f, bytes, part_off: 0, direct: false };
            let (plan, why) = choose_cluster(&mut tree(n), &tgt, &o).unwrap();
            (plan.g.cluster, why)
        };
        let (c, why) = pick(16 << 20, 2500);
        assert!(c < 4096, "full volume of tiny files: {c} ({why})");
        let (c, why) = pick(1 << 30, 2500);
        assert_eq!(c, 4096, "mostly empty volume: {why}");
        let o = BuildOpts { cluster: Some(65536), ..o.clone() };
        let f = File::create(dir.join("j.img")).unwrap();
        f.set_len(16 << 20).unwrap();
        let tgt = Target { _flags: FlagGuard::new(&f).unwrap(), file: f, bytes: 16 << 20, part_off: 0, direct: false };
        let (plan, why) = choose_cluster(&mut tree(10), &tgt, &o).unwrap();
        assert_eq!((plan.g.cluster, why.as_str()), (65536, "set by --cluster"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plan_cost_counts_bytes_and_the_prefix_skip() {
        let mut nodes = sample_tree();
        let g = geometry(64 << 20, 4096, 0).unwrap();
        let p = plan(&mut nodes, g, FatMode::Prefix).unwrap();
        let full = plan_cost(&p, FatMode::Full, 1e6, 0.5);
        assert!((full - p.write_bytes as f64 / 1e6).abs() < 1e-9);
        assert!((plan_cost(&p, FatMode::Prefix, 1e6, 0.5) - full - 0.5).abs() < 1e-9);
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
        let r = boot_region(&g, 0, 32 << 20, 5, 0x1234, 0, 0, None);
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
        assert!(r[120..510].iter().all(|&b| b == 0xF4));
        // A job id goes into the boot code area, and the checksum covers it.
        let id = [0xA5u8; 16];
        let t = boot_region(&g, 0, 32 << 20, 5, 0x1234, 0, 0, Some(&id));
        assert_eq!(&t[JOB_TAG_OFF..JOB_TAG_OFF + 11], JOB_TAG);
        assert_eq!(&t[JOB_TAG_OFF + 11..JOB_TAG_OFF + 27], &id);
        assert!(t[JOB_TAG_OFF + 27..510].iter().all(|&b| b == 0xF4));
        let tc = checksum32(&t[..11 * 512], &[106, 107, 112], 0);
        assert_ne!(tc, c);
        assert!(t[11 * 512..].as_chunks::<4>().0.iter().all(|w| *w == tc.to_le_bytes()));
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

    // ───── device offsets, flush boundaries, usable size ─────

    const LAYOUT_VOLS: [u64; 4] = [32 << 20, 64 << 20, 1 << 30, 3 << 30];
    const LAYOUT_CLUSTERS: [u64; 4] = [512, 4096, 32 << 10, 128 << 10];

    #[test]
    fn geometry_heap_is_4mib_aligned_on_the_device() {
        // The partition starts at 1 MiB: the heap must sit on a 4 MiB boundary of the device.
        let dev_off = 1 << 20;
        let mut checked = 0;
        for &vol in &LAYOUT_VOLS {
            for &c in &LAYOUT_CLUSTERS {
                let Some(g) = geometry(vol, c, dev_off) else { continue };
                checked += 1;
                assert_eq!((g.heap_off * SECTOR + dev_off) % ALIGN, 0, "vol {vol} cluster {c}");
            }
        }
        assert!(checked >= 8, "only {checked} geometries were checked");
    }

    #[test]
    fn geometry_heap_is_4mib_aligned_for_whole_device_images() {
        let mut checked = 0;
        for &vol in &LAYOUT_VOLS {
            for &c in &LAYOUT_CLUSTERS {
                let Some(g) = geometry(vol, c, 0) else { continue };
                checked += 1;
                assert_eq!((g.heap_off * SECTOR) % ALIGN, 0, "vol {vol} cluster {c}");
            }
        }
        assert!(checked >= 8, "only {checked} geometries were checked");
    }

    #[test]
    fn geometry_ignores_device_offsets_that_are_not_4k_multiples() {
        let mut checked = 0;
        for &vol in &LAYOUT_VOLS {
            for &c in &LAYOUT_CLUSTERS {
                let Some(base) = geometry(vol, c, 0) else { continue };
                let Some(odd) = geometry(vol, c, (1 << 20) + 512) else { continue };
                checked += 1;
                assert_eq!(
                    (odd.fat_off, odd.fat_len, odd.heap_off, odd.cluster_count),
                    (base.fat_off, base.fat_len, base.heap_off, base.cluster_count),
                    "vol {vol} cluster {c}"
                );
            }
        }
        assert!(checked >= 8, "only {checked} geometries were checked");
    }

    #[test]
    fn usable_bytes_rounds_down_to_4k() {
        assert_eq!(usable_bytes(0), 0);
        assert_eq!(usable_bytes(4095), 0);
        assert_eq!(usable_bytes(4096), 4096);
        assert_eq!(usable_bytes(64 << 20), 64 << 20);
        assert_eq!(usable_bytes((64 << 20) + 512), 64 << 20);
        assert_eq!(usable_bytes((64 << 20) + 1000), 64 << 20);
        assert_eq!(usable_bytes((64 << 20) + 4095), 64 << 20);
        for b in [1u64, 511, 512, 4097, 12_345_678] {
            let u = usable_bytes(b);
            assert_eq!(u % 4096, 0, "{b}");
            assert!(u <= b && b - u < 4096, "{b}");
        }
    }

    /// Deterministic byte source for the SeqWriter tests (xorshift64).
    fn next_rand(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn seqw_temp(tag: &str) -> (PathBuf, File) {
        let p = std::env::temp_dir().join(format!("flashcp-seqw-{tag}-{}", std::process::id()));
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        (p, f)
    }

    #[test]
    fn seq_writer_device_offset_keeps_stream_intact() {
        let (p, f) = seqw_temp("devoff");
        let mut w = SeqWriter::new(Arc::new(f));
        w.set_device_offset(1 << 20);
        let mut rng = 0x2545_F491_4F6C_DD1D_u64;
        let mut expect: Vec<u8> = Vec::new();
        // Mixed write() sizes from 1 byte to 300 KB, so flushes land on every kind of boundary.
        while expect.len() < 20 << 20 {
            let n = 1 + (next_rand(&mut rng) % 300_000) as usize;
            let chunk: Vec<u8> = (0..n).map(|_| next_rand(&mut rng) as u8).collect();
            w.write(&chunk).unwrap();
            expect.extend_from_slice(&chunk);
        }
        // Then a stream piece read straight from a file (write_from), across boundaries too.
        let (src_p, mut src_w) = seqw_temp("devoff-src");
        let piece: Vec<u8> = (0..(9 << 20) + 777).map(|_| next_rand(&mut rng) as u8).collect();
        std::io::Write::write_all(&mut src_w, &piece).unwrap();
        drop(src_w);
        let mut src = File::open(&src_p).unwrap();
        w.write_from(&mut src, piece.len() as u64, None).unwrap();
        expect.extend_from_slice(&piece);
        assert_eq!(w.cur(), expect.len() as u64);
        w.finish().unwrap();

        let got = fs::read(&p).unwrap();
        assert!(got.len() >= expect.len(), "file is shorter than the stream");
        assert!(got[..expect.len()] == expect[..], "stream content differs");
        assert!(got[expect.len()..].iter().all(|&b| b == 0), "padding after the stream is not zero");
        fs::remove_file(&p).unwrap();
        fs::remove_file(&src_p).unwrap();
    }

    #[test]
    fn seq_writer_flushes_on_absolute_device_multiples() {
        // Stream offset 0 is device byte 1 MiB, so the first flush boundary is at stream 7 MiB.
        let (p, f) = seqw_temp("bounds");
        let mut w = SeqWriter::new(Arc::new(f));
        w.set_device_offset(1 << 20);
        w.write(&vec![1u8; 7 << 20]).unwrap();
        assert_eq!(w.written, 7 << 20, "flush at device 8 MiB");
        w.write(&vec![2u8; (8 << 20) - 1]).unwrap();
        assert_eq!(w.written, 7 << 20, "no flush before device 16 MiB");
        w.write(&[3u8]).unwrap();
        assert_eq!(w.written, 15 << 20, "flush at device 16 MiB");
        w.finish().unwrap();
        let got = fs::read(&p).unwrap();
        assert_eq!(got.len() as u64, 15 << 20);
        assert!(got[..7 << 20].iter().all(|&b| b == 1));
        assert!(got[7 << 20..(15 << 20) - 1].iter().all(|&b| b == 2));
        assert_eq!(got[(15 << 20) - 1], 3);
        fs::remove_file(&p).unwrap();
    }
}
