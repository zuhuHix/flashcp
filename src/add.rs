//! `flashcp add`: add a tree to an EXISTING exFAT volume without erasing anything, and
//! `flashcp info`: read-only volume summary.
//!
//! The volume is parsed and validated first (boot region and checksum, FAT, allocation
//! bitmap, up-case table, every directory on the way to the destination). Anything we do not
//! fully understand (TexFAT / two FATs, a dirty volume, bad checksums, unknown critical
//! directory entries) is refused before a single byte is written.
//!
//! Allocation prefers one contiguous run (every new object NoFatChain). When free space is
//! fragmented, the biggest free extents are used, filled in ascending order; an object that
//! crosses an extent boundary gets a real FAT chain (NoFatChain = 0).
//!
//! Crash ordering (a sync between every phase):
//!   0. set VolumeDirty in the main boot sector (excluded from the boot checksum);
//!   1. data: file contents, new directories' clusters, and extension clusters of existing
//!      directories — all in clusters that are still free, so nothing references them yet.
//!      Written as one ascending stream through the sequential (O_DIRECT) writer;
//!   2. allocation bitmap + FAT chains of the new objects (still unreferenced: a crash here
//!      only leaks clusters);
//!   3. FAT changes to EXISTING chains (linking extension clusters to an existing directory),
//!      then the directory entries: entry sets appended into existing directories, and
//!      DataLength / NoFatChain updates of extended directories. Only now does anything
//!      become reachable, and everything it points at is already on disk and allocated;
//!   4. clear VolumeDirty and update PercentInUse.

use crate::devprofile::{GapMode, PatternChoice};
use crate::events::{Event, Op, Sink, Ticker};
use crate::exfat::{
    Item, Node, SeqWriter, TargetSpec, entry_set, finish_set, open_target_fd, ordered_reader, release, scan_tree,
    set_bytes, set_direct, validate_name,
};
use crate::sim::trace::{self, Op as TOp};
use crate::util::human_bytes;
use crate::volume::*;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Test hook: stop after the named phase (dirty | data | alloc | fat) to simulate a crash.
const STOP_ENV: &str = "FLASHCP_TEST_STOP_AFTER";
/// Test hook: print how many gap bytes the data phase zero-filled and how many it skipped.
const GAPS_ENV: &str = "FLASHCP_TEST_GAPS";
/// Test hook: use a "fill gaps up to N bytes, free clusters too" policy (any target), so the
/// rule that a used cluster is never zero-filled can be exercised on image files.
const GAP_FILL_ENV: &str = "FLASHCP_TEST_GAP_FILL";
/// Without a device profile (image files, unmeasured devices): gaps up to this size are
/// zero-filled, and only when they are our own cluster slack.
const SLACK_FILL: u64 = 32 << 10;

#[derive(Clone, Debug, Default)]
pub struct AddOpts {
    /// Directory inside the volume to add into ("a/b/c"); missing parents are created.
    pub to: Option<String>,
    /// Skip source entries whose name already exists (instead of failing).
    pub skip_existing: bool,
    pub readers: usize,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct AddStats {
    pub files: u64,
    pub dirs: u64,
    pub skipped: u64,
    pub data_bytes: u64,
    pub written: u64,
    /// New files and directories that had to be split across free extents (FAT-chained).
    pub fragmented: u64,
    pub cluster: u64,
    pub secs: f64,
}

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub label: String,
    pub cluster: u64,
    /// Cluster heap size in bytes.
    pub size: u64,
    pub free: u64,
    pub files: u64,
    pub dirs: u64,
    pub dirty: bool,
}

impl VolumeInfo {
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"label":{},"cluster":{},"size":{},"free":{},"files":{},"dirs":{},"dirty":{}}}"#,
            crate::events::json_str(&self.label),
            self.cluster,
            self.size,
            self.free,
            self.files,
            self.dirs,
            self.dirty
        )
    }
}

/// Free extents (start cluster, length) from the bitmap, ascending.
pub(crate) fn free_extents(bitmap: &[u8], cc: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut run: Option<u64> = None;
    let mut i = 0u64;
    while i < cc {
        let byte = bitmap[(i / 8) as usize];
        if i.is_multiple_of(8) && i + 8 <= cc && (byte == 0 || byte == 0xFF) {
            match (byte, run) {
                (0, None) => run = Some(i),
                (0xFF, Some(s)) => {
                    out.push((s + 2, i - s));
                    run = None;
                }
                _ => {}
            }
            i += 8;
            continue;
        }
        let used = (byte >> (i % 8)) & 1 == 1;
        match (used, run) {
            (false, None) => run = Some(i),
            (true, Some(s)) => {
                out.push((s + 2, i - s));
                run = None;
            }
            _ => {}
        }
        i += 1;
    }
    if let Some(s) = run {
        out.push((s + 2, cc - s));
    }
    out
}

/// Removes [start, start+n) from the free list if it lies inside one extent.
pub(crate) fn take_exact(free: &mut Vec<(u64, u64)>, start: u64, n: u64) -> bool {
    let Some(k) = free.iter().position(|&(s, l)| s <= start && start + n <= s + l) else { return false };
    let (s, l) = free.remove(k);
    if start + n < s + l {
        free.insert(k, (start + n, s + l - start - n));
    }
    if start > s {
        free.insert(k, (s, start - s));
    }
    true
}

/// Picks extents holding `total` clusters: the smallest single extent that fits, otherwise the
/// biggest ones. Returned ascending by address.
pub(crate) fn choose_extents(free: &[(u64, u64)], total: u64) -> Option<Vec<(u64, u64)>> {
    if total == 0 {
        return Some(Vec::new());
    }
    if let Some(&(s, _)) = free.iter().filter(|e| e.1 >= total).min_by_key(|e| (e.1, e.0)) {
        return Some(vec![(s, total)]);
    }
    let mut by_size = free.to_vec();
    by_size.sort_by_key(|e| (std::cmp::Reverse(e.1), e.0));
    let mut chosen = Vec::new();
    let mut sum = 0;
    for e in by_size {
        if sum >= total {
            break;
        }
        let take = e.1.min(total - sum);
        chosen.push((e.0, take));
        sum += take;
    }
    if sum < total {
        return None;
    }
    chosen.sort();
    Some(chosen)
}

/// Splits `n` clusters off the front of `extents` (consumed in order) as runs.
pub(crate) fn carve(extents: &mut std::collections::VecDeque<(u64, u64)>, mut n: u64) -> Vec<(u64, u64)> {
    let mut runs = Vec::new();
    while n > 0 {
        let (s, l) = extents.pop_front().expect("allocation exceeded the chosen extents");
        let take = l.min(n);
        runs.push((s, take));
        if take < l {
            extents.push_front((s + take, l - take));
        }
        n -= take;
    }
    runs
}

pub(crate) fn run_clusters(runs: &[(u64, u64)]) -> Vec<u32> {
    runs.iter().flat_map(|&(s, n)| (s..s + n).map(|c| c as u32)).collect()
}

/// FAT entries chaining `clusters` in order, ending with EOC.
pub(crate) fn chain_entries(clusters: &[u32], out: &mut Vec<(u32, u32)>) {
    for (k, &c) in clusters.iter().enumerate() {
        out.push((c, clusters.get(k + 1).copied().unwrap_or(EOC)));
    }
}

// ───────────────────────── gap policy ─────────────────────────

/// How the data phase treats a gap between two spans it writes: zero-fill it (the stream stays
/// one sequential run) or skip it (the next span is a new request), and how far a file's last
/// write is padded.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GapPolicy {
    /// Fill gaps of at most this many bytes, skip bigger ones.
    pub(crate) max_fill: u64,
    /// Gaps may also cover clusters outside our runs that are free in the pre-add bitmap
    /// (otherwise only our own slack is filled).
    pub(crate) free_too: bool,
    /// A file's last write is padded with zeros to a multiple of this many bytes (≥ 4096),
    /// never past the end of its run.
    pub(crate) pad: u64,
    /// Writes in flight for the scattered spans.
    pub(crate) qd: usize,
    /// The candidate this policy is, when it is one (for the plan line).
    pub(crate) mode: Option<GapMode>,
    /// Where the policy came from, for the plan line.
    pub(crate) source: String,
}

/// Filling a gap of `gap` bytes costs gap / bps; skipping it costs one more request
/// (skip_s). Fill iff that is cheaper.
pub(crate) fn worth_filling(gap: u64, bps: f64, skip_s: f64) -> bool {
    (gap as f64) / bps < skip_s
}

/// The "fill every gap" candidate fills gaps up to this size (one allocation unit): a bigger
/// gap is free space between free extents, not slack.
const FILL_CAP: u64 = 4 << 20;
/// The "pad" candidate pads each file's write to this (a typical NAND page / FTL unit).
const PAD_UNIT: u64 = 16 << 10;
/// Minimum padding: the O_DIRECT alignment.
const MIN_PAD: u64 = 4096;

impl GapPolicy {
    /// The old fixed behaviour: our own slack up to 32 KB, `qd` writes in flight.
    pub(crate) fn fallback(source: &str) -> GapPolicy {
        Self::fallback_qd(source, crate::devprofile::DEFAULT_WRITE_QD)
    }
    pub(crate) fn fallback_qd(source: &str, qd: usize) -> GapPolicy {
        GapPolicy { max_fill: SLACK_FILL, free_too: false, pad: MIN_PAD, qd, mode: None, source: source.into() }
    }
    /// One of the candidates the in-situ probe compares.
    pub(crate) fn of(mode: GapMode, qd: usize, source: &str) -> GapPolicy {
        let (max_fill, pad) = match mode {
            GapMode::Skip => (0, MIN_PAD),
            GapMode::Fill => (FILL_CAP, MIN_PAD),
            GapMode::Pad(p) => (0, p.max(MIN_PAD)),
        };
        GapPolicy { max_fill, free_too: true, pad, qd, mode: Some(mode), source: source.into() }
    }
    /// From a cached write model (bandwidth + cost per request): the largest gap
    /// [`worth_filling`] accepts. `qd` comes from the transport, never from the model (a
    /// random-4K QD8 number on USB BOT measures write merging, not what add does).
    pub(crate) fn from_profile(p: &crate::devprofile::DevProfile, qd: usize, source: &str) -> GapPolicy {
        let (bps, skip) = p.write_cost();
        let g = (bps * skip).floor().max(0.0) as u64;
        let max_fill = if worth_filling(g, bps, skip) { g } else { g.saturating_sub(1) };
        GapPolicy { max_fill, free_too: true, pad: MIN_PAD, qd, mode: None, source: source.into() }
    }
    pub(crate) fn describe(&self) -> String {
        let what = match self.mode {
            Some(GapMode::Skip) => "gaps: skip all".to_string(),
            Some(GapMode::Fill) => format!("gaps: fill free ≤ {}", human_bytes(self.max_fill as f64)),
            Some(GapMode::Pad(p)) => format!("gaps: pad files to {}, skip the rest", human_bytes(p as f64)),
            None => format!(
                "gaps: {} ≤ {}, skip otherwise",
                if self.free_too { "fill" } else { "fill own slack" },
                human_bytes(self.max_fill as f64)
            ),
        };
        format!("{what}; {} in flight ({})", self.qd, self.source)
    }
    /// Whether the gap from the stream position `cur` to the next span at `next` is
    /// zero-filled. `prev_end` is where our previous span (whole clusters) ended: the clusters
    /// from there to `next` are foreign (see [`fill_gap`]). Nothing before the first span.
    fn fills(&self, cur: u64, next: u64, prev_end: Option<u64>, pre_bitmap: &[u8], heap: u64, cs: u64) -> bool {
        prev_end.is_some_and(|pe| fill_gap(next - cur, (pe - heap) / cs + 2..(next - heap) / cs + 2, pre_bitmap, self))
    }
    /// Where a file's last write ends: `cur` padded to [`GapPolicy::pad`], within its run.
    fn pad_end(&self, cur: u64, run_end: u64) -> u64 {
        (cur.div_ceil(self.pad) * self.pad).min(run_end)
    }
}

/// Whether every cluster in `clusters` is free in `bitmap` (bit c − 2).
fn all_free(bitmap: &[u8], clusters: std::ops::Range<u64>) -> bool {
    clusters.into_iter().all(|c| {
        let i = (c - 2) as usize;
        bitmap.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 0)
    })
}

/// The gap decision. The gap is `gap` bytes: our own slack (inside the run we just wrote, so
/// always ours to write), followed by the clusters `foreign` that lie between our runs. Those
/// belong to nobody only if the PRE-ADD bitmap calls them free (verify_allocation proved that
/// no clear bit is referenced). A used cluster is never written, whatever the policy says:
/// that would destroy an existing file.
pub(crate) fn fill_gap(gap: u64, foreign: std::ops::Range<u64>, pre_bitmap: &[u8], pol: &GapPolicy) -> bool {
    if gap > pol.max_fill {
        return false;
    }
    foreign.is_empty() || (pol.free_too && all_free(pre_bitmap, foreign))
}

// ───────────────────────── in-situ write probe ─────────────────────────

/// One data-phase segment as the probe sees it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProbeSeg {
    /// Partition offset and length (whole clusters).
    dev: u64,
    len: u64,
    /// Bytes of content in it (the rest is slack).
    payload: u64,
    /// The last segment of a file (its write gets padded).
    file_end: bool,
}

/// The writes the data phase issues for `segs` under `pol`, as (offset, length), contiguous
/// writes merged: exactly the decisions of the data-phase loop ([`GapPolicy::fills`],
/// [`GapPolicy::pad_end`]).
fn planned_writes(segs: &[ProbeSeg], pol: &GapPolicy, pre_bitmap: &[u8], heap: u64, cs: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut push = |a: u64, b: u64| {
        if b > a {
            match out.last_mut() {
                Some(l) if l.0 + l.1 == a => l.1 += b - a,
                _ => out.push((a, b - a)),
            }
        }
    };
    let Some(first) = segs.first() else { return out };
    let (mut cur, mut prev_end) = (first.dev, None);
    for s in segs {
        if s.dev > cur && pol.fills(cur, s.dev, prev_end, pre_bitmap, heap, cs) {
            push(cur, s.dev);
        }
        cur = cur.max(s.dev);
        prev_end = Some(s.dev + s.len);
        let mut end = s.dev + s.payload;
        if s.file_end {
            end = pol.pad_end(end, s.dev + s.len);
        }
        push(cur, end);
        cur = end;
    }
    out
}

/// The probe measures in allocation units of this size, aligned to it on the whole device.
const PROBE_AU: u64 = 4 << 20;
/// Each candidate runs over this many consecutive allocation units (so the cost of closing
/// an AU, a deferred merge, is in the number).
const PROBE_AUS: u64 = 3;
/// Candidates the probe runs, one window each: skip every gap, fill every gap. (Padding to
/// 16 KB and 2 or 8 writes in flight were measured on a USB 2 BOT stick and did not pay; see
/// [`probe_in_situ`].)
const PROBE_WINDOWS: u64 = 2;
/// Fewest windows worth measuring: a plan smaller than that is too small to matter.
const PROBE_MIN_WINDOWS: u64 = 2;
/// Spans at least this big are written alone (as [`crate::exfat::SeqWriter`] does).
const PROBE_SOLO: u64 = 1 << 20;

/// Consecutive windows of PROBE_AUS × PROBE_AU from the first AU boundary in the data
/// stream: index ranges into `segs` (segments wholly inside each window). Only as many as the
/// stream covers completely, at most PROBE_WINDOWS.
fn probe_windows(segs: &[ProbeSeg], dev_off: u64) -> Vec<std::ops::Range<usize>> {
    let Some(first) = segs.first() else { return vec![] };
    let end = segs.last().map_or(0, |s| s.dev + s.len);
    let w0 = (first.dev + dev_off).div_ceil(PROBE_AU) * PROBE_AU - dev_off;
    let span = PROBE_AUS * PROBE_AU;
    (0..PROBE_WINDOWS)
        .map(|k| (w0 + k * span, w0 + (k + 1) * span))
        .take_while(|&(_, b)| b <= end)
        .map(|(a, b)| {
            let i = segs.partition_point(|s| s.dev < a);
            let j = segs.partition_point(|s| s.dev + s.len <= b);
            i..j.max(i)
        })
        .collect()
}

/// Issues `writes` with junk like the data phase would (ascending; spans ≥ PROBE_SOLO alone,
/// smaller ones `qd` in flight), then syncs. The seconds it took including the sync, or None
/// as soon as it passes `limit` seconds (it then cannot win).
fn run_trial(dev: &File, writes: &[(u64, u64)], qd: usize, buf: &[u8], limit: f64) -> Option<f64> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let t = Instant::now();
    let over = || t.elapsed().as_secs_f64() > limit;
    let mut k = 0;
    while k < writes.len() {
        if writes[k].1 >= PROBE_SOLO {
            let (mut off, end) = (writes[k].0, writes[k].0 + writes[k].1);
            while off < end {
                if over() {
                    return None;
                }
                let n = (end - off).min(buf.len() as u64);
                trace::serial(TOp::Write, off, n, || dev.write_all_at(&buf[..n as usize], off)).ok()?;
                off += n;
            }
            k += 1;
            continue;
        }
        let j = k + writes[k..].iter().position(|w| w.1 >= PROBE_SOLO).unwrap_or(writes.len() - k);
        let small = &writes[k..j];
        let (next, stop) = (AtomicUsize::new(0), AtomicBool::new(false));
        let grp = trace::group();
        let ok = std::thread::scope(|s| {
            let hs: Vec<_> = (0..qd.min(small.len()))
                .map(|_| {
                    s.spawn(|| {
                        loop {
                            if stop.load(Relaxed) {
                                return true;
                            }
                            let i = next.fetch_add(1, Relaxed);
                            let Some(&(off, len)) = small.get(i) else { return true };
                            if over() {
                                stop.store(true, Relaxed);
                                return true;
                            }
                            let w = || dev.write_all_at(&buf[..len as usize], off);
                            if trace::io(TOp::Write, off, len, qd, grp, off, w).is_err() {
                                stop.store(true, Relaxed);
                                return false;
                            }
                        }
                    })
                })
                .collect();
            hs.into_iter().all(|h| h.join().unwrap_or(false))
        });
        if !ok || over() {
            return None;
        }
        k = j;
    }
    trace::sync(|| dev.sync_all()).ok()?;
    let secs = t.elapsed().as_secs_f64();
    (secs <= limit).then_some(secs)
}

/// One candidate's result: seconds per byte of payload, None if it lost before finishing.
struct Trial {
    mode: GapMode,
    qd: usize,
    per_byte: Option<f64>,
}

fn mode_name(m: GapMode, qd: usize) -> String {
    match m {
        GapMode::Skip => format!("skip@{qd}"),
        GapMode::Fill => "fill".into(),
        GapMode::Pad(p) => format!("pad{}k@{qd}", p >> 10),
    }
}

/// Measures the gap-policy candidates IN the clusters this add is about to write: each in its
/// own window of PROBE_AUS consecutive 4 MB allocation units of the data stream, with the
/// exact writes the data phase would issue there ([`planned_writes`]), ending with a sync:
/// skip every gap, then zero-fill every gap, both at `qd` writes in flight (the transport's
/// default). Measured on a SanDisk Cruzer Blade (USB 2 BOT, 20k ~8 KB files onto a 32 KB
/// volume holding data, two runs each): skip@1 96-114 s, skip@2 88-157 s, skip@8 128-148 s,
/// fill 120-173 s, pad-to-16K 119-207 s. Run-to-run noise on that stick (±40 %) is larger
/// than any difference but fill's and pad's, so pad and the extra depths are not probed (a
/// forced policy can still pick them). Every byte written is in a cluster the pre-add bitmap calls free
/// (verify_allocation proved nothing references those), checked before anything is written;
/// the data phase overwrites the windows afterwards. A candidate stops as soon as it is
/// slower than the best so far, so the probe costs about (candidates × the best's time),
/// ~4 s on that stick. Returns the cheapest per byte of payload (filling, which writes more,
/// has to win by 5 %), and a summary extrapolated to `plan_payload` bytes.
fn probe_in_situ(
    dev: &File,
    segs: &[ProbeSeg],
    v: &Vol,
    dev_off: u64,
    plan_payload: u64,
    qd: usize,
) -> Option<(PatternChoice, String)> {
    let wins = probe_windows(segs, dev_off);
    if (wins.len() as u64) < PROBE_MIN_WINDOWS {
        return None;
    }
    let (pre, heap, cs) = (&v.bitmap, v.heap_off, v.cs);
    let mut buf = crate::util::AlignedBuf::new(PROBE_SOLO as usize);
    crate::util::Rng(0x5EED).fill(buf.as_mut());
    let t0 = Instant::now();
    let mut trials: Vec<Trial> = Vec::new();
    let best = |trials: &[Trial]| trials.iter().filter_map(|t| t.per_byte).fold(f64::INFINITY, f64::min);
    for (w, mode) in wins.into_iter().zip([GapMode::Skip, GapMode::Fill]) {
        let ws = &segs[w];
        let payload: u64 = ws.iter().map(|s| s.payload).sum();
        if payload == 0 {
            return None;
        }
        let writes = planned_writes(ws, &GapPolicy::of(mode, qd, ""), pre, heap, cs);
        // The hard rule, checked again on what is actually about to be written.
        let free = writes.iter().all(|&(o, l)| {
            o >= heap && all_free(pre, (o - heap) / cs + 2..(o + l - heap).div_ceil(cs) + 2) && o % 4096 == 0
        });
        if !free || writes.iter().any(|w| w.1 % 4096 != 0) {
            return None;
        }
        let limit = best(&trials) * payload as f64;
        let secs = run_trial(dev, &writes, qd, buf.as_ref(), if limit.is_finite() { limit } else { 60.0 });
        if secs.is_none() && trials.is_empty() {
            return None; // the first one has no limit: an I/O error
        }
        trials.push(Trial { mode, qd, per_byte: secs.map(|s| s / payload as f64) });
    }
    let mut win: Option<&Trial> = None;
    for t in trials.iter().filter(|t| t.per_byte.is_some()) {
        if win.is_none_or(|w| t.per_byte.unwrap() < w.per_byte.unwrap() * 0.95) {
            win = Some(t);
        }
    }
    let win = win?;
    let est = |t: &Trial| match t.per_byte {
        Some(p) => format!("{} {:.0} s", mode_name(t.mode, t.qd), p * plan_payload as f64),
        None => format!("{} > {:.0} s", mode_name(t.mode, t.qd), best(&trials) * plan_payload as f64),
    };
    let summary = format!(
        "measured here in {:.1} s, est.: {}",
        t0.elapsed().as_secs_f64(),
        trials.iter().map(est).collect::<Vec<_>>().join(", ")
    );
    Some((PatternChoice { mode: win.mode, qd: win.qd }, summary))
}

/// Test hook: force a candidate: `skip`, `fill`, `pad` (16 KB) or `pad<bytes>`, optionally
/// `:<writes in flight>`.
const GAP_POLICY_ENV: &str = "FLASHCP_TEST_GAP_POLICY";

fn forced_policy(v: &str, qd: usize) -> Option<GapPolicy> {
    let (m, q) = v.split_once(':').unwrap_or((v, ""));
    let mode = match m {
        "skip" => GapMode::Skip,
        "fill" => GapMode::Fill,
        "pad" => GapMode::Pad(PAD_UNIT),
        _ => GapMode::Pad(m.strip_prefix("pad")?.parse().ok().filter(|p: &u64| p.is_multiple_of(4096) && *p > 0)?),
    };
    let qd = if q.is_empty() {
        qd
    } else {
        q.parse().ok().filter(|&q| (1..=crate::devprofile::MAX_WRITE_QD).contains(&q))?
    };
    Some(GapPolicy::of(mode, qd, "test override"))
}

/// The gap policy for this target: what the in-situ probe chose for this device and pattern
/// class (cached), else a probe now in the clusters this add will write (block devices only,
/// never in a dry run), else a cached write model, else the fallback. Writes in flight never
/// come from a random-write benchmark: USB BOT gets 1 (strictly ascending), the rest 8.
/// Image files always use the fallback (their cost is the page cache's).
pub(crate) fn choose_gap_policy(dev: &File, direct: bool, v: &Vol, dev_off: u64, dry_run: bool, segs: &[ProbeSeg]) -> GapPolicy {
    use crate::devprofile as dp;
    let rdev = dp::block_dev_of(dev);
    let transport = rdev.and_then(|r| crate::probe::device_info_of(r).transport);
    let tq = if dp::is_bot(transport.as_deref()) { dp::BOT_WRITE_QD } else { dp::DEFAULT_WRITE_QD };
    if let Some(p) = std::env::var(GAP_POLICY_ENV).ok().and_then(|s| forced_policy(&s, tq)) {
        return p;
    }
    if let Some(max_fill) = std::env::var(GAP_FILL_ENV).ok().and_then(|v| v.parse().ok()) {
        let qd = dp::DEFAULT_WRITE_QD;
        return GapPolicy { max_fill, free_too: true, pad: MIN_PAD, qd, mode: None, source: "test override".into() };
    }
    let Some(rdev) = rdev else { return GapPolicy::fallback("image default") };
    let mut sizes: Vec<u64> = segs.iter().filter(|s| s.file_end).map(|s| s.payload).collect();
    let mid = sizes.len() / 2;
    let median = if sizes.is_empty() { 0 } else { *sizes.select_nth_unstable(mid).1 };
    let key = dp::pattern_key(v.cs, median);
    let cached = dp::load(rdev).unwrap_or_default();
    if let Some(c) = cached.patterns.get(&key) {
        return GapPolicy::of(c.mode, c.qd, &format!("measured for {key} earlier"));
    }
    let unmeasured = |why: &str| {
        if cached.has_write_model() {
            GapPolicy::from_profile(&cached, tq, &format!("cached write model; {why}"))
        } else {
            GapPolicy::fallback_qd(&format!("default; {why}"), tq)
        }
    };
    if dry_run {
        return unmeasured("not measured in a dry run");
    }
    if !direct || (probe_windows(segs, dev_off).len() as u64) < PROBE_MIN_WINDOWS {
        return unmeasured("too little to measure in");
    }
    if !set_direct(dev, true) {
        return unmeasured("no O_DIRECT");
    }
    let payload: u64 = segs.iter().map(|s| s.payload).sum();
    trace::phase("probe");
    let mut measured = probe_in_situ(dev, segs, v, dev_off, payload, tq);
    set_direct(dev, false);
    // Recording for a simulated device: the image's timing says nothing about that device.
    if let (Some(c), Some(m)) = (crate::sim::device::sim_device().and_then(|d| d.probe_choice), measured.as_mut()) {
        *m = (c, format!("simulated device: {}", mode_name(c.mode, c.qd)));
    }
    let Some((choice, summary)) = measured else { return unmeasured("measuring failed") };
    if std::env::var_os(GAPS_ENV).is_some() {
        eprintln!("probe {key}: {summary} → {}", mode_name(choice.mode, choice.qd));
    }
    let p = dp::DevProfile {
        transport: Some(transport.unwrap_or_else(|| "other".into())),
        patterns: [(key, choice)].into(),
        ..Default::default()
    };
    dp::save(rdev, &p);
    GapPolicy::of(choice.mode, choice.qd, &summary)
}

// ───────────────────────── patches ─────────────────────────

/// Small writes for one metadata phase; committed ascending and coalesced, then synced.
#[derive(Default)]
pub(crate) struct Patches(Vec<(u64, Vec<u8>)>);

impl Patches {
    pub(crate) fn add(&mut self, off: u64, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.0.push((off, bytes));
        }
    }
    pub(crate) fn fat(&mut self, v: &Vol, entries: &[(u32, u32)]) {
        for &(c, n) in entries {
            self.add(v.fat_off + c as u64 * 4, n.to_le_bytes().to_vec());
        }
    }
    /// Writes `bytes` at byte `off` of an object stored in `clusters`.
    pub(crate) fn object(&mut self, v: &Vol, clusters: &[u32], mut off: usize, mut bytes: &[u8]) {
        let cs = v.cs as usize;
        while !bytes.is_empty() {
            let n = (cs - off % cs).min(bytes.len());
            self.add(v.cluster_off(clusters[off / cs]) + (off % cs) as u64, bytes[..n].to_vec());
            off += n;
            bytes = &bytes[n..];
        }
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.0.iter().map(|p| p.1.len() as u64).sum()
    }
    pub(crate) fn commit(mut self, dev: &File) -> Result<()> {
        self.0.sort_by_key(|p| p.0);
        let mut i = 0;
        while i < self.0.len() {
            let (off, mut buf) = (self.0[i].0, self.0[i].1.clone());
            let mut j = i + 1;
            while j < self.0.len() && self.0[j].0 <= off + buf.len() as u64 {
                ensure!(self.0[j].0 == off + buf.len() as u64, "internal error: overlapping metadata writes");
                buf.extend_from_slice(&self.0[j].1);
                j += 1;
            }
            trace::serial(TOp::Write, off, buf.len() as u64, || dev.write_all_at(&buf, off))
                .with_context(|| format!("write metadata at {off}"))?;
            i = j;
        }
        trace::sync(|| dev.sync_all()).context("sync")?;
        Ok(())
    }
}

pub(crate) fn stop_after(phase: &str) -> Result<()> {
    if std::env::var(STOP_ENV).as_deref() == Ok(phase) {
        bail!("stopped after the {phase} phase ({STOP_ENV})");
    }
    Ok(())
}

/// VolumeFlags while modifying: dirty set, ClearToZero cleared (spec 3.1.13.4).
pub(crate) fn working_flags(flags: u16) -> u16 {
    (flags | FLAG_DIRTY) & !FLAG_CLEAR_TO_ZERO
}

/// VolumeFlags when done: dirty cleared, ClearToZero stays cleared.
pub(crate) fn clean_flags(flags: u16) -> u16 {
    flags & !(FLAG_DIRTY | FLAG_CLEAR_TO_ZERO)
}

pub(crate) fn write_flags(v: &mut Vol, flags: u16, pct: Option<u8>) -> Result<()> {
    v.sector0[106..108].copy_from_slice(&flags.to_le_bytes());
    if let Some(p) = pct {
        v.sector0[112] = p;
    }
    trace::serial(TOp::Write, 0, v.sector0.len() as u64, || v.dev.write_all_at(&v.sector0, 0))
        .context("write boot sector")?;
    trace::sync(|| v.dev.sync_all()).context("sync")?;
    v.flags = flags;
    Ok(())
}

// ───────────────────────── the data phase (shared with sync) ─────────────────────────

/// Bytes the data phase writes for `segs` (a file's last write padded to 4 KB within its run).
/// `sizes[k]` is the size of the file `SegKind::File(k, _)` belongs to.
pub(crate) fn data_write_bytes(segs: &[Seg], sizes: &[u64]) -> u64 {
    segs.iter()
        .map(|s| match s.kind {
            SegKind::File(k, off) => {
                let left = sizes[k] - off;
                if left <= s.len { left.div_ceil(4096) * 4096 } else { s.len }.min(s.len)
            }
            SegKind::Mem(..) => s.len,
        })
        .sum()
}

/// The gap policy for the data phase over `segs` (see [`choose_gap_policy`]); `direct` is
/// whether O_DIRECT may be used at all (target support and 4 KB alignment).
pub(crate) fn data_gap_policy(
    dev: &File,
    direct: bool,
    v: &Vol,
    dev_off: u64,
    dry_run: bool,
    segs: &[Seg],
    sizes: &[u64],
) -> GapPolicy {
    if segs.is_empty() {
        return GapPolicy::fallback("nothing to write");
    }
    let probe_segs: Vec<ProbeSeg> = segs
        .iter()
        .map(|s| match s.kind {
            SegKind::File(k, off) => {
                let payload = (sizes[k] - off).min(s.len);
                ProbeSeg { dev: s.dev, len: s.len, payload, file_end: off + payload == sizes[k] }
            }
            SegKind::Mem(..) => ProbeSeg { dev: s.dev, len: s.len, payload: s.len, file_end: false },
        })
        .collect();
    choose_gap_policy(dev, direct, v, dev_off, dry_run, &probe_segs)
}

/// Phase 1: writes `segs` (sorted by address, all in clusters the pre-write bitmap `v.bitmap`
/// calls free) as one ascending stream through the sequential writer, then syncs. `reads[k]`
/// is the source file and size of `SegKind::File(k, _)`; `mems[m]` the bytes of
/// `SegKind::Mem(m, _)`. `direct`: O_DIRECT may be used (switched on for the stream only).
/// Progress counts files from `files_done` on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_data(
    excl: &Arc<File>,
    direct: bool,
    v: &Vol,
    segs: &[Seg],
    mems: &[Vec<u8>],
    reads: Vec<(PathBuf, u64)>,
    pol: &GapPolicy,
    readers: usize,
    tick: &mut Ticker<'_>,
    files_done: u64,
) -> Result<()> {
    if segs.is_empty() {
        return Ok(());
    }
    let cs = v.cs;
    let direct = direct && set_direct(excl, true);
    let mut w = if direct { SeqWriter::new(excl.clone()) } else { SeqWriter::with_align(excl.clone(), 1) };
    w.set_inflight(pol.qd);
    let files = reads.clone();
    let (rx, budget) = ordered_reader(reads, readers.max(1), 256 << 20, 16 << 20);
    let mut pending: BTreeMap<usize, Item> = BTreeMap::new();
    enum Cur {
        Data(Vec<u8>),
        Stream(File),
    }
    let mut cur: Option<(usize, Cur)> = None;
    let mut last_k = 0usize;
    let (mut files_done, mut bytes_done) = (files_done, 0u64);
    let mut prev_end: Option<u64> = None;
    let (pre_bitmap, heap) = (&v.bitmap, v.heap_off);
    let (mut filled, mut skipped_gaps) = (0u64, 0u64);
    for s in segs {
        // Unwritten cluster slack after a file's data is not part of the file (beyond
        // ValidDataLength), so a gap may be skipped or zero-filled, whichever the device
        // does faster (fill_gap). Clusters between our runs are only ever filled when the
        // pre-add bitmap calls them free; segments are whole clusters, so prev_end and
        // s.dev are cluster boundaries.
        if s.dev > w.cur() {
            if pol.fills(w.cur(), s.dev, prev_end, pre_bitmap, heap, cs) {
                filled += s.dev - w.cur();
                w.zeros_to(s.dev)?;
            } else {
                skipped_gaps += 1;
                w.skip_to(s.dev)?;
            }
        }
        ensure!(w.cur() == s.dev, "internal layout error: overlapping data segments");
        prev_end = Some(s.dev + s.len);
        match s.kind {
            SegKind::Mem(m, off) => w.write(&mems[m][off..off + s.len as usize])?,
            SegKind::File(k, off) => {
                let (src, size) = (&files[k].0, files[k].1);
                if cur.as_ref().map(|c| c.0) != Some(k) {
                    ensure!(cur.is_none() && k >= last_k, "internal layout error: interleaved files");
                    while !pending.contains_key(&k) {
                        let (idx, item) = rx.recv().context("reader stopped")?;
                        pending.insert(idx, item);
                    }
                    let c = match pending.remove(&k).unwrap() {
                        Item::Data(d) => {
                            ensure!(d.len() as u64 == size, "{} changed size while copying", src.display());
                            Cur::Data(d)
                        }
                        Item::Stream => {
                            let f = File::open(src).with_context(|| format!("open {}", src.display()))?;
                            crate::util::fadvise_sequential(&f);
                            Cur::Stream(f)
                        }
                    };
                    cur = Some((k, c));
                    last_k = k;
                }
                let take = (size - off).min(s.len);
                match &mut cur.as_mut().unwrap().1 {
                    Cur::Data(d) => w.write(&d[off as usize..(off + take) as usize])?,
                    Cur::Stream(f) => w.write_from(f, take, None)?,
                }
                if off + take == size {
                    // Pad to the I/O alignment (or the policy's unit); the rest of the
                    // cluster is slack. Never past the run's end: with clusters under 4 KB
                    // the next cluster may belong to someone else.
                    w.zeros_to(pol.pad_end(w.cur(), s.dev + s.len))?;
                    if matches!(cur.take(), Some((_, Cur::Data(_)))) {
                        release(&budget, size.min(256 << 20));
                    }
                    files_done += 1;
                    bytes_done += size;
                    tick.progress(files_done, bytes_done, false);
                }
            }
        }
    }
    ensure!(cur.is_none(), "internal layout error: a file was not fully written");
    if std::env::var_os(GAPS_ENV).is_some() {
        eprintln!("gaps: {filled} bytes zero-filled, {skipped_gaps} skipped");
    }
    w.finish()?; // syncs
    set_direct(excl, false); // metadata phases are small buffered writes
    Ok(())
}

// ───────────────────────── add ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Obj {
    /// Extension clusters of an existing directory.
    Ext(usize),
    NewDir(usize),
    File(usize),
}

pub(crate) enum SegKind {
    Mem(usize, usize),
    /// (index into file_order, byte offset in the file)
    File(usize, u64),
}

pub(crate) struct Seg {
    pub(crate) dev: u64,
    pub(crate) len: u64,
    pub(crate) kind: SegKind,
}

pub fn add(src: &Path, target: &Path, o: &AddOpts, sink: Sink<'_>) -> Result<AddStats> {
    add_to(src, &TargetSpec::from(target), o, sink)
}

/// [`add`] onto a [`TargetSpec`] (a path, or an inherited block-device fd).
pub fn add_to(src: &Path, target: &TargetSpec, o: &AddOpts, sink: Sink<'_>) -> Result<AddStats> {
    let t0 = Instant::now();
    let mut tick = Ticker::new(sink);
    ensure!(
        fs::metadata(src).with_context(|| format!("source {}", src.display()))?.is_dir(),
        "source must be a directory"
    );
    let tgt = target.open_rw()?;
    // An empty volume with coarse clusters (a fresh stick) is written as a build instead.
    let tgt = match crate::add_build::empty_as_build(src, tgt, o.to.as_deref(), o.readers, o.dry_run, tick.sink())? {
        Ok(stats) => return Ok(stats),
        Err(tgt) => tgt,
    };
    // The one open file description (exclusively claimed: O_EXCL / flock) is used for
    // everything and stays open until we return, so nobody can mount the device mid-way.
    // Nothing is reopened by path. Metadata I/O is buffered; O_DIRECT is switched on (F_SETFL)
    // only for the data phase. Duplicated fds share the description, hence the flags.
    let excl = Arc::new(tgt.file);
    set_direct(&excl, false);
    let mut v = Vol::open(excl.try_clone().context("dup target fd")?, tgt.bytes, false, true)?;
    let cs = v.cs;
    let mut nodes = scan_tree(src, &v.upc)?;
    // Mandatory: never trust a clear bitmap bit that something still references. The walk
    // keeps the directories planning may touch (the --to path and every source directory
    // path under it), so planning uses exactly what was verified.
    let mut wanted: HashSet<Vec<Vec<u16>>> = HashSet::new();
    let mut dest: Vec<Vec<u16>> = Vec::new();
    wanted.insert(dest.clone());
    for comp in o.to.as_deref().unwrap_or("").split('/').filter(|c| !c.is_empty() && *c != ".") {
        dest.push(upcase(&comp.encode_utf16().collect::<Vec<_>>(), &v.upc));
        wanted.insert(dest.clone());
    }
    let mut q = vec![(0usize, dest)];
    while let Some((n, key)) = q.pop() {
        for &c in &nodes[n].children {
            if nodes[c].is_dir {
                let mut k = key.clone();
                k.push(upcase(&nodes[c].name, &v.upc));
                wanted.insert(k.clone());
                q.push((c, k));
            }
        }
    }
    let tv = Instant::now();
    let verified = v.verify_allocation(&wanted)?;
    let verify_secs = tv.elapsed().as_secs_f64();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let now = (now.as_secs() as i64, now.subsec_nanos());

    // ── destination (--to), creating missing parents as new directories ──
    let mut appends: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut cur = 0usize;
    let mut new_parent: Option<usize> = None;
    for comp in o.to.as_deref().unwrap_or("").split('/').filter(|c| !c.is_empty() && *c != ".") {
        ensure!(comp != "..", "--to must not contain '..'");
        let name: Vec<u16> = comp.encode_utf16().collect();
        validate_name(&name, &v.upc, &mut HashSet::new(), Path::new(comp))?;
        let synth = |nodes: &mut Vec<Node>| {
            nodes.push(Node {
                name: name.clone(),
                src: PathBuf::new(),
                is_dir: true,
                size: 0,
                mtime: now,
                children: vec![],
                first_cluster: 0,
                clusters: 0,
            });
            nodes.len() - 1
        };
        if let Some(np) = new_parent {
            let k = synth(&mut nodes);
            nodes[np].children.push(k);
            new_parent = Some(k);
            continue;
        }
        match v.dirs[cur].children.get(&upcase(&name, &v.upc)).cloned() {
            Some(ch) if ch.is_dir => cur = v.load_child(cur, &ch)?,
            Some(_) => bail!("--to: {}/{comp} exists on the volume and is not a directory", v.dirs[cur].path),
            None => {
                let k = synth(&mut nodes);
                appends.entry(cur).or_default().push(k);
                new_parent = Some(k);
            }
        }
    }

    // ── merge the source tree into the destination ──
    let mut skipped = 0u64;
    let mut collisions = Vec::new();
    let top = nodes[0].children.clone();
    if let Some(np) = new_parent {
        nodes[np].children = top;
    } else {
        let mut work = vec![(cur, top)];
        while let Some((d, kids)) = work.pop() {
            for c in kids {
                let key = upcase(&nodes[c].name, &v.upc);
                match v.dirs[d].children.get(&key).cloned() {
                    None => appends.entry(d).or_default().push(c),
                    Some(ch) if ch.is_dir && nodes[c].is_dir => {
                        let nd = v.load_child(d, &ch)?;
                        work.push((nd, nodes[c].children.clone()));
                    }
                    Some(ch) => {
                        if o.skip_existing {
                            skipped += 1;
                        } else {
                            collisions.push(format!("{}/{}", v.dirs[d].path, String::from_utf16_lossy(&ch.name)));
                        }
                    }
                }
            }
        }
    }
    if !collisions.is_empty() {
        collisions.sort();
        let n = collisions.len();
        let list: Vec<_> = collisions.into_iter().take(10).collect();
        bail!(
            "{n} name(s) already exist on the volume (names are case-insensitive), e.g. {}; pass --skip-existing to skip them",
            list.join(", ")
        );
    }

    // ── what is new: directories (BFS) and files ──
    let mut new_dirs = Vec::new();
    let mut file_parents: Vec<Vec<usize>> = Vec::new(); // file children, grouped per parent
    for kids in appends.values() {
        file_parents.push(kids.iter().copied().filter(|&c| !nodes[c].is_dir).collect());
        new_dirs.extend(kids.iter().copied().filter(|&c| nodes[c].is_dir));
    }
    let mut q = 0;
    while q < new_dirs.len() {
        let d = new_dirs[q];
        file_parents.push(nodes[d].children.iter().copied().filter(|&c| !nodes[c].is_dir).collect());
        let sub: Vec<usize> = nodes[d].children.iter().copied().filter(|&c| nodes[c].is_dir).collect();
        new_dirs.extend(sub);
        q += 1;
    }
    let new_files: Vec<usize> = file_parents.into_iter().flatten().collect();
    let data_bytes: u64 = new_files.iter().map(|&i| nodes[i].size).sum();

    // ── sizes ──
    let mut ext_need: BTreeMap<usize, u64> = BTreeMap::new(); // existing dir → extension clusters
    for (&d, kids) in &appends {
        let need: u64 = kids.iter().map(|&c| set_bytes(&nodes[c])).sum();
        let dir = &v.dirs[d];
        let free = (dir.data.len() - dir.end) as u64;
        if need > free {
            let n = (need - free).div_ceil(cs);
            ensure!(
                dir.data.len() as u64 + n * cs <= MAX_DIR_BYTES,
                "directory {} would grow past the 256 MB exFAT limit",
                if dir.path.is_empty() { "/" } else { &dir.path }
            );
            ext_need.insert(d, n);
        }
    }
    for &d in &new_dirs {
        let bytes: u64 = nodes[d].children.iter().map(|&c| set_bytes(&nodes[c])).sum();
        nodes[d].clusters = bytes.div_ceil(cs).max(1);
        ensure!(nodes[d].clusters * cs <= MAX_DIR_BYTES, "a source directory has too many entries for exFAT");
    }
    for &f in &new_files {
        nodes[f].clusters = nodes[f].size.div_ceil(cs);
    }

    // ── allocation ──
    let mut free = free_extents(&v.bitmap, v.cc);
    let free_total: u64 = free.iter().map(|e| e.1).sum();
    let mut runs: BTreeMap<Obj, Vec<(u64, u64)>> = BTreeMap::new();
    // Existing NoFatChain directories keep NoFatChain if the clusters right after them are free.
    for (&d, &n) in &ext_need {
        let dir = &v.dirs[d];
        if dir.no_fat_chain {
            let next = *dir.clusters.last().unwrap() as u64 + 1;
            if take_exact(&mut free, next, n) {
                runs.insert(Obj::Ext(d), vec![(next, n)]);
            }
        }
    }
    let mut order: Vec<(Obj, u64)> = Vec::new();
    for (&d, &n) in &ext_need {
        if !runs.contains_key(&Obj::Ext(d)) {
            order.push((Obj::Ext(d), n));
        }
    }
    order.extend(new_dirs.iter().map(|&d| (Obj::NewDir(d), nodes[d].clusters)));
    order.extend(new_files.iter().filter(|&&f| nodes[f].clusters > 0).map(|&f| (Obj::File(f), nodes[f].clusters)));
    let total: u64 = order.iter().map(|e| e.1).sum();
    let Some(chosen) = choose_extents(&free, total) else {
        let need: u64 = total + runs.values().flatten().map(|r| r.1).sum::<u64>();
        bail!(
            "not enough free space: need {} ({need} clusters), the volume has {} free",
            human_bytes((need * cs) as f64),
            human_bytes((free_total * cs) as f64)
        );
    };
    let mut ext_q: std::collections::VecDeque<_> = chosen.into();
    for &(obj, n) in &order {
        runs.insert(obj, carve(&mut ext_q, n));
    }
    let frag = |obj: Obj| runs.get(&obj).is_some_and(|r| r.len() > 1);
    for (&obj, r) in &runs {
        if let Obj::NewDir(i) | Obj::File(i) = obj {
            nodes[i].first_cluster = r[0].0 as u32;
        }
    }
    let set_for = |nodes: &[Node], i: usize, fragmented: bool| {
        let n = &nodes[i];
        let mut e = entry_set(n, &v.upc);
        if fragmented {
            e[33] = 0x01; // AllocationPossible, FAT chain
        }
        let len = if n.is_dir { n.clusters * cs } else { n.size };
        finish_set(e, len, len)
    };
    let obj_of = |i: usize| if nodes[i].is_dir { Obj::NewDir(i) } else { Obj::File(i) };

    // ── phase-1 content: new directories, extension clusters; segments sorted by address ──
    let mut mems: Vec<Vec<u8>> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    let push_mem = |segs: &mut Vec<Seg>, mems: &mut Vec<Vec<u8>>, r: &[(u64, u64)], mut bytes: Vec<u8>| {
        let total: u64 = r.iter().map(|x| x.1).sum::<u64>() * cs;
        bytes.resize(total as usize, 0);
        let m = mems.len();
        mems.push(bytes);
        let mut off = 0usize;
        for &(s, n) in r {
            segs.push(Seg { dev: v.cluster_off(s as u32), len: n * cs, kind: SegKind::Mem(m, off) });
            off += (n * cs) as usize;
        }
    };
    for &d in &new_dirs {
        let mut bytes = Vec::new();
        for &c in &nodes[d].children {
            bytes.extend(set_for(&nodes, c, frag(obj_of(c))));
        }
        push_mem(&mut segs, &mut mems, &runs[&Obj::NewDir(d)], bytes);
    }
    let mut p3_fat = Patches::default();
    let mut p3_dir = Patches::default();
    let mut ext_fat: Vec<(u32, u32)> = Vec::new();
    for (&d, kids) in &appends {
        let mut bytes = Vec::new();
        for &c in kids {
            bytes.extend(set_for(&nodes, c, frag(obj_of(c))));
        }
        let dir = &v.dirs[d];
        let room = dir.data.len() - dir.end;
        let tail = bytes.len().min(room);
        let clusters = dir.clusters.clone();
        p3_dir.object(&v, &clusters, dir.end, &bytes[..tail]);
        let after = dir.end + tail;
        if dir.data[after..].iter().any(|&b| b != 0) {
            // Everything after the end-of-directory marker must read as "end".
            p3_dir.object(&v, &clusters, after, &vec![0u8; dir.data.len() - after]);
        }
        let Some(r) = runs.get(&Obj::Ext(d)).cloned() else { continue };
        push_mem(&mut segs, &mut mems, &r, bytes[tail..].to_vec());
        let ext = run_clusters(&r);
        let contiguous_ext = dir.no_fat_chain && r.len() == 1 && r[0].0 == *clusters.last().unwrap() as u64 + 1;
        let new_len = (clusters.len() + ext.len()) as u64 * cs;
        if !contiguous_ext {
            // Existing chain → extension. A NoFatChain directory needs its chain written first.
            let mut all = if dir.no_fat_chain { clusters.clone() } else { vec![*clusters.last().unwrap()] };
            all.extend(&ext);
            chain_entries(&all, &mut ext_fat);
        }
        if let Some((p, off, len)) = dir.entry {
            let mut set = v.dirs[p].data[off..off + len].to_vec();
            if !contiguous_ext {
                set[33] &= !0x02;
            }
            let set = finish_set(set, new_len, new_len);
            let pc = v.dirs[p].clusters.clone();
            p3_dir.object(&v, &pc, off, &set);
        }
    }
    p3_fat.fat(&v, &ext_fat);

    // ── phase-2: bitmap + FAT chains of fragmented new objects ──
    let mut p2 = Patches::default();
    let mut bm = v.bitmap.clone();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut new_fat: Vec<(u32, u32)> = Vec::new();
    for (&obj, r) in &runs {
        for &(s, n) in r {
            for c in s..s + n {
                let i = (c - 2) as usize;
                bm[i / 8] |= 1 << (i % 8);
            }
            ranges.push((((s - 2) / 8) as usize, ((s - 2 + n - 1) / 8) as usize + 1));
        }
        if !matches!(obj, Obj::Ext(_)) && r.len() > 1 {
            chain_entries(&run_clusters(r), &mut new_fat);
        }
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let bmc = v.bitmap_clusters.clone();
    for (a, b) in merged {
        p2.object(&v, &bmc, a, &bm[a..b]);
    }
    p2.fat(&v, &new_fat);

    // ── plan ──
    segs.sort_by_key(|s| s.dev);
    let file_order: Vec<usize> = new_files.iter().copied().filter(|&f| nodes[f].size > 0).collect();
    let pos_of: HashMap<usize, usize> = file_order.iter().enumerate().map(|(k, &f)| (f, k)).collect();
    for &f in &file_order {
        let mut off = 0u64;
        for &(s, n) in &runs[&Obj::File(f)] {
            segs.push(Seg { dev: v.cluster_off(s as u32), len: n * cs, kind: SegKind::File(pos_of[&f], off) });
            off += n * cs;
        }
    }
    segs.sort_by_key(|s| s.dev);
    let sizes: Vec<u64> = file_order.iter().map(|&f| nodes[f].size).collect();
    let data_write = data_write_bytes(&segs, &sizes);
    let write_bytes = data_write + p2.bytes() + p3_fat.bytes() + p3_dir.bytes() + 2 * v.bps;
    let fragmented = runs.iter().filter(|(o, r)| !matches!(o, Obj::Ext(_)) && r.len() > 1).count() as u64;
    let (files, dirs) = (new_files.len() as u64, new_dirs.len() as u64);
    // Measuring writes into the free clusters this add is about to write, so it happens
    // before anything else is written.
    let direct = tgt.direct && cs.is_multiple_of(4096) && v.heap_off.is_multiple_of(4096);
    let pol = data_gap_policy(&excl, direct, &v, tgt.part_off * 512, o.dry_run, &segs, &sizes);
    let detail = format!(
        "plan: add {files} files, {dirs} dirs, {} data │ cluster {} │ {} free │ {} new objects fragmented │ will write {}{} │ {} │ verified {verified} objects in {:.0} ms",
        human_bytes(data_bytes as f64),
        human_bytes(cs as f64),
        human_bytes((free_total * cs) as f64),
        fragmented,
        human_bytes(write_bytes as f64),
        if skipped > 0 { format!(" │ skipping {skipped} existing") } else { String::new() },
        pol.describe(),
        verify_secs * 1e3
    );
    tick.emit(Event::Plan { op: Op::Add, files, dirs, bytes: data_bytes, write_bytes, detail, strategy: None });
    let stats = |written: u64| AddStats {
        files,
        dirs,
        skipped,
        data_bytes,
        written,
        fragmented,
        cluster: cs,
        secs: t0.elapsed().as_secs_f64(),
    };
    if o.dry_run || (files == 0 && dirs == 0) {
        tick.emit(Event::Done { files, bytes: data_bytes, secs: t0.elapsed().as_secs_f64() });
        return Ok(stats(0));
    }

    // ── phase 0: mark dirty ──
    trace::note("gap_policy", || crate::events::json_str(&pol.describe()));
    trace::phase("dirty");
    let flags = v.flags;
    write_flags(&mut v, working_flags(flags), None)?;
    stop_after("dirty")?;

    // ── phase 1: data, one ascending stream ──
    trace::phase("data");
    let empty_files = (files as usize - file_order.len()) as u64;
    tick.progress(empty_files, 0, true);
    let reads: Vec<(PathBuf, u64)> = file_order.iter().map(|&i| (nodes[i].src.clone(), nodes[i].size)).collect();
    write_data(&excl, direct, &v, &segs, &mems, reads, &pol, o.readers, &mut tick, empty_files)?;
    stop_after("data")?;

    // ── phase 2: bitmap + new chains ──
    trace::phase("alloc");
    p2.commit(&v.dev)?;
    v.bitmap = bm;
    stop_after("alloc")?;

    // ── phase 3: link into existing chains, then directory entries ──
    trace::phase("fat+dir");
    p3_fat.commit(&v.dev)?;
    stop_after("fat")?;
    p3_dir.commit(&v.dev)?;

    // ── phase 4: clean ──
    trace::phase("clean");
    let used = v.cc - v.free_clusters();
    let pct = (used * 100 / v.cc).min(100) as u8;
    write_flags(&mut v, clean_flags(flags), Some(pct))?;
    tick.progress(files, data_bytes, true);
    if !tgt.direct {
        eprintln!("note: target does not support O_DIRECT, used buffered writes");
    }
    let s = stats(write_bytes);
    tick.emit(Event::Done { files, bytes: data_bytes, secs: s.secs });
    drop(excl);
    Ok(s)
}

// ───────────────────────── info ─────────────────────────

/// Read-only summary of an exFAT volume (needs only read access; no exclusive open).
pub fn info(target: &Path) -> Result<VolumeInfo> {
    info_of(&TargetSpec::from(target))
}

/// [`info`] of a [`TargetSpec`]; an inherited fd may be O_RDONLY and must be a block device.
pub fn info_of(target: &TargetSpec) -> Result<VolumeInfo> {
    let (f, len) = match target {
        TargetSpec::Fd(fd) => {
            let t = open_target_fd(*fd, false)?;
            (t.file, t.bytes)
        }
        TargetSpec::Path(target) => {
            let f = File::open(target).with_context(|| format!("open {}", target.display()))?;
            let md = f.metadata()?;
            let len = if md.file_type().is_block_device() {
                crate::exfat::blk_size(&f)?
            } else if md.is_file() {
                md.len()
            } else {
                bail!("{} is neither a block device nor a regular image file", target.display());
            };
            (f, len)
        }
    };
    let mut v = Vol::open(f, len, true, true)?;
    let (mut files, mut dirs) = (0u64, 0u64);
    // Directories are read one at a time and dropped, so memory stays bounded.
    let mut stack: Vec<Dir> = vec![v.dirs.pop().ok_or_else(|| anyhow!("no root directory"))?];
    while let Some(d) = stack.pop() {
        for ch in d.children.values() {
            if ch.is_dir {
                dirs += 1;
                // Every directory owns at least one cluster, so more means a cycle.
                ensure!(dirs <= v.cc, "directory tree has a cycle; run fsck.exfat");
                stack.push(v.read_child(0, &d.path, ch)?);
            } else {
                files += 1;
            }
        }
    }
    Ok(VolumeInfo {
        label: v.label.clone(),
        cluster: v.cs,
        size: v.cc * v.cs,
        free: v.free_clusters() * v.cs,
        files,
        dirs,
        dirty: v.flags & FLAG_DIRTY != 0,
    })
}

/// Whether a regular image file is attached to a loop device (and so may be mounted).
pub(crate) fn loop_device_of(md: &fs::Metadata) -> Option<String> {
    for e in fs::read_dir("/sys/block").ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("loop") {
            continue;
        }
        let Ok(bf) = fs::read_to_string(e.path().join("loop/backing_file")) else { continue };
        let bf = bf.trim_end_matches('\n');
        let bf = bf.strip_suffix(" (deleted)").unwrap_or(bf);
        if let Ok(m) = fs::metadata(bf)
            && m.dev() == md.dev()
            && m.ino() == md.ino()
        {
            return Some(format!("/dev/{name}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_extents_from_bitmap() {
        // clusters 2..=4 used, 5..=9 free, 10 used, 11..=25 free (cc = 24)
        let cc = 24u64;
        let mut bm = vec![0u8; 3];
        for i in [0usize, 1, 2, 8] {
            bm[i / 8] |= 1 << (i % 8);
        }
        assert_eq!(free_extents(&bm, cc), vec![(5, 5), (11, 15)]);
        assert_eq!(free_extents(&[0xFF, 0xFF], 16), vec![]);
        assert_eq!(free_extents(&[0, 0], 16), vec![(2, 16)]);
        assert_eq!(free_extents(&[0xFF, 0x00, 0xFF, 0x01], 26), vec![(10, 8), (27, 1)]);
    }

    #[test]
    fn choose_prefers_best_single_fit_then_biggest() {
        let free = [(10, 5), (100, 50), (200, 8), (300, 20)];
        assert_eq!(choose_extents(&free, 7), Some(vec![(200, 7)]));
        assert_eq!(choose_extents(&free, 50), Some(vec![(100, 50)]));
        assert_eq!(choose_extents(&free, 60), Some(vec![(100, 50), (300, 10)]));
        assert_eq!(choose_extents(&free, 83), Some(vec![(10, 5), (100, 50), (200, 8), (300, 20)]));
        assert_eq!(choose_extents(&free, 84), None);
        assert_eq!(choose_extents(&free, 0), Some(vec![]));
    }

    #[test]
    fn carve_splits_across_extents() {
        let mut q: std::collections::VecDeque<_> = vec![(10u64, 3u64), (20, 4)].into();
        assert_eq!(carve(&mut q, 2), vec![(10, 2)]);
        assert_eq!(carve(&mut q, 3), vec![(12, 1), (20, 2)]);
        assert_eq!(carve(&mut q, 2), vec![(22, 2)]);
        assert!(q.is_empty());
    }

    #[test]
    fn take_exact_splits_extent() {
        let mut f = vec![(10u64, 10u64), (30, 5)];
        assert!(take_exact(&mut f, 12, 3));
        assert_eq!(f, vec![(10, 2), (15, 5), (30, 5)]);
        assert!(!take_exact(&mut f, 18, 4));
        assert!(take_exact(&mut f, 30, 5));
        assert_eq!(f, vec![(10, 2), (15, 5)]);
    }

    #[test]
    fn volume_flags_clear_to_zero_is_cleared() {
        for f in [0u16, FLAG_CLEAR_TO_ZERO, FLAG_CLEAR_TO_ZERO | 0x10] {
            assert_eq!(working_flags(f) & FLAG_CLEAR_TO_ZERO, 0, "working {f:#x}");
            assert_ne!(working_flags(f) & FLAG_DIRTY, 0, "working {f:#x}");
            assert_eq!(clean_flags(working_flags(f)) & (FLAG_CLEAR_TO_ZERO | FLAG_DIRTY), 0, "clean {f:#x}");
        }
        assert_eq!(clean_flags(working_flags(0x10)), 0x10, "other bits are preserved");
    }

    #[test]
    fn chain_entries_end_with_eoc() {
        let mut v = Vec::new();
        chain_entries(&[5, 6, 9], &mut v);
        assert_eq!(v, vec![(5, 6), (6, 9), (9, EOC)]);
    }

    fn profile(bps: f64, skip: f64) -> crate::devprofile::DevProfile {
        crate::devprofile::DevProfile { seq_write_bps: Some(bps), write_skip_s: Some(skip), ..Default::default() }
    }

    #[test]
    fn gap_threshold_is_the_cost_crossover() {
        // 6.5 MB/s, 2 ms per extra request: filling is cheaper below 13 000 bytes.
        assert!(worth_filling(12_999, 6.5e6, 0.002));
        assert!(!worth_filling(13_000, 6.5e6, 0.002));
        let p = GapPolicy::from_profile(&profile(6.5e6, 0.002), 2, "t");
        assert_eq!(p.max_fill, 12_999);
        assert!(p.free_too);
        // max_fill is exactly the largest gap worth_filling accepts, for awkward numbers too.
        for (bps, skip) in [(9_708_427.533, 0.000_869_66), (1e9, 1e-4), (3.3e6, 1.0 / 3.0), (6e6, 0.0)] {
            let p = GapPolicy::from_profile(&profile(bps, skip), 2, "t");
            assert!(p.max_fill == 0 || worth_filling(p.max_fill, bps, skip), "{bps} {skip}");
            assert!(!worth_filling(p.max_fill + 1, bps, skip), "{bps} {skip}");
        }
        assert_eq!(GapPolicy::from_profile(&profile(6e6, 0.0), 2, "t").max_fill, 0);
        // Writes in flight come from the caller (the transport), never from a cached write_qd.
        let mut q8 = profile(6e6, 0.001);
        q8.write_qd = Some(8);
        assert_eq!(GapPolicy::from_profile(&q8, 2, "t").qd, 2);
    }

    #[test]
    fn gap_never_fills_a_used_cluster() {
        // Clusters 2..=17; 5 and 9 are used by existing files.
        let mut bm = vec![0u8; 2];
        for c in [5u64, 9] {
            bm[((c - 2) / 8) as usize] |= 1 << ((c - 2) % 8);
        }
        let generous = GapPolicy { max_fill: u64::MAX, ..GapPolicy::of(GapMode::Fill, 8, "t") };
        assert!(fill_gap(1 << 20, 6..9, &bm, &generous), "free clusters between our runs");
        assert!(fill_gap(1 << 20, 10..18, &bm, &generous));
        for r in [5..6, 4..6, 2..10, 9..10, 8..12] {
            assert!(!fill_gap(4096, r.clone(), &bm, &generous), "used cluster in {r:?} would be overwritten");
        }
        // Past the bitmap counts as not free.
        assert!(!fill_gap(4096, 17..19, &bm, &generous));
        // Own slack only (no foreign clusters): just the size limit.
        assert!(fill_gap(1 << 20, 7..7, &bm, &generous));
        let fallback = GapPolicy::fallback("t");
        assert!(fill_gap(SLACK_FILL, 7..7, &bm, &fallback));
        assert!(!fill_gap(SLACK_FILL + 1, 7..7, &bm, &fallback));
        assert!(!fill_gap(4096, 6..7, &bm, &fallback), "the fallback fills only its own slack");
        let tight = GapPolicy { max_fill: 8192, ..generous.clone() };
        assert!(fill_gap(8192, 6..8, &bm, &tight));
        assert!(!fill_gap(8193, 6..8, &bm, &tight));
        // The measured candidates obey the same rule.
        let fill = GapPolicy::of(GapMode::Fill, 1, "t");
        assert!(fill_gap(FILL_CAP, 6..9, &bm, &fill) && !fill_gap(FILL_CAP + 1, 6..9, &bm, &fill));
        assert!(!fill_gap(4096, 4..6, &bm, &fill));
        for m in [GapMode::Skip, GapMode::Pad(PAD_UNIT)] {
            assert!(!fill_gap(4096, 6..7, &bm, &GapPolicy::of(m, 1, "t")), "{m:?} fills nothing");
        }
    }

    /// Small files in 4 KB clusters at 4096 (clusters 2…): file A 5000 bytes in clusters 2-3,
    /// a used cluster 4, file B 100 bytes in cluster 5, free cluster 6, file C 9000 bytes in
    /// 7-9 (3 clusters, the last one partly slack).
    fn layout() -> (Vec<ProbeSeg>, Vec<u8>, u64, u64) {
        let (heap, cs) = (4096u64, 4096u64);
        let at = |c: u64| heap + (c - 2) * cs;
        let segs = vec![
            ProbeSeg { dev: at(2), len: 2 * cs, payload: 5000, file_end: true },
            ProbeSeg { dev: at(5), len: cs, payload: 100, file_end: true },
            ProbeSeg { dev: at(7), len: 3 * cs, payload: 9000, file_end: true },
        ];
        let mut bm = vec![0u8; 2];
        bm[0] |= 1 << (4 - 2);
        (segs, bm, heap, cs)
    }

    #[test]
    fn planned_writes_follow_the_policy_and_skip_used_clusters() {
        let (segs, bm, heap, cs) = layout();
        let at = |c: u64| heap + (c - 2) * cs;
        let skip = planned_writes(&segs, &GapPolicy::of(GapMode::Skip, 1, "t"), &bm, heap, cs);
        assert_eq!(skip, vec![(at(2), 8192), (at(5), 4096), (at(7), 12288)]);
        // Fill: cluster 4 is used, so the gap 3→5 stays a skip; cluster 6 is free and filled.
        let fill = planned_writes(&segs, &GapPolicy::of(GapMode::Fill, 1, "t"), &bm, heap, cs);
        assert_eq!(fill, vec![(at(2), 8192), (at(5), 5 * 4096)]);
        // Pad to 16 KB: each file's write grows to its run's end at most.
        let pad = planned_writes(&segs, &GapPolicy::of(GapMode::Pad(16384), 1, "t"), &bm, heap, cs);
        assert_eq!(pad, vec![(at(2), 8192), (at(5), 4096), (at(7), 12288)]);
        // Every written byte is in a cluster the bitmap calls free.
        for w in [&skip, &fill, &pad] {
            for &(o, l) in w.iter() {
                assert!(all_free(&bm, (o - heap) / cs + 2..(o + l - heap).div_ceil(cs) + 2), "{o} {l}");
            }
        }
    }

    #[test]
    fn pad_end_stays_in_the_run() {
        let p = GapPolicy::of(GapMode::Pad(16384), 1, "t");
        assert_eq!(p.pad_end(8000, 32768), 16384);
        assert_eq!(p.pad_end(16384, 32768), 16384);
        assert_eq!(p.pad_end(20000, 24576), 24576);
        assert_eq!(GapPolicy::of(GapMode::Skip, 1, "t").pad_end(8000, 32768), 8192);
    }

    #[test]
    fn probe_windows_are_whole_device_aligned_aus_inside_the_stream() {
        let cs = 32u64 << 10;
        // One 8 KB file per cluster from 1 MB + 32 KB on, 100 MB of stream; partition at 1 MB.
        let segs: Vec<ProbeSeg> = (0..3200u64)
            .map(|i| ProbeSeg { dev: (1 << 20) + cs + i * cs, len: cs, payload: 8192, file_end: true })
            .collect();
        let w = probe_windows(&segs, 1 << 20);
        assert_eq!(w.len() as u64, PROBE_WINDOWS);
        let span = PROBE_AUS * PROBE_AU;
        for (k, r) in w.iter().enumerate() {
            let a = segs[r.start].dev;
            assert_eq!((a + (1 << 20)) % PROBE_AU, 0, "window {k} starts on a device AU");
            assert_eq!(r.len() as u64, span / cs, "window {k} holds whole segments only");
            assert!(segs[r.end - 1].dev + cs <= a + span);
        }
        assert!(w.windows(2).all(|p| p[0].end == p[1].start), "consecutive");
        // A short stream has fewer windows.
        assert!((probe_windows(&segs[..600], 1 << 20).len() as u64) < PROBE_MIN_WINDOWS);
        assert!(probe_windows(&[], 0).is_empty());
    }

    #[test]
    fn forced_policies_parse() {
        assert_eq!(forced_policy("skip", 2).unwrap().mode, Some(GapMode::Skip));
        assert_eq!(forced_policy("skip", 2).unwrap().qd, 2);
        assert_eq!(forced_policy("fill:1", 2).unwrap().qd, 1);
        assert_eq!(forced_policy("pad", 2).unwrap().pad, PAD_UNIT);
        assert_eq!(forced_policy("pad8192:8", 2).unwrap().pad, 8192);
        for bad in ["pad5", "x", "skip:0", "skip:x", "pad0"] {
            assert!(forced_policy(bad, 2).is_none(), "{bad}");
        }
    }

    #[test]
    fn volume_info_json() {
        let i = VolumeInfo { label: "A\"B".into(), cluster: 4096, size: 10, free: 5, files: 1, dirs: 2, dirty: false };
        assert_eq!(
            i.to_json(),
            r#"{"label":"A\"B","cluster":4096,"size":10,"free":5,"files":1,"dirs":2,"dirty":false}"#
        );
    }
}
