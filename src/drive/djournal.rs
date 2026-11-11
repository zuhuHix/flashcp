//! The drive journal: a permanent file `/.flashcp-drive-journal` (Hidden | System | Archive,
//! contiguous) that holds the journal of the commit in flight.
//!
//! Layout: a 512-byte header (magic, sequence number, state, body length and hash, header
//! hash), then the body (zstd): the volume's serial and geometry, every journaled write of the
//! commit (phase, device offset, bytes before, bytes after) and samples (offset, length, hash)
//! of the data the commit wrote into free clusters.
//!
//! A commit writes body and header (state ACTIVE) together and syncs before its first
//! metadata write, and marks the header IDLE (one sector) after its last. On mount:
//! - no journal file, or IDLE: nothing to do (a dirty volume is then someone else's: refused);
//! - ACTIVE but the body does not match its hash: the journal write itself was torn, so no
//!   metadata write of that commit happened: treated as IDLE;
//! - ACTIVE and whole: the serial and geometry must match, every data sample must still hash
//!   the same, and every 512-byte sector the writes touch must hold one of the states the
//!   writes take it through (the shared check of `flashcp sync`'s journal); then every write
//!   is replayed in order (a sync per phase) and the header marked IDLE. If anything does not
//!   match, the volume changed since: nothing is written and the mount is refused.
//!
//! The file is created once, by the first commit on a volume: its header (IDLE) into free
//! clusters, a sync, its bitmap bits, a sync, then its entry set inside one sector of the root
//! (padded with deleted entries to stay in one sector), a sync. A crash in between leaks the
//! clusters (fsck.exfat accepts that); nothing references them.

use super::commit::{JW, PHASES};
use super::dev::Dev;
use crate::volume::{Child, Vol, le32, le64};
use anyhow::{Context, Result, bail, ensure};

pub(crate) const NAME: &str = ".flashcp-drive-journal";
pub(crate) const ATTRS: u16 = super::set::ATTR_HIDDEN | super::set::ATTR_SYSTEM | super::set::ATTR_ARCHIVE;
/// The largest journal body a commit writes (compressed) and decodes to (raw): a commit is
/// split until it fits both, and recovery refuses anything bigger before allocating for it.
pub(crate) const MAX_BODY: u64 = 8 << 20;
pub(crate) const MAX_RAW: usize = 16 << 20;
/// A data sample is at most this long (commits sample ≤ 512 bytes), and all of them together
/// at most [`MAX_SAMPLED`]: checked before any is read.
const MAX_SAMPLE: u64 = 4096;
const MAX_SAMPLED: u64 = 32 << 20;

/// Bytes the encoded (uncompressed) body of `writes` and `samples` takes.
pub(crate) fn raw_len(v: &Vol, writes: &[JW], samples: usize) -> usize {
    let geom = ident(v).1.len();
    4 + 4 + 2 + geom + 4 + writes.iter().map(|w| 13 + 2 * w.post.len()).sum::<usize>() + 4 + 20 * samples
}

/// Test hook: the size of a journal file created now.
pub(crate) const JOURNAL_BYTES_ENV: &str = "FLASHCP_TEST_JOURNAL_BYTES";
const MAGIC: &[u8; 16] = b"FLASHCP-DRIVEJ1\0";
const STATE_IDLE: u8 = 0;
const STATE_ACTIVE: u8 = 1;
const BODY_MAGIC: &[u8; 4] = b"DJB1";

/// Where the journal file is: contiguous clusters [first, first + clusters).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) first: u32,
    pub(crate) len: u64,
    pub(crate) nfc: bool,
}

pub(crate) fn is_journal(ch: &Child) -> bool {
    !ch.is_dir && ch.attrs & 0x06 == 0x06 && ch.name.iter().copied().eq(NAME.encode_utf16())
}

/// Names users may not create in the root: the drive journal's and sync's (any case).
pub(crate) fn reserved_name(key: &[u16], upc: &[u16]) -> bool {
    [NAME, crate::volume::SYNC_JOURNAL_NAME].iter().any(|n| {
        let k: Vec<u16> = n.encode_utf16().map(|c| upc[c as usize]).collect();
        k == key
    })
}

/// For `add` and `sync` on a volume drive mode used: refuses while a commit of `flashcp
/// mount` is pending (only a mount replays it), and hides the journal file from their view of
/// the root (it is never a user file; its clusters stay allocated).
pub(crate) fn hide_from(v: &mut Vol) -> Result<()> {
    let Some(root) = v.dirs.first_mut() else { return Ok(()) };
    let key = root.children.iter().find(|(_, c)| is_journal(c)).map(|(k, _)| k.clone());
    let Some(key) = key else { return Ok(()) };
    let c = root.children.remove(&key).unwrap();
    let p = Place { first: c.first, len: c.len, nfc: c.no_fat_chain };
    ensure!(
        header_state(v, &p) != Some(true),
        "the volume holds an interrupted commit of flashcp mount; mount it with `flashcp mount` once (that \
         recovers it), then retry (nothing was written)"
    );
    Ok(())
}

pub(crate) fn find(root: &[Child]) -> Option<Place> {
    root.iter().find(|c| is_journal(c)).map(|c| Place { first: c.first, len: c.len, nfc: c.no_fat_chain })
}

/// Finds the journal file by scanning the raw root directory (no validation of other sets,
/// which an interrupted commit may have left torn: the journal's own set is never rewritten).
pub(crate) fn locate_raw(v: &mut Vol, max_root: u64) -> Result<Option<Place>> {
    let root = le32(&v.sector0, 96);
    let cl = v.chain(root, "root directory", max_root / v.cs)?;
    let data = v.read_clusters(&cl, cl.len() as u64 * v.cs)?;
    let name: Vec<u16> = NAME.encode_utf16().collect();
    let want = super::set::set_bytes(name.len());
    let mut found = None;
    let mut i = 0;
    while i + want <= data.len() {
        let t = data[i];
        if t == 0 {
            break;
        }
        if t == 0x85 && data[i + 1] as usize == want / 32 - 1 && data[i + 32] == 0xC0 {
            let set = &data[i..i + want];
            if crate::volume::checksum_ok_set(set)
                && set[35] as usize == name.len()
                && let Ok(f) = super::set::decode(set)
                && f.name == name
                && f.attrs & 0x16 == 0x06
            {
                ensure!(found.is_none(), "the root holds two {NAME} entries; run fsck.exfat");
                found = Some(Place { first: f.first, len: f.len, nfc: f.nfc });
            }
        }
        i += 32;
    }
    Ok(found)
}

/// The journal's size for a volume of `vol_bytes` with clusters of `cs`.
pub(crate) fn capacity(vol_bytes: u64, cs: u64) -> u64 {
    // test hook: a small journal file, to exercise split commits
    if let Some(b) = std::env::var(JOURNAL_BYTES_ENV).ok().and_then(|v| v.parse::<u64>().ok()) {
        return b.max(4096).div_ceil(cs) * cs;
    }
    (vol_bytes / 512).clamp(1 << 20, 8 << 20).div_ceil(cs) * cs
}

fn ident(v: &Vol) -> (u32, String) {
    (le32(&v.sector0, 100), crate::journal::geometry_of(&v.sector0))
}

fn header(seq: u64, state: u8, body: &[u8]) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..16].copy_from_slice(MAGIC);
    h[16..24].copy_from_slice(&seq.to_le_bytes());
    h[24] = state;
    h[32..40].copy_from_slice(&(body.len() as u64).to_le_bytes());
    h[40..56].copy_from_slice(&xxhash_rust::xxh3::xxh3_128(body).to_le_bytes());
    let hh = xxhash_rust::xxh3::xxh3_64(&h[..56]);
    h[56..64].copy_from_slice(&hh.to_le_bytes());
    h
}

struct Header {
    seq: u64,
    state: u8,
    body_len: u64,
    body_hash: u128,
}

fn parse_header(h: &[u8]) -> Option<Header> {
    if &h[..16] != MAGIC || xxhash_rust::xxh3::xxh3_64(&h[..56]) != le64(h, 56) {
        return None;
    }
    Some(Header {
        seq: le64(h, 16),
        state: h[24],
        body_len: le64(h, 32),
        body_hash: u128::from_le_bytes(h[40..56].try_into().unwrap()),
    })
}

/// A data sample: device range and the xxh3 of its bytes.
pub(crate) type Sample = (u64, u32, u64);

pub(crate) fn encode_body(v: &Vol, writes: &[JW], samples: &[Sample]) -> Result<Vec<u8>> {
    let (serial, geom) = ident(v);
    let mut b = Vec::new();
    b.extend_from_slice(BODY_MAGIC);
    b.extend_from_slice(&serial.to_le_bytes());
    b.extend_from_slice(&(geom.len() as u16).to_le_bytes());
    b.extend_from_slice(geom.as_bytes());
    b.extend_from_slice(&(writes.len() as u32).to_le_bytes());
    for w in writes {
        b.push(w.phase);
        b.extend_from_slice(&w.off.to_le_bytes());
        b.extend_from_slice(&(w.post.len() as u32).to_le_bytes());
        b.extend_from_slice(&w.pre);
        b.extend_from_slice(&w.post);
    }
    b.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.0.to_le_bytes());
        b.extend_from_slice(&s.1.to_le_bytes());
        b.extend_from_slice(&s.2.to_le_bytes());
    }
    zstd::bulk::compress(&b, 3).context("compress the journal")
}

struct Body {
    serial: u32,
    geometry: String,
    writes: Vec<JW>,
    samples: Vec<Sample>,
}

fn decode_body(z: &[u8], max: usize) -> Result<Body> {
    let b = zstd::bulk::decompress(z, max).context("journal body does not decompress")?;
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8]> {
        ensure!(at + n <= b.len(), "journal body is truncated");
        at += n;
        Ok(&b[at - n..at])
    };
    ensure!(take(4)? == BODY_MAGIC, "not a drive journal body");
    let serial = u32::from_le_bytes(take(4)?.try_into()?);
    let gl = u16::from_le_bytes(take(2)?.try_into()?) as usize;
    let geometry = String::from_utf8(take(gl)?.to_vec())?;
    // counts are never trusted to size anything: records are pushed as they parse, and a
    // count that cannot fit the remaining bytes is refused first
    let nw = u32::from_le_bytes(take(4)?.try_into()?) as usize;
    ensure!(nw.saturating_mul(13) <= b.len(), "the journal declares more writes than its body holds");
    let mut writes = Vec::new();
    for _ in 0..nw {
        let phase = take(1)?[0];
        ensure!((phase as usize) < PHASES.len(), "bad phase in journal");
        let off = u64::from_le_bytes(take(8)?.try_into()?);
        let len = u32::from_le_bytes(take(4)?.try_into()?) as usize;
        let pre = take(len)?.to_vec();
        let post = take(len)?.to_vec();
        writes.push(JW { phase, off, pre, post });
    }
    let ns = u32::from_le_bytes(take(4)?.try_into()?) as usize;
    ensure!(ns.saturating_mul(20) <= b.len(), "the journal declares more samples than its body holds");
    let mut samples = Vec::new();
    for _ in 0..ns {
        let off = u64::from_le_bytes(take(8)?.try_into()?);
        let len = u32::from_le_bytes(take(4)?.try_into()?);
        ensure!(len as u64 <= MAX_SAMPLE, "a journal sample of {len} bytes is longer than a commit writes");
        let h = u64::from_le_bytes(take(8)?.try_into()?);
        samples.push((off, len, h));
    }
    Ok(Body { serial, geometry, writes, samples })
}

/// Device offset of the journal file's first byte.
fn base(v: &Vol, p: &Place) -> u64 {
    v.cluster_off(p.first)
}

/// Writes the commit's journal (ACTIVE) and syncs.
pub(crate) fn write_active(dev: &dyn Dev, v: &Vol, p: &Place, seq: u64, body: &[u8]) -> Result<()> {
    ensure!(
        512 + body.len() as u64 <= p.len,
        "the commit's journal ({} bytes) does not fit the journal file ({} bytes)",
        body.len(),
        p.len
    );
    let mut buf = Vec::with_capacity(512 + body.len());
    buf.extend_from_slice(&header(seq, STATE_ACTIVE, body));
    buf.extend_from_slice(body);
    buf.resize(buf.len().div_ceil(512) * 512, 0);
    dev.write_at(&buf, base(v, p)).context("write the journal")?;
    dev.sync().context("sync")
}

pub(crate) fn write_idle(dev: &dyn Dev, v: &Vol, p: &Place, seq: u64) -> Result<()> {
    write_idle_at(dev, header_off(v, p), seq)
}

/// Marks the journal whose header is at byte `off` IDLE, and syncs.
pub(crate) fn write_idle_at(dev: &dyn Dev, off: u64, seq: u64) -> Result<()> {
    dev.write_at(&header(seq, STATE_IDLE, &[]), off).context("write the journal header")?;
    dev.sync().context("sync")
}

/// Device offset of the journal's header.
pub(crate) fn header_off(v: &Vol, p: &Place) -> u64 {
    base(v, p)
}

/// Whether every byte `writes` touch already holds its final state (the last write's `post`):
/// the commit finished, and only its IDLE header may be missing. Bounded like [`validate`]
/// before anything is read.
fn completed(v: &Vol, writes: &[JW]) -> Result<bool> {
    let mut spans: Vec<(u64, u64)> = Vec::with_capacity(writes.len());
    for w in writes {
        let end = w.off.checked_add(w.post.len() as u64).context("a journaled write overflows")?;
        ensure!(end <= v.vol_bytes && w.pre.len() == w.post.len(), "a journaled write is malformed");
        spans.push((w.off, end));
    }
    spans.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (a, z) in spans {
        match merged.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(z),
            _ => merged.push((a, z)),
        }
    }
    let total: u64 = merged.iter().map(|r| r.1 - r.0).sum();
    ensure!(total <= 2 * MAX_RAW as u64, "the journal touches {total} bytes, more than a commit writes");
    for (a, z) in merged {
        let now = v.read_at(a, z - a)?;
        let mut want = now.clone();
        for w in writes.iter().filter(|w| w.off < z && a < w.off + w.post.len() as u64) {
            let o = (w.off - a) as usize;
            want[o..o + w.post.len()].copy_from_slice(&w.post);
        }
        if want != now {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Applies `writes` phase by phase, a sync after each phase that wrote something.
pub(crate) fn apply(dev: &dyn Dev, writes: &[JW], mut on_phase: impl FnMut(&str)) -> Result<()> {
    for (k, name) in PHASES.iter().enumerate() {
        let ws: Vec<&JW> = writes.iter().filter(|w| w.phase as usize == k).collect();
        if ws.is_empty() {
            continue;
        }
        on_phase(name);
        crate::sim::trace::phase(name);
        for w in ws {
            dev.write_at(&w.post, w.off).with_context(|| format!("write metadata at {}", w.off))?;
        }
        dev.sync().context("sync")?;
        // test hook: die right after this phase (a crash with the journal active)
        if std::env::var(ABORT_AFTER_ENV).is_ok_and(|v| v == *name) {
            eprintln!("{ABORT_AFTER_ENV}: aborting after phase {name}");
            std::process::abort();
        }
    }
    Ok(())
}

/// Test hook (hardware crash tests): abort the process after the named metadata phase.
pub(crate) const ABORT_AFTER_ENV: &str = "FLASHCP_TEST_ABORT_AFTER";

#[derive(Debug, PartialEq, Eq)]
pub enum Recovered {
    Nothing,
    /// An interrupted commit was replayed (writes of it).
    Replayed(usize),
}

/// Why recovery refused to write anything.
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

/// Reads the journal file's header and body (when ACTIVE and whole).
fn load(v: &Vol, p: &Place) -> Result<Option<(Header, Body)>> {
    ensure!(p.nfc && p.len >= 4096 && p.len <= 64 << 20, "{NAME} is not a journal flashcp made (layout)");
    let h = v.read_at(base(v, p), 512)?;
    let Some(h) = parse_header(&h) else { return Ok(None) };
    if h.state != STATE_ACTIVE {
        return Ok(None);
    }
    if h.body_len == 0 || 512 + h.body_len > p.len {
        return Ok(None);
    }
    if h.body_len > MAX_BODY {
        bail!(Refused(format!("{NAME} claims a body of {} bytes, more than a commit writes", h.body_len)));
    }
    let z = v.read_at(base(v, p) + 512, h.body_len)?;
    if xxhash_rust::xxh3::xxh3_128(&z) != h.body_hash {
        return Ok(None); // a torn journal write: its commit never started its metadata
    }
    // the decoded size is checked before anything is allocated for it
    match zstd::zstd_safe::get_frame_content_size(&z) {
        Ok(Some(n)) if n <= MAX_RAW as u64 => {}
        Ok(Some(n)) => bail!(Refused(format!("{NAME} decodes to {n} bytes, more than a commit writes"))),
        _ => bail!(Refused(format!("{NAME}: the body does not state its size"))),
    }
    let body = decode_body(&z, MAX_RAW).map_err(|e| Refused(format!("{NAME}: {e:#}")))?;
    Ok(Some((h, body)))
}

/// Replays an interrupted commit (see the module doc). Err([`Refused`]) when the volume
/// changed since; nothing is written then.
pub(crate) fn recover(v: &Vol, dev: &dyn Dev, p: &Place) -> Result<Recovered> {
    let Some((h, body)) = load(v, p)? else { return Ok(Recovered::Nothing) };
    let (serial, geom) = ident(v);
    if body.serial != serial || body.geometry != geom {
        bail!(Refused(format!(
            "the interrupted commit's journal is for volume {:08x} ({}), not this one ({serial:08x}, {geom}); \
             nothing was written",
            body.serial, body.geometry
        )));
    }
    // A commit that finished but was not marked IDLE yet (a crash right after its last sync,
    // or the header a commit leaves ACTIVE while the next one may follow, see `fs.rs`):
    // nothing to replay. Its data samples are not checked: a later commit may have written
    // into a sampled run's own slack since (an append), which is no reason to refuse.
    if completed(v, &body.writes).map_err(|e| Refused(format!("{NAME}: {e:#}")))? {
        write_idle(dev, v, p, h.seq)?;
        return Ok(Recovered::Nothing);
    }
    let sampled: u64 = body.samples.iter().map(|s| s.1 as u64).sum();
    if sampled > MAX_SAMPLED || body.samples.iter().any(|s| s.0.checked_add(s.1 as u64).is_none_or(|e| e > v.vol_bytes))
    {
        bail!(Refused(format!("the journal's data samples ({sampled} bytes) are not a commit's; nothing was written")));
    }
    for &(off, len, hash) in &body.samples {
        let b = v.read_at(off, len as u64)?;
        if xxhash_rust::xxh3::xxh3_64(&b) != hash {
            bail!(Refused(format!(
                "the volume changed since flashcp's interrupted commit (data at byte {off} differs); nothing was \
                 written. Run fsck.exfat."
            )));
        }
    }
    validate(v, &body.writes).map_err(|e| Refused(format!("{e:#}; nothing was written. Run fsck.exfat.")))?;
    let jw: Vec<crate::journal::JWrite> = body
        .writes
        .iter()
        .map(|w| crate::journal::JWrite {
            phase: PHASES[w.phase as usize].to_string(),
            off: w.off,
            pre: w.pre.clone(),
            post: w.post.clone(),
        })
        .collect();
    if let Err(e) = crate::journal::check(&v.dev, &jw) {
        if std::env::var_os("FLASHCP_DEBUG_JOURNAL").is_some() {
            for w in &body.writes {
                eprintln!(
                    "jw phase {} off {} len {} sectors {}..{}",
                    w.phase,
                    w.off,
                    w.post.len(),
                    w.off / 512,
                    (w.off + w.post.len() as u64).div_ceil(512)
                );
            }
            let cur = v.read_at(2050 * 512, 512)?;
            eprintln!("sector 2050 now: {:02x?}", &cur[..64]);
        }
        bail!(Refused(format!("{e:#}. Run fsck.exfat.")));
    }
    apply(dev, &body.writes, |_| {})?;
    write_idle(dev, v, p, h.seq)?;
    Ok(Recovered::Replayed(body.writes.len()))
}

/// Checks that every journaled write is one a commit can make, before anything is replayed:
/// - inside the volume, `pre` and `post` of one length;
/// - `dirty` / `clean`: VolumeFlags (bytes 106–107) and PercentInUse (112) of sector 0 only;
/// - `alloc` / `link`: FAT entries (whole, aligned), or bitmap clusters (`alloc`, `free`);
/// - `alloc` / `link`: directory clusters (`alloc` writes deleted entries past a
///   directory's end, so that a set placed after them starts a sector).
///
/// Which clusters hold the bitmap and the directories comes from the tree as it will be after
/// the replay: the device's bytes with every write's `post` applied, walked from the root with
/// the shared validating parser (bounded, every cluster visited once). A write anywhere else
/// (file data, the up-case table, the journal, the backup boot region) is refused, so a forged
/// journal can never alter file contents.
pub(crate) fn validate(v: &Vol, writes: &[JW]) -> Result<()> {
    use std::collections::{BTreeMap, HashSet};
    let bps = v.bps;
    let fat_len = le32(&v.sector0, 84) as u64 * bps;
    let (fat_a, fat_b) = (v.fat_off, v.fat_off + fat_len);
    let (heap_a, heap_b) = (v.heap_off, v.heap_off + v.cc * v.cs);
    // the post-replay image of the touched bytes, as merged sector-aligned ranges (their
    // total is bounded by the body's size, checked before anything is read)
    let mut spans: Vec<(u64, u64)> = Vec::with_capacity(writes.len());
    for w in writes {
        ensure!(!w.post.is_empty() && w.pre.len() == w.post.len(), "a journaled write is malformed");
        let end = w.off.checked_add(w.post.len() as u64).context("a journaled write overflows")?;
        ensure!(end <= v.vol_bytes, "a journaled write at byte {} lies outside the volume", w.off);
        spans.push((w.off / 512 * 512, end.div_ceil(512) * 512));
    }
    spans.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (a, z) in spans {
        match merged.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(z),
            _ => merged.push((a, z)),
        }
    }
    let total: u64 = merged.iter().map(|r| r.1 - r.0).sum();
    ensure!(total <= 2 * MAX_RAW as u64, "the journal touches {total} bytes, more than a commit writes");
    let mut img: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for (a, z) in merged {
        img.insert(a, v.read_at(a, z - a)?);
    }
    for w in writes {
        let (&a, b) = img.range_mut(..=w.off).next_back().unwrap();
        let o = (w.off - a) as usize;
        b[o..o + w.post.len()].copy_from_slice(&w.post);
    }
    let read = |off: u64, len: u64| -> Result<Vec<u8>> {
        let mut out = v.read_at(off, len)?;
        let first = img.range(..=off).next_back().map_or(off, |(&a, _)| a);
        for (&s, b) in img.range(first..off + len) {
            let (a, z) = (s.max(off), (s + b.len() as u64).min(off + len));
            if a < z {
                out[(a - off) as usize..(z - off) as usize].copy_from_slice(&b[(a - s) as usize..(z - s) as usize]);
            }
        }
        Ok(out)
    };
    let fat = |c: u32| -> Result<u32> {
        ensure!(c >= 2 && (c as u64) < v.cc + 2, "cluster {c:#x} out of range");
        Ok(le32(&read(fat_a + c as u64 * 4, 4)?, 0))
    };
    let cluster_of = |off: u64| ((off - heap_a) / v.cs) as u32 + 2;
    // heap clusters the writes touch, by phase
    let mut need: HashSet<u32> = HashSet::new();
    for w in writes {
        let (a, z) = (w.off, w.off + w.post.len() as u64);
        let phase = PHASES[w.phase as usize];
        if z <= 512 {
            let only = |ok: &[u64]| (a..z).all(|x| ok.contains(&x));
            match phase {
                "dirty" => ensure!(only(&[106, 107]), "the journal writes the boot sector beyond VolumeFlags"),
                "clean" => ensure!(only(&[106, 107, 112]), "the journal writes the boot sector beyond its flags"),
                _ => bail!("the journal writes the boot sector in phase {phase}"),
            }
        } else if a >= fat_a && z <= fat_b {
            ensure!(matches!(phase, "alloc" | "link"), "the journal writes the FAT in phase {phase}");
            ensure!((a - fat_a).is_multiple_of(4) && (z - a).is_multiple_of(4), "a FAT write is not whole entries");
        } else if a >= heap_a && z <= heap_b {
            ensure!(matches!(phase, "alloc" | "link" | "free"), "the journal writes the cluster heap in phase {phase}");
            for c in cluster_of(a)..=cluster_of(z - 1) {
                need.insert(c);
            }
        } else {
            bail!("a journaled write at byte {a} is outside every metadata region");
        }
    }
    if need.is_empty() {
        return Ok(());
    }
    // walk the post-replay tree: the bitmap's clusters and every directory's
    let chain = |first: u32, nfc: bool, len: u64, seen: &mut HashSet<u32>| -> Result<Vec<u32>> {
        let n = len.div_ceil(v.cs);
        ensure!(n <= v.cc, "an object is longer than the volume");
        let mut out = Vec::with_capacity(n.min(1 << 16) as usize);
        let mut c = first;
        for k in 0..n {
            ensure!(c >= 2 && (c as u64) < v.cc + 2, "a chain leaves the cluster heap");
            ensure!(seen.insert(c), "cluster {c:#x} is referenced twice");
            out.push(c);
            if k + 1 < n {
                c = if nfc { c + 1 } else { fat(c)? };
            }
        }
        Ok(out)
    };
    let mut seen: HashSet<u32> = HashSet::new();
    let root = le32(&v.sector0, 96);
    let mut root_cl = Vec::new();
    let mut c = root;
    loop {
        ensure!(c >= 2 && (c as u64) < v.cc + 2 && seen.insert(c), "the root directory's chain is broken");
        root_cl.push(c);
        ensure!(root_cl.len() as u64 * v.cs <= crate::volume::MAX_DIR_BYTES, "the root directory is too long");
        c = fat(c)?;
        if c == crate::volume::EOC {
            break;
        }
    }
    let mut allowed: std::collections::HashMap<u32, &'static str> = std::collections::HashMap::new();
    let load = |cl: &[u32]| -> Result<Vec<u8>> {
        let mut d = Vec::with_capacity(cl.len() * v.cs as usize);
        for &c in cl {
            d.extend(read(v.cluster_off(c), v.cs)?);
        }
        Ok(d)
    };
    let mut extras = crate::volume::RootExtras::default();
    let data = load(&root_cl)?;
    let parsed = crate::volume::parse_dir(&data, Some(&mut extras), "/", v.cs, false)?;
    for &c in &root_cl {
        allowed.insert(c, "dir");
    }
    ensure!(extras.bitmaps.len() == 1, "the volume after the replay has no single bitmap");
    let bm = extras.bitmaps[0];
    ensure!(le64(&bm, 24) == v.cc.div_ceil(8), "the bitmap's length does not match the cluster count");
    for c in chain(le32(&bm, 20), false, le64(&bm, 24), &mut seen)? {
        allowed.insert(c, "bitmap");
    }
    let mut stack: Vec<(Vec<crate::volume::Child>, String)> = vec![(parsed.1, String::new())];
    'walk: while let Some((kids, path)) = stack.pop() {
        for ch in kids.into_iter().filter(|c| c.is_dir) {
            if need.iter().all(|c| allowed.contains_key(c)) {
                break 'walk;
            }
            ensure!(ch.len <= crate::volume::MAX_DIR_BYTES && ch.len > 0, "a directory has an invalid size");
            let p = format!("{path}/{}", String::from_utf16_lossy(&ch.name));
            let cl = chain(ch.first, ch.no_fat_chain, ch.len, &mut seen)?;
            for &c in &cl {
                allowed.insert(c, "dir");
            }
            let d = load(&cl)?;
            let (_, sub, _) = crate::volume::parse_dir(&d, None, &p, v.cs, false)?;
            stack.push((sub, p));
        }
    }
    for w in writes {
        let (a, z) = (w.off, w.off + w.post.len() as u64);
        if a < heap_a || z > heap_b {
            continue;
        }
        let phase = PHASES[w.phase as usize];
        for c in cluster_of(a)..=cluster_of(z - 1) {
            match (allowed.get(&c).copied(), phase) {
                (Some("bitmap"), "alloc" | "free") | (Some("dir"), "alloc" | "link") => {}
                (what, _) => bail!(
                    "the journal writes cluster {c:#x} ({}) in phase {phase}: not a commit's write",
                    what.unwrap_or("file data or free space")
                ),
            }
        }
    }
    Ok(())
}

/// The entry set of the journal file.
pub(crate) fn entry(upc: &[u16], first: u32, len: u64) -> Vec<u8> {
    let t = super::core::now();
    let f = super::set::SetFields {
        name: NAME.encode_utf16().collect(),
        attrs: ATTRS,
        first,
        nfc: true,
        len,
        valid: len,
        ctime: t,
        mtime: t,
        atime: t,
    };
    super::set::encode(&f, upc, None)
}

/// A slot for a set of `need` bytes inside one 512-byte sector of root data `d` (before
/// `end`: a run of unused entries; else at `end`, padded up to the next sector with deleted
/// entries). Returns (offset, padding entries before it at `end`).
pub(crate) fn one_sector_slot(d: &[u8], end: usize, need: usize) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + need <= end {
        if i % 512 + need <= 512 && (0..need / 32).all(|k| d[i + 32 * k] & 0x80 == 0 && d[i + 32 * k] != 0) {
            return Some((i, 0));
        }
        i += 32;
    }
    let at = if end % 512 + need <= 512 { end } else { end.div_ceil(512) * 512 };
    if at + need <= d.len() { Some((at, (at - end) / 32)) } else { None }
}

/// The journal's bytes for a mount-time check: whether the journal names a pending commit,
/// read from root data `root` (no other reads but the header).
pub(crate) fn header_state(v: &Vol, p: &Place) -> Option<bool> {
    let h = v.read_at(base(v, p), 512).ok()?;
    parse_header(&h).map(|h| h.state == STATE_ACTIVE)
}
