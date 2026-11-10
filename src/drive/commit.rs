//! Commits: the difference between the inode tree and the volume, as crash-ordered writes.
//!
//! A commit is planned under the core lock (memory only, plus FAT and committed-data reads),
//! executed without it, and finalized under it again. Execution, with a sync after each step:
//!
//! | step     | writes                                                                                   |
//! |----------|------------------------------------------------------------------------------------------|
//! | `data`   | file data, new directories, directory extensions (zeros): FREE clusters only, plus in-place writes past a file's committed ValidDataLength (not part of its committed content) |
//! | journal  | the commit's journal into `/.flashcp-drive-journal` (header last in the same sync; a torn journal fails its hash and counts as never written) |
//! | `dirty`  | VolumeDirty                                                                              |
//! | `alloc`  | bitmap bits of new clusters; FAT entries no committed chain uses                          |
//! | `link`   | entry sets (new, relocated, edited in one sector, removed), FAT entries of committed chains, directory lengths |
//! | `free`   | bitmap bits of clusters nothing references any more                                       |
//! | `clean`  | VolumeDirty cleared, PercentInUse                                                         |
//! | idle     | the journal header marked idle                                                            |
//!
//! Every write from `dirty` to `clean` is in the journal with the bytes before and after it;
//! the next mount replays an active journal (see [`super::djournal`]), so a crash leaves the
//! volume either exactly as before the commit (the journal never became active) or, after
//! recovery, exactly as after it. Committed clusters are never overwritten: a changed cluster
//! of a committed file gets a new one (copy-on-write), freed clusters are reusable only by the
//! next commit, and nothing a reader may still need is freed before the commit is durable.

use super::core::{Core, DirData, Disk, Ino, Inode, Layout, ROOT};
use super::set::{self, SetFields};
use super::stage::Piece;
use crate::volume::EOC;
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

pub(crate) const PHASES: [&str; 5] = ["dirty", "alloc", "link", "free", "clean"];
pub(crate) const P_DIRTY: u8 = 0;
pub(crate) const P_ALLOC: u8 = 1;
pub(crate) const P_LINK: u8 = 2;
pub(crate) const P_FREE: u8 = 3;
pub(crate) const P_CLEAN: u8 = 4;

/// Files up to this size are rewritten whole into fresh contiguous clusters when changed.
const REWRITE_WHOLE: u64 = 4 << 20;

#[derive(Clone)]
pub(crate) enum Src {
    Zero,
    /// Committed bytes at this device offset (a copy-on-write source).
    Base(u64),
    Piece(Piece, u64),
    Mem(Arc<Vec<u8>>, usize),
}

#[derive(Clone)]
pub(crate) struct Seg {
    pub(crate) dev: u64,
    pub(crate) len: u64,
    pub(crate) src: Src,
}

/// One journaled metadata write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JW {
    pub(crate) phase: u8,
    pub(crate) off: u64,
    pub(crate) pre: Vec<u8>,
    pub(crate) post: Vec<u8>,
}

/// What finalizing a commit does to one inode.
pub(crate) struct InoFin {
    pub(crate) disk: Option<Disk>,
    /// The committed layout after the commit (None: unchanged).
    pub(crate) lay: Option<Layout>,
    /// The set as written (to tell whether the inode changed again meanwhile).
    pub(crate) fields: Option<SetFields>,
    /// Pieces up to this seq are on the volume now.
    pub(crate) drop_seq: Option<u64>,
    /// Bytes [0, keep) of the new layout are content.
    pub(crate) keep: Option<u64>,
}

pub(crate) struct Plan {
    pub(crate) segs: Vec<Seg>,
    pub(crate) writes: Vec<JW>,
    pub(crate) fins: Vec<(Ino, InoFin)>,
    pub(crate) dirs: Vec<(Ino, DirData)>,
    pub(crate) alloc_runs: Vec<(u64, u64)>,
    /// The bitmap before this commit (which gaps between data writes may be zero-filled).
    pub(crate) pre_bm: Arc<Vec<u8>>,
    /// Inodes planned.
    pub(crate) inodes: usize,
    /// Everything dirty at planning time is in it (not a split part).
    pub(crate) full: bool,
    /// The change counter at planning time.
    pub(crate) seq: u64,
    pub(crate) free_runs: Vec<(u64, u64)>,
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) sector0: Vec<u8>,
}

/// A directory's working copy during planning.
struct DW {
    data: Vec<u8>,
    end: usize,
    clusters: Vec<u32>,
    nfc: bool,
    /// Created by this commit (its content goes in `data`, not in journaled patches).
    new: bool,
    /// Bytes of the committed copy (patches past it have zeros as their `pre`).
    old_len: usize,
    old_clusters: usize,
    touched: Vec<(usize, usize)>,
    /// Reusable slot runs (offset, bytes) of entries nobody uses, before `end`.
    holes: Vec<(usize, usize)>,
}

struct Planner<'a> {
    c: &'a mut Core,
    free: Vec<(u64, u64)>,
    dws: HashMap<Ino, DW>,
    /// FAT entry → (value, written in `link` because a committed chain uses it).
    fat: BTreeMap<u32, (u32, bool)>,
    alloc_runs: Vec<(u64, u64)>,
    free_runs: Vec<(u64, u64)>,
    segs: Vec<Seg>,
    fins: HashMap<Ino, InoFin>,
    /// Directories whose clusters this commit frees (no patches into them).
    dead_dirs: HashSet<Ino>,
    files: u64,
    bytes: u64,
    /// Free extents reserved for everything this commit allocates without a preferred
    /// position (one extent that holds the whole batch when there is one, else the fewest
    /// large ones), carved in order: the batch's data is one sequential stream instead of
    /// one hole per file.
    arena: VecDeque<(u64, u64)>,
}

fn holes_of(data: &[u8], end: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < end {
        let t = data[i];
        if t & 0x80 == 0 && t != 0 {
            match out.last_mut() {
                Some(l) if l.0 + l.1 == i => l.1 += 32,
                _ => out.push((i, 32)),
            }
        }
        i += 32;
    }
    out
}

impl Planner<'_> {
    fn cs(&self) -> u64 {
        self.c.vol.cs
    }

    /// Allocates `n` clusters, preferring to continue right after `after`.
    fn alloc(&mut self, n: u64, after: Option<u32>) -> Result<Vec<(u64, u64)>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        if let Some(a) = after {
            let want = a as u64 + 1;
            if let Some(k) = self.free.iter().position(|&(s, l)| s == want && l >= 1) {
                let (s, l) = self.free[k];
                let take = l.min(n);
                if take < l {
                    self.free[k] = (s + take, l - take);
                } else {
                    self.free.remove(k);
                }
                self.alloc_runs.push((s, take));
                let mut runs = vec![(s, take)];
                if take < n {
                    // records its own runs
                    runs.extend(self.alloc(n - take, None)?);
                }
                return Ok(runs);
            }
        }
        // the commit's arena first: its new data goes out as one stream
        if self.arena.iter().map(|e| e.1).sum::<u64>() >= n {
            let runs = crate::add::carve(&mut self.arena, n);
            self.alloc_runs.extend(runs.iter().copied());
            return Ok(runs);
        }
        // the estimate was short: what is left of the arena is free space again
        if !self.arena.is_empty() {
            self.free.extend(self.arena.drain(..));
            self.free.sort_unstable();
        }
        let chosen = crate::add::choose_extents(&self.free, n)
            .ok_or_else(|| anyhow::anyhow!("no space left on the volume ({n} clusters needed)"))?;
        for &(s, l) in &chosen {
            ensure!(crate::add::take_exact(&mut self.free, s, l), "internal error: allocation outside free space");
        }
        self.alloc_runs.extend(chosen.iter().copied());
        Ok(chosen)
    }

    fn dw(&mut self, d: Ino) -> Result<&mut DW> {
        if !self.dws.contains_key(&d) {
            let n = &self.c.inodes[&d];
            let w = match &n.dd {
                Some(dd) => DW {
                    data: dd.data.clone(),
                    end: dd.end,
                    clusters: dd.clusters.clone(),
                    nfc: dd.nfc,
                    new: false,
                    old_len: dd.data.len(),
                    old_clusters: dd.clusters.len(),
                    touched: Vec::new(),
                    holes: holes_of(&dd.data, dd.end),
                },
                None => {
                    ensure!(n.disk.is_none(), "internal error: committed directory {} not loaded", self.c.path(d));
                    DW {
                        data: Vec::new(),
                        end: 0,
                        clusters: Vec::new(),
                        nfc: true,
                        new: true,
                        old_len: 0,
                        old_clusters: 0,
                        touched: Vec::new(),
                        holes: Vec::new(),
                    }
                }
            };
            self.dws.insert(d, w);
        }
        Ok(self.dws.get_mut(&d).unwrap())
    }

    /// Marks the entries of a set unused (all of them, InUse cleared).
    fn clear_set(&mut self, d: Ino, off: usize, len: usize) -> Result<()> {
        if self.dead_dirs.contains(&d) {
            return Ok(());
        }
        let w = self.dw(d)?;
        for k in (off..off + len).step_by(32) {
            w.data[k] &= 0x7F;
        }
        w.touched.push((off, off + len));
        Ok(())
    }

    /// Writes `set` into directory `d` (a reused hole, else at the end, extending it). Returns
    /// its offset; queues `d` when its own layout changed.
    fn place(&mut self, d: Ino, set: &[u8], grown: &mut Vec<Ino>) -> Result<usize> {
        let cs = self.cs() as usize;
        let need = set.len();
        let w = self.dw(d)?;
        if let Some(k) = w.holes.iter().position(|h| h.1 >= need) {
            let (o, l) = w.holes[k];
            if l == need {
                w.holes.remove(k);
            } else {
                w.holes[k] = (o + need, l - need);
            }
            w.data[o..o + need].copy_from_slice(set);
            w.touched.push((o, o + need));
            return Ok(o);
        }
        let o = w.end;
        if o + need > w.data.len() {
            let grow_to = (o + need).div_ceil(cs) * cs;
            ensure!(grow_to as u64 <= crate::volume::MAX_DIR_BYTES, "directory would exceed 256 MB");
            let new = w.new;
            w.data.resize(grow_to, 0);
            if !new {
                let add = (grow_to / cs - w.clusters.len()) as u64;
                let last = *w.clusters.last().unwrap();
                let runs = self.alloc(add, Some(last))?;
                let w = self.dws.get_mut(&d).unwrap();
                let cl = crate::add::run_clusters(&runs);
                let contiguous = cl[0] == last + 1 && runs.len() == 1;
                // zeros into the new clusters (data step), entries come in `link`
                for &(s, n) in &runs {
                    self.segs.push(Seg { dev: self.c.vol.cluster_off(s as u32), len: n * cs as u64, src: Src::Zero });
                }
                let was_nfc = w.nfc;
                w.clusters.extend(cl.iter().copied());
                if was_nfc && !contiguous {
                    w.nfc = false;
                    // The committed version ignores the FAT: the whole chain goes in `alloc`.
                    let all = w.clusters.clone();
                    for (k, &c) in all.iter().enumerate() {
                        self.fat.insert(c, (all.get(k + 1).copied().unwrap_or(EOC), false));
                    }
                } else if !was_nfc {
                    let all = w.clusters.clone();
                    let old_n = all.len() - cl.len();
                    // The committed chain's last link changes (link); the new ones (alloc).
                    self.fat.insert(all[old_n - 1], (all[old_n], true));
                    for k in old_n..all.len() {
                        self.fat.insert(all[k], (all.get(k + 1).copied().unwrap_or(EOC), false));
                    }
                }
                if d != ROOT || !was_nfc {
                    grown.push(d);
                }
            }
        }
        let w = self.dws.get_mut(&d).unwrap();
        w.data[o..o + need].copy_from_slice(set);
        w.end = o + need;
        w.touched.push((o, o + need));
        Ok(o)
    }

    /// Plans a changed file's new data: layout, data segments, freed clusters. Returns
    /// (layout, keep).
    #[allow(clippy::needless_range_loop)]
    fn plan_file(&mut self, i: Ino) -> Result<(Layout, u64)> {
        let cs = self.cs();
        let runs = self.c.runs(i).map_err(|e| anyhow::anyhow!("reading {}: errno {e}", self.c.path(i)))?;
        let n = &self.c.inodes[&i];
        let (size, keep, old) = (n.size, n.keep.min(n.size), n.lay.clone());
        let ov_end = n.ov.end().min(size);
        let old_cl = crate::add::run_clusters(&runs.iter().map(|&(s, n)| (s as u64, n as u64)).collect::<Vec<_>>());
        // New ValidDataLength: through the last byte that holds data (staged or kept).
        let valid = ov_end.max(keep).min(size);
        let n_new = size.div_ceil(cs) as usize;
        let n_old = old_cl.len();
        let ov = n.ov.clone();
        let changed = |a: u64, b: u64| -> bool {
            let b = b.min(valid);
            a < b && (b > keep || ov.range(a, b).next().is_some())
        };
        let mut protected_changed = 0usize;
        for k in 0..n_old.min(n_new) {
            let (a, b) = (k as u64 * cs, (k as u64 + 1) * cs);
            if a < old.valid && changed(a, b) {
                protected_changed += 1;
            }
        }
        let whole = n_old == 0 || size <= REWRITE_WHOLE || protected_changed * 2 > n_new.max(1);
        // Physical cluster per logical one; true where this commit writes it.
        let mut phys: Vec<u32> = Vec::with_capacity(n_new);
        let mut write: Vec<bool> = Vec::with_capacity(n_new);
        let mut freed: Vec<u32> = Vec::new();
        if whole {
            let r = self.alloc(n_new as u64, None)?;
            phys = crate::add::run_clusters(&r);
            write = (0..n_new).map(|k| (k as u64 * cs) < valid).collect();
            freed.extend(old_cl.iter().copied());
        } else {
            let mut cow = Vec::new();
            for k in 0..n_new {
                let (a, b) = (k as u64 * cs, (k as u64 + 1) * cs);
                if k < n_old {
                    let ch = changed(a, b);
                    if ch && a < old.valid {
                        cow.push(k);
                        phys.push(0);
                        freed.push(old_cl[k]);
                    } else {
                        phys.push(old_cl[k]);
                    }
                    write.push(ch);
                } else {
                    phys.push(0);
                    write.push(a < valid);
                }
            }
            freed.extend(old_cl.iter().skip(n_new).copied());
            if !cow.is_empty() {
                let r = self.alloc(cow.len() as u64, None)?;
                for (k, c) in cow.into_iter().zip(crate::add::run_clusters(&r)) {
                    phys[k] = c;
                }
            }
            if n_new > n_old {
                let r = self.alloc((n_new - n_old) as u64, old_cl.last().copied())?;
                for (k, c) in (n_old..n_new).zip(crate::add::run_clusters(&r)) {
                    phys[k] = c;
                }
            }
        }
        // FAT chain unless contiguous.
        let contiguous = phys.windows(2).all(|w| w[1] == w[0] + 1);
        let nfc = contiguous;
        if !nfc {
            let committed: HashSet<u32> = if old.nfc { HashSet::new() } else { old_cl.iter().copied().collect() };
            for (k, &c) in phys.iter().enumerate() {
                let next = phys.get(k + 1).copied().unwrap_or(EOC);
                let cur = self.c.vol.fat(c).context("read FAT")?;
                if cur != next || !committed.contains(&c) {
                    self.fat.insert(c, (next, committed.contains(&c)));
                }
            }
        }
        // Data segments: per run of written, physically consecutive clusters.
        let mut k = 0;
        while k < n_new {
            if !write[k] {
                k += 1;
                continue;
            }
            let mut j = k + 1;
            while j < n_new && write[j] && phys[j] == phys[j - 1] + 1 {
                j += 1;
            }
            let (a, b) = (k as u64 * cs, (j as u64 * cs).min(valid.div_ceil(512) * 512).min(j as u64 * cs));
            let dev0 = self.c.vol.cluster_off(phys[k]);
            self.compose(i, &runs, a, b, keep, valid, dev0)?;
            k = j;
        }
        let r: Vec<(u64, u64)> = freed.iter().map(|&c| (c as u64, 1)).collect();
        self.free_runs.extend(r);
        self.files += 1;
        self.bytes += valid;
        let first = phys.first().copied().unwrap_or(0);
        let mut lruns: Vec<(u32, u32)> = Vec::new();
        for &c in &phys {
            match lruns.last_mut() {
                Some(l) if l.0 + l.1 == c => l.1 += 1,
                _ => lruns.push((c, 1)),
            }
        }
        Ok((Layout { first, nfc: nfc && first != 0, len: size, valid, runs: Some(lruns) }, valid))
    }

    /// Segments writing logical bytes [a, b) of file `i` to device offset `dev0` on: staged
    /// pieces, committed bytes below `keep`, zeros.
    #[allow(clippy::too_many_arguments)]
    fn compose(&mut self, i: Ino, runs: &[(u32, u32)], a: u64, b: u64, keep: u64, valid: u64, dev0: u64) -> Result<()> {
        let cs = self.cs();
        let n = &self.c.inodes[&i];
        let mut at = a;
        let mut out: Vec<Seg> = Vec::new();
        let gap = |out: &mut Vec<Seg>, from: u64, to: u64| {
            let mut x = from;
            while x < to {
                if x < keep.min(valid) {
                    // committed bytes: map logical x to its old device offset, run by run
                    let mut logical = 0u64;
                    let mut done = false;
                    for &(s, cnt) in runs {
                        let rl = cnt as u64 * cs;
                        if x >= logical && x < logical + rl {
                            let take = (logical + rl).min(to).min(keep.min(valid)) - x;
                            let src = self.c.vol.cluster_off(s) + (x - logical);
                            out.push(Seg { dev: dev0 + (x - a), len: take, src: Src::Base(src) });
                            x += take;
                            done = true;
                            break;
                        }
                        logical += rl;
                    }
                    if !done {
                        // past the committed runs (cannot happen: keep ≤ committed length)
                        out.push(Seg { dev: dev0 + (x - a), len: to - x, src: Src::Zero });
                        x = to;
                    }
                } else {
                    out.push(Seg { dev: dev0 + (x - a), len: to - x, src: Src::Zero });
                    x = to;
                }
            }
        };
        let pieces: Vec<(u64, Piece)> = n.ov.range(a, b).map(|(o, p)| (o, p.clone())).collect();
        for (po, p) in pieces {
            let (pa, pb) = (po.max(a), (po + p.len).min(b));
            if pa > at {
                gap(&mut out, at, pa);
            }
            out.push(Seg { dev: dev0 + (pa - a), len: pb - pa, src: Src::Piece(p, pa - po) });
            at = pb;
        }
        if at < b {
            gap(&mut out, at, b);
        }
        self.segs.extend(out);
        Ok(())
    }
}

/// At least `n` of the dirty inodes `all`, closed under what must commit together:
/// - unlinked ones deepest first (a directory's entries go before or with it), then linked
///   ones by depth (a new directory goes with or before its entries);
/// - with an unlinked directory, every dirty inode whose committed set lies in it (its
///   unlinked entries, and whatever moved out of it: their only committed set is in there);
/// - with an unlinked inode, the inode that took its name (a replaced target and its
///   replacement change together: the name never disappears);
/// - with an inode that took over a name, the unlinked inode that had it.
fn subset(c: &Core, mut all: Vec<Ino>, n: usize) -> Vec<Ino> {
    let linked = |i: &Ino| c.inodes.get(i).is_some_and(|x| x.linked);
    all.sort_by_cached_key(|&i| {
        let d = depth(c, i) as i64;
        (linked(&i), if linked(&i) { d } else { -d }, i)
    });
    let dirty: HashSet<Ino> = all.iter().copied().collect();
    let mut in_dir: HashMap<Ino, Vec<Ino>> = HashMap::new();
    for &i in &all {
        if let Some(d) = c.inodes.get(&i).and_then(|x| x.disk) {
            in_dir.entry(d.dir).or_default().push(i);
        }
    }
    // name → (unlinked holder, new holder) pairs, by (parent, up-cased name)
    let mut by_name: HashMap<(Ino, Vec<u16>), Vec<Ino>> = HashMap::new();
    for &i in &all {
        if let Some(x) = c.inodes.get(&i) {
            by_name.entry((x.parent, super::set::key(&x.name, &c.vol.upc))).or_default().push(i);
            if let Some(d) = x.disk {
                // its committed name too (a rename away from it)
                if let Some(dd) = c.inodes.get(&d.dir).and_then(|p| p.dd.as_ref())
                    && let Ok(f) = super::set::decode(&dd.data[d.off..d.off + d.len])
                {
                    by_name.entry((d.dir, super::set::key(&f.name, &c.vol.upc))).or_default().push(i);
                }
            }
        }
    }
    let mut take: HashSet<Ino> = HashSet::new();
    let mut out: Vec<Ino> = Vec::new();
    let mut stack: Vec<Ino> = Vec::new();
    for &i in &all {
        if out.len() >= n {
            break;
        }
        stack.push(i);
        while let Some(j) = stack.pop() {
            if !dirty.contains(&j) || !take.insert(j) {
                continue;
            }
            out.push(j);
            let Some(x) = c.inodes.get(&j) else { continue };
            if !x.linked && x.is_dir {
                stack.extend(in_dir.get(&j).into_iter().flatten().copied());
            }
            if let Some(v) = by_name.get(&(x.parent, super::set::key(&x.name, &c.vol.upc))) {
                stack.extend(v.iter().copied());
            }
            if let Some(d) = x.disk
                && let Some(dd) = c.inodes.get(&d.dir).and_then(|p| p.dd.as_ref())
                && let Ok(f) = super::set::decode(&dd.data[d.off..d.off + d.len])
                && let Some(v) = by_name.get(&(d.dir, super::set::key(&f.name, &c.vol.upc)))
            {
                stack.extend(v.iter().copied());
            }
            // a new directory needs its parent committed with or before it
            if x.linked && x.disk.is_none() {
                stack.push(x.parent);
            }
        }
    }
    out
}

/// Depth of an inode in the desired tree.
fn depth(c: &Core, mut i: Ino) -> usize {
    let mut d = 0;
    while i != ROOT && d < 4096 {
        i = c.inodes.get(&i).map_or(ROOT, |n| n.parent);
        d += 1;
    }
    d
}

/// Reads the committed content of an unlinked but still open file into its overlay, so its
/// clusters can be freed.
fn materialize(c: &mut Core, i: Ino) -> Result<()> {
    let n = &c.inodes[&i];
    let keep = n.keep.min(n.size);
    if keep == 0 {
        return Ok(());
    }
    let mut at = 0u64;
    while at < keep {
        let len = (keep - at).min(4 << 20);
        let bytes = c.read(i, at, len as u32).map_err(|e| anyhow::anyhow!("read errno {e}"))?;
        // Only the committed bytes not shadowed by staged ones need copying, but writing the
        // read-back (which already merges them) is simplest.
        c.seq += 1;
        let seq = c.seq;
        let n = c.inodes.get_mut(&i).unwrap();
        c.stager.write(&mut n.ov, at, &bytes, seq)?;
        at += len;
    }
    let n = c.inodes.get_mut(&i).unwrap();
    n.keep = 0;
    Ok(())
}

/// Plans a commit of everything dirty. None when there is nothing to do.
/// With `limit`, at most that many inodes go into this commit (the rest stays dirty for the
/// next one): unlinked ones first (a replaced target must go with its replacement), then by
/// depth, so a new directory is always committed with or before its entries.
pub(crate) fn plan(c: &mut Core, limit: Option<usize>) -> Result<Option<Plan>> {
    if c.dirty.is_empty() {
        return Ok(None);
    }
    let mut batch: Vec<Ino> = c.dirty.iter().copied().collect();
    let full = limit.is_none_or(|n| n >= batch.len());
    if let Some(n) = limit.filter(|&n| n < batch.len()) {
        batch = subset(c, batch, n);
    }
    // Unlinked inodes that never reached the volume: nothing to write.
    let mut work: Vec<Ino> = Vec::new();
    for &i in &batch {
        let Some(n) = c.inodes.get(&i) else {
            c.dirty.remove(&i);
            continue;
        };
        if !n.linked && n.disk.is_none() {
            c.dirty.remove(&i);
            c.gc(i);
            continue;
        }
        work.push(i);
    }
    if work.is_empty() {
        return Ok(None);
    }
    // Every directory a set is written into or removed from must be loaded.
    for &i in &work {
        let n = &c.inodes[&i];
        let (p, old, linked, own) = (n.parent, n.disk.map(|d| d.dir), n.linked, n.is_dir && n.disk.is_some());
        if linked {
            c.load(p).map_err(|e| anyhow::anyhow!("load {}: errno {e}", c.path(p)))?;
        }
        if let Some(o) = old {
            c.load(o).map_err(|e| anyhow::anyhow!("load {}: errno {e}", c.path(o)))?;
        }
        // a committed directory whose own set changes (renamed, moved, retimed) or that goes
        if own {
            c.load(i).map_err(|e| anyhow::anyhow!("load {}: errno {e}", c.path(i)))?;
        }
    }
    let free = crate::add::free_extents(&c.bm, c.vol.cc);
    let mut p = Planner {
        c,
        free,
        dws: HashMap::new(),
        fat: BTreeMap::new(),
        alloc_runs: Vec::new(),
        free_runs: Vec::new(),
        segs: Vec::new(),
        fins: HashMap::new(),
        dead_dirs: HashSet::new(),
        files: 0,
        bytes: 0,
        arena: VecDeque::new(),
    };
    let seq = p.c.seq;
    // ── removals (unlinked) and moves away (the old set goes) ──
    for &i in &work {
        let n = &p.c.inodes[&i];
        if !n.linked && n.is_dir {
            p.dead_dirs.insert(i);
        }
    }
    for &i in &work {
        let n = &p.c.inodes[&i];
        let Some(d) = n.disk else { continue };
        if n.linked {
            continue;
        }
        let is_dir = n.is_dir;
        if !is_dir && n.opens > 0 {
            materialize(p.c, i)?;
        }
        p.clear_set(d.dir, d.off, d.len)?;
        let runs: Vec<(u64, u64)> = if is_dir {
            match p.c.inodes[&i].dd.as_ref() {
                Some(dd) => dd.clusters.iter().map(|&c| (c as u64, 1)).collect(),
                None => p.c.runs(i).unwrap_or_default().iter().map(|&(s, n)| (s as u64, n as u64)).collect(),
            }
        } else {
            p.c.runs(i).map_err(|e| anyhow::anyhow!("errno {e}"))?.iter().map(|&(s, n)| (s as u64, n as u64)).collect()
        };
        p.free_runs.extend(runs);
        p.fins.insert(
            i,
            InoFin { disk: None, lay: Some(Layout::default()), fields: None, drop_seq: None, keep: Some(0) },
        );
    }
    // ── file data ──
    let mut lays: HashMap<Ino, (Layout, u64)> = HashMap::new();
    let mut files: Vec<Ino> = work
        .iter()
        .copied()
        .filter(|i| p.c.inodes[i].linked && !p.c.inodes[i].is_dir && p.c.inodes[i].data_dirty)
        .collect();
    files.sort_by_cached_key(|&i| p.c.path(i));
    // reserve the arena: an upper bound of what the batch allocates (changed files whole, new
    // directories with their entries); only clusters free before the commit, as always
    let cs = p.cs();
    let mut total: u64 = files.iter().map(|i| p.c.inodes[i].size.div_ceil(cs)).sum();
    let mut new_dirs: HashMap<Ino, u64> = HashMap::new();
    for &i in &work {
        let n = &p.c.inodes[&i];
        if n.linked && n.is_dir && n.disk.is_none() {
            new_dirs.entry(i).or_insert(0);
        }
        if n.linked && p.c.inodes.get(&n.parent).is_some_and(|d| d.disk.is_none() && n.parent != ROOT) {
            *new_dirs.entry(n.parent).or_insert(0) += set::set_bytes(n.name.len()) as u64;
        }
    }
    total += new_dirs.values().map(|b| b.div_ceil(cs).max(1)).sum::<u64>();
    if total > 0
        && let Some(ch) = crate::add::choose_extents(&p.free, total)
    {
        for &(s, l) in &ch {
            ensure!(crate::add::take_exact(&mut p.free, s, l), "internal error: arena outside free space");
        }
        p.arena = ch.into_iter().collect();
    }
    for i in files {
        let l = p.plan_file(i)?;
        lays.insert(i, l);
    }
    // ── entry sets, deepest first (a directory's content is complete before its own set) ──
    let mut queue: BTreeMap<(std::cmp::Reverse<usize>, Ino), ()> = BTreeMap::new();
    for &i in &work {
        if p.c.inodes[&i].linked {
            queue.insert((std::cmp::Reverse(depth(p.c, i)), i), ());
        }
    }
    let mut done: HashSet<Ino> = HashSet::new();
    while let Some(((_, i), ())) = queue.pop_first() {
        if !done.insert(i) || i == ROOT {
            continue;
        }
        let n = &p.c.inodes[&i];
        let (parent, disk, is_dir) = (n.parent, n.disk, n.is_dir);
        // The layout the set must point at.
        let (lay, keep) = if is_dir {
            let cs = p.cs() as usize;
            let w = p.dw(i)?;
            if w.new {
                // allocate and fill the new directory now that its entries are known
                let len = w.data.len().max(1).div_ceil(cs) * cs;
                w.data.resize(len, 0);
                let data = Arc::new(w.data.clone());
                let r = p.alloc((len / cs) as u64, None)?;
                let cl = crate::add::run_clusters(&r);
                let nfc = r.len() == 1;
                if !nfc {
                    for (k, &c) in cl.iter().enumerate() {
                        p.fat.insert(c, (cl.get(k + 1).copied().unwrap_or(EOC), false));
                    }
                }
                let mut off = 0usize;
                for &(s, n) in &r {
                    let l = n as usize * cs;
                    p.segs.push(Seg {
                        dev: p.c.vol.cluster_off(s as u32),
                        len: l as u64,
                        src: Src::Mem(data.clone(), off),
                    });
                    off += l;
                }
                let w = p.dws.get_mut(&i).unwrap();
                w.clusters = cl.clone();
                w.nfc = nfc;
                let runs = r.iter().map(|&(s, n)| (s as u32, n as u32)).collect();
                (Layout { first: cl[0], nfc, len: len as u64, valid: len as u64, runs: Some(runs) }, 0)
            } else {
                let len = w.data.len() as u64;
                let nfc = w.nfc;
                let first = w.clusters[0];
                let mut runs: Vec<(u32, u32)> = Vec::new();
                for &c in &w.clusters {
                    match runs.last_mut() {
                        Some(l) if l.0 + l.1 == c => l.1 += 1,
                        _ => runs.push((c, 1)),
                    }
                }
                (Layout { first, nfc, len, valid: len, runs: Some(runs) }, 0)
            }
        } else if let Some(l) = lays.get(&i) {
            l.clone()
        } else {
            let n = &p.c.inodes[&i];
            (n.lay.clone(), n.keep)
        };
        let f = p.c.fields(i, lay.first, lay.nfc, lay.len, lay.valid);
        let old: Option<Vec<u8>> = disk.map(|d| {
            let dd = p.c.inodes[&d.dir].dd.as_ref().unwrap();
            dd.data[d.off..d.off + d.len].to_vec()
        });
        let new = set::encode(&f, &p.c.vol.upc, old.as_deref());
        let mut grown = Vec::new();
        let new_disk = match (disk, old) {
            (Some(d), Some(old)) if d.dir == parent && old.len() == new.len() => {
                let lo = (0..new.len()).find(|&k| new[k] != old[k]);
                match lo {
                    None => Some(d),
                    Some(lo) => {
                        let hi = (0..new.len()).rev().find(|&k| new[k] != old[k]).unwrap() + 1;
                        if (d.off + lo) / 512 == (d.off + hi - 1) / 512 {
                            let w = p.dw(parent)?;
                            w.data[d.off..d.off + d.len].copy_from_slice(&new);
                            w.touched.push((d.off + lo, d.off + hi));
                            Some(d)
                        } else {
                            let o = p.place(parent, &new, &mut grown)?;
                            p.clear_set(d.dir, d.off, d.len)?;
                            Some(Disk { dir: parent, off: o, len: new.len() })
                        }
                    }
                }
            }
            (Some(d), _) => {
                let o = p.place(parent, &new, &mut grown)?;
                p.clear_set(d.dir, d.off, d.len)?;
                Some(Disk { dir: parent, off: o, len: new.len() })
            }
            (None, _) => {
                let o = p.place(parent, &new, &mut grown)?;
                Some(Disk { dir: parent, off: o, len: new.len() })
            }
        };
        let data_dirty = p.c.inodes[&i].data_dirty;
        p.fins.insert(
            i,
            InoFin {
                disk: new_disk,
                lay: if is_dir || data_dirty { Some(lay) } else { None },
                fields: Some(f),
                drop_seq: if data_dirty { Some(seq) } else { None },
                keep: if !is_dir && data_dirty { Some(keep) } else { None },
            },
        );
        for g in grown {
            if g != ROOT {
                done.remove(&g);
                queue.insert((std::cmp::Reverse(depth(p.c, g)), g), ());
            }
        }
        // a new directory's parent must have its set placed too (it is queued: dirty)
    }
    // Directories extended but not otherwise dirty were queued above; the root has no set.
    let n_inodes = work.len();
    finish(p, n_inodes, full, seq).map(Some)
}

/// Turns the planner's state into the journaled writes.
fn finish(mut p: Planner<'_>, n_inodes: usize, full: bool, plan_seq: u64) -> Result<Plan> {
    let mut writes: Vec<JW> = Vec::new();
    // dirty
    let s0 = p.c.vol.sector0.clone();
    let flags = crate::volume::le16(&s0, 106);
    let wf = crate::add::working_flags(flags);
    writes.push(JW { phase: P_DIRTY, off: 106, pre: s0[106..108].to_vec(), post: wf.to_le_bytes().to_vec() });
    // bitmap: alloc, then free
    let mut bm = p.c.bm.clone();
    let bm_patch =
        |bm: &mut Vec<u8>, runs: &[(u64, u64)], set: bool, phase: u8, out: &mut Vec<JW>, v: &crate::volume::Vol| {
            let mut bytes: Vec<usize> = Vec::new();
            for &(s, n) in runs {
                for c in s..s + n {
                    let i = (c - 2) as usize;
                    bytes.push(i / 8);
                }
            }
            bytes.sort_unstable();
            bytes.dedup();
            let mut k = 0;
            while k < bytes.len() {
                let mut j = k + 1;
                while j < bytes.len() && bytes[j] == bytes[j - 1] + 1 {
                    j += 1;
                }
                let (a, b) = (bytes[k], bytes[j - 1] + 1);
                let pre = bm[a..b].to_vec();
                for &(s, n) in runs {
                    for c in s..s + n {
                        let i = (c - 2) as usize;
                        if i / 8 >= a && i / 8 < b {
                            if set {
                                bm[i / 8] |= 1 << (i % 8);
                            } else {
                                bm[i / 8] &= !(1 << (i % 8));
                            }
                        }
                    }
                }
                let post = bm[a..b].to_vec();
                if pre != post {
                    object_writes(v, &v.bitmap_clusters, a, &pre, &post, phase, out);
                }
                k = j;
            }
        };
    let alloc_runs = p.alloc_runs.clone();
    let free_runs = p.free_runs.clone();
    let mut alloc_w = Vec::new();
    let mut free_w = Vec::new();
    bm_patch(&mut bm, &alloc_runs, true, P_ALLOC, &mut alloc_w, &p.c.vol);
    bm_patch(&mut bm, &free_runs, false, P_FREE, &mut free_w, &p.c.vol);
    // FAT
    let mut fat_w: Vec<JW> = Vec::new();
    let fat_off = p.c.vol.fat_off;
    let fat: Vec<(u32, (u32, bool))> = p.fat.iter().map(|(&c, &v)| (c, v)).collect();
    for (c, (val, link)) in fat {
        let cur = p.c.vol.fat(c).context("read FAT")?;
        if cur == val && link {
            continue;
        }
        fat_w.push(JW {
            phase: if link { P_LINK } else { P_ALLOC },
            off: fat_off + c as u64 * 4,
            pre: cur.to_le_bytes().to_vec(),
            post: val.to_le_bytes().to_vec(),
        });
    }
    writes.extend(alloc_w);
    writes.extend(fat_w.iter().filter(|w| w.phase == P_ALLOC).cloned());
    writes.extend(fat_w.iter().filter(|w| w.phase == P_LINK).cloned());
    // directories
    let mut dirs_out: Vec<(Ino, DirData)> = Vec::new();
    let mut dir_ids: Vec<Ino> = p.dws.keys().copied().collect();
    dir_ids.sort();
    for d in dir_ids {
        let w = p.dws.remove(&d).unwrap();
        if p.dead_dirs.contains(&d) {
            continue;
        }
        if !w.new {
            let old = &p.c.inodes[&d].dd.as_ref().unwrap().data;
            let mut ranges = w.touched.clone();
            ranges.sort();
            let mut merged: Vec<(usize, usize)> = Vec::new();
            for (a, b) in ranges {
                let (a, b) = (a / 512 * 512, (b.div_ceil(512) * 512).min(w.data.len()));
                match merged.last_mut() {
                    Some(l) if a <= l.1 => l.1 = l.1.max(b),
                    _ => merged.push((a, b)),
                }
            }
            for (a, b) in merged {
                let pre: Vec<u8> = (a..b).map(|k| if k < w.old_len { old[k] } else { 0 }).collect();
                let post = w.data[a..b].to_vec();
                if pre != post {
                    object_writes(&p.c.vol, &w.clusters, a, &pre, &post, P_LINK, &mut writes);
                }
            }
            let _ = w.old_clusters;
        }
        dirs_out.push((d, DirData { clusters: w.clusters, nfc: w.nfc, data: w.data, end: w.end }));
    }
    writes.extend(free_w);
    // clean + PercentInUse
    let used_after =
        p.c.used + alloc_runs.iter().map(|r| r.1).sum::<u64>() - free_runs.iter().map(|r| r.1).sum::<u64>();
    let pct = (used_after * 100 / p.c.vol.cc.max(1)) as u8;
    let cf = crate::add::clean_flags(wf);
    writes.push(JW { phase: P_CLEAN, off: 106, pre: wf.to_le_bytes().to_vec(), post: cf.to_le_bytes().to_vec() });
    if s0[112] != pct {
        writes.push(JW { phase: P_CLEAN, off: 112, pre: vec![s0[112]], post: vec![pct] });
    }
    writes.sort_by_key(|w| (w.phase, w.off));
    let mut merged: Vec<JW> = Vec::with_capacity(writes.len());
    for w in writes {
        match merged.last_mut() {
            Some(l) if l.phase == w.phase && l.off + l.post.len() as u64 == w.off => {
                l.pre.extend_from_slice(&w.pre);
                l.post.extend_from_slice(&w.post);
            }
            Some(l) if l.phase == w.phase && l.off + l.post.len() as u64 > w.off => {
                bail!("internal error: overlapping metadata writes at {}", w.off)
            }
            _ => merged.push(w),
        }
    }
    let writes = merged;
    let mut segs = std::mem::take(&mut p.segs);
    segs.retain(|s| s.len > 0);
    segs.sort_by_key(|s| s.dev);
    for w in segs.windows(2) {
        if w[0].dev + w[0].len > w[1].dev {
            bail!("internal error: overlapping data segments at {}", w[1].dev);
        }
    }
    let mut s0n = s0.clone();
    s0n[106..108].copy_from_slice(&cf.to_le_bytes());
    s0n[112] = pct;
    Ok(Plan {
        segs,
        writes,
        fins: p.fins.into_iter().collect(),
        dirs: dirs_out,
        alloc_runs,
        pre_bm: Arc::new(p.c.bm.clone()),
        inodes: n_inodes,
        full,
        seq: plan_seq,
        free_runs,
        files: p.files,
        bytes: p.bytes,
        sector0: s0n,
    })
}

/// Journaled writes of bytes [a, a + post.len()) of an object stored in `clusters`.
fn object_writes(
    v: &crate::volume::Vol,
    clusters: &[u32],
    a: usize,
    pre: &[u8],
    post: &[u8],
    phase: u8,
    out: &mut Vec<JW>,
) {
    let cs = v.cs as usize;
    let mut k = 0usize;
    while k < post.len() {
        let at = a + k;
        let n = (cs - at % cs).min(post.len() - k);
        let off = v.cluster_off(clusters[at / cs]) + (at % cs) as u64;
        match out.last_mut() {
            Some(l) if l.phase == phase && l.off + l.post.len() as u64 == off => {
                l.pre.extend_from_slice(&pre[k..k + n]);
                l.post.extend_from_slice(&post[k..k + n]);
            }
            _ => out.push(JW { phase, off, pre: pre[k..k + n].to_vec(), post: post[k..k + n].to_vec() }),
        }
        k += n;
    }
}

/// Applies a finished commit to the in-memory state (under the lock).
pub(crate) fn finalize(c: &mut Core, p: Plan) {
    for (d, dd) in p.dirs {
        if let Some(n) = c.inodes.get_mut(&d) {
            n.dd = Some(dd);
        }
    }
    for &(s, n) in &p.alloc_runs {
        for x in s..s + n {
            let i = (x - 2) as usize;
            c.bm[i / 8] |= 1 << (i % 8);
        }
    }
    for &(s, n) in &p.free_runs {
        for x in s..s + n {
            let i = (x - 2) as usize;
            c.bm[i / 8] &= !(1 << (i % 8));
        }
    }
    c.used = c.used + p.alloc_runs.iter().map(|r| r.1).sum::<u64>() - p.free_runs.iter().map(|r| r.1).sum::<u64>();
    c.vol.sector0 = p.sector0;
    c.vol.fat_cache.clear();
    c.vol.fat_order.clear();
    for (i, f) in p.fins {
        let cs = c.vol.cs;
        let Some(n) = c.inodes.get_mut(&i) else { continue };
        n.disk = f.disk;
        if let Some(l) = f.lay {
            n.lay = l;
        }
        if let Some(seq) = f.drop_seq {
            let r = n.ov.drop_through(seq);
            c.stager.released(r);
        }
        if let Some(k) = f.keep {
            n.keep = k.min(n.cut).min(n.size);
        }
        n.cut = u64::MAX;
        let data_dirty = !n.is_dir && (!n.ov.is_empty() || n.size != n.lay.len || n.keep < n.lay.valid.min(n.size));
        n.data_dirty = data_dirty;
        let need = if data_dirty { n.size.div_ceil(cs) } else { 0 };
        c.reserved = c.reserved - n.need + need;
        n.need = need;
        let meta_same = match (&f.fields, n.disk) {
            (Some(fl), Some(_)) => {
                fl.name == n.name
                    && fl.attrs == n.attrs
                    && fl.mtime == n.mtime
                    && fl.ctime == n.ctime
                    && fl.atime == n.atime
            }
            _ => true,
        };
        let parent_same = n.disk.is_none_or(|d| d.dir == n.parent) || !n.linked;
        let gone = !n.linked && n.disk.is_some();
        if !data_dirty && meta_same && parent_same && !gone && (n.linked || n.disk.is_none()) {
            c.dirty.remove(&i);
        }
        if !n.linked && n.disk.is_none() {
            c.dirty.remove(&i);
            c.gc(i);
        }
    }
    c.commits += 1;
    if p.full {
        c.durable_seq = c.durable_seq.max(p.seq);
    }
    let _ = Inode::clone;
}

/// Marks the start of a commit's execution: truncations from now on cap what the commit's
/// data may still show (see `Inode::cut`).
pub(crate) fn begin(c: &mut Core, p: &Plan) {
    for (i, _) in &p.fins {
        if let Some(n) = c.inodes.get_mut(i) {
            n.cut = u64::MAX;
        }
    }
    let _ = VecDeque::<u8>::new;
}
