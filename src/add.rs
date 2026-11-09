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

use crate::events::{Event, Op, Sink, Ticker};
use crate::exfat::{
    Item, Node, SeqWriter, TargetSpec, entry_set, finish_set, open_target_fd, ordered_reader, release, scan_tree,
    set_bytes, set_direct, validate_name,
};
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
/// Gaps up to this size between written spans are zero-filled rather than skipped.
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
fn free_extents(bitmap: &[u8], cc: u64) -> Vec<(u64, u64)> {
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
fn take_exact(free: &mut Vec<(u64, u64)>, start: u64, n: u64) -> bool {
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
fn choose_extents(free: &[(u64, u64)], total: u64) -> Option<Vec<(u64, u64)>> {
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
fn carve(extents: &mut std::collections::VecDeque<(u64, u64)>, mut n: u64) -> Vec<(u64, u64)> {
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

fn run_clusters(runs: &[(u64, u64)]) -> Vec<u32> {
    runs.iter().flat_map(|&(s, n)| (s..s + n).map(|c| c as u32)).collect()
}

/// FAT entries chaining `clusters` in order, ending with EOC.
fn chain_entries(clusters: &[u32], out: &mut Vec<(u32, u32)>) {
    for (k, &c) in clusters.iter().enumerate() {
        out.push((c, clusters.get(k + 1).copied().unwrap_or(EOC)));
    }
}

// ───────────────────────── patches ─────────────────────────

/// Small writes for one metadata phase; committed ascending and coalesced, then synced.
#[derive(Default)]
struct Patches(Vec<(u64, Vec<u8>)>);

impl Patches {
    fn add(&mut self, off: u64, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.0.push((off, bytes));
        }
    }
    fn fat(&mut self, v: &Vol, entries: &[(u32, u32)]) {
        for &(c, n) in entries {
            self.add(v.fat_off + c as u64 * 4, n.to_le_bytes().to_vec());
        }
    }
    /// Writes `bytes` at byte `off` of an object stored in `clusters`.
    fn object(&mut self, v: &Vol, clusters: &[u32], mut off: usize, mut bytes: &[u8]) {
        let cs = v.cs as usize;
        while !bytes.is_empty() {
            let n = (cs - off % cs).min(bytes.len());
            self.add(v.cluster_off(clusters[off / cs]) + (off % cs) as u64, bytes[..n].to_vec());
            off += n;
            bytes = &bytes[n..];
        }
    }
    fn bytes(&self) -> u64 {
        self.0.iter().map(|p| p.1.len() as u64).sum()
    }
    fn commit(mut self, dev: &File) -> Result<()> {
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
            dev.write_all_at(&buf, off).with_context(|| format!("write metadata at {off}"))?;
            i = j;
        }
        dev.sync_all().context("sync")?;
        Ok(())
    }
}

fn stop_after(phase: &str) -> Result<()> {
    if std::env::var(STOP_ENV).as_deref() == Ok(phase) {
        bail!("stopped after the {phase} phase ({STOP_ENV})");
    }
    Ok(())
}

/// VolumeFlags while modifying: dirty set, ClearToZero cleared (spec 3.1.13.4).
fn working_flags(flags: u16) -> u16 {
    (flags | FLAG_DIRTY) & !FLAG_CLEAR_TO_ZERO
}

/// VolumeFlags when done: dirty cleared, ClearToZero stays cleared.
fn clean_flags(flags: u16) -> u16 {
    flags & !(FLAG_DIRTY | FLAG_CLEAR_TO_ZERO)
}

fn write_flags(v: &mut Vol, flags: u16, pct: Option<u8>) -> Result<()> {
    v.sector0[106..108].copy_from_slice(&flags.to_le_bytes());
    if let Some(p) = pct {
        v.sector0[112] = p;
    }
    v.dev.write_all_at(&v.sector0, 0).context("write boot sector")?;
    v.dev.sync_all().context("sync")?;
    v.flags = flags;
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

enum SegKind {
    Mem(usize, usize),
    /// (index into file_order, byte offset in the file)
    File(usize, u64),
}

struct Seg {
    dev: u64,
    len: u64,
    kind: SegKind,
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
    let data_write: u64 = segs
        .iter()
        .map(|s| match s.kind {
            SegKind::File(k, off) => {
                let left = nodes[file_order[k]].size - off;
                if left <= s.len { left.div_ceil(4096) * 4096 } else { s.len }.min(s.len)
            }
            SegKind::Mem(..) => s.len,
        })
        .sum();
    let write_bytes = data_write + p2.bytes() + p3_fat.bytes() + p3_dir.bytes() + 2 * v.bps;
    let fragmented = runs.iter().filter(|(o, r)| !matches!(o, Obj::Ext(_)) && r.len() > 1).count() as u64;
    let (files, dirs) = (new_files.len() as u64, new_dirs.len() as u64);
    let detail = format!(
        "plan: add {files} files, {dirs} dirs, {} data │ cluster {} │ {} free │ {} new objects fragmented │ will write {}{} │ verified {verified} objects in {:.0} ms",
        human_bytes(data_bytes as f64),
        human_bytes(cs as f64),
        human_bytes((free_total * cs) as f64),
        fragmented,
        human_bytes(write_bytes as f64),
        if skipped > 0 { format!(" │ skipping {skipped} existing") } else { String::new() },
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
    let flags = v.flags;
    write_flags(&mut v, working_flags(flags), None)?;
    stop_after("dirty")?;

    // ── phase 1: data, one ascending stream ──
    let empty_files = (files as usize - file_order.len()) as u64;
    tick.progress(empty_files, 0, true);
    if !segs.is_empty() {
        let direct =
            tgt.direct && cs.is_multiple_of(4096) && v.heap_off.is_multiple_of(4096) && set_direct(&excl, true);
        let mut w = if direct { SeqWriter::new(excl.clone()) } else { SeqWriter::with_align(excl.clone(), 1) };
        let reads: Vec<(PathBuf, u64)> = file_order.iter().map(|&i| (nodes[i].src.clone(), nodes[i].size)).collect();
        let (rx, budget) = ordered_reader(reads, o.readers.max(1), 256 << 20, 16 << 20);
        let mut pending: BTreeMap<usize, Item> = BTreeMap::new();
        enum Cur {
            Data(Vec<u8>),
            Stream(File),
        }
        let mut cur: Option<(usize, Cur)> = None;
        let mut last_k = 0usize;
        let (mut files_done, mut bytes_done) = (empty_files, 0u64);
        let mut prev_end = 0u64;
        for s in &segs {
            // Unwritten cluster slack after a file's data is not part of the file (beyond
            // ValidDataLength), so a big gap is skipped instead of zero-filled. A small gap is
            // filled to keep writes merged, but ONLY when it is our own slack (this segment
            // starts right where the previous one ended); otherwise the gap holds clusters that
            // belong to existing files.
            if s.dev > w.cur() {
                if s.dev == prev_end && s.dev - w.cur() <= SLACK_FILL {
                    w.zeros_to(s.dev)?;
                } else {
                    w.skip_to(s.dev)?;
                }
            }
            ensure!(w.cur() == s.dev, "internal layout error: overlapping data segments");
            prev_end = s.dev + s.len;
            match s.kind {
                SegKind::Mem(m, off) => w.write(&mems[m][off..off + s.len as usize])?,
                SegKind::File(k, off) => {
                    let n = &nodes[file_order[k]];
                    if cur.as_ref().map(|c| c.0) != Some(k) {
                        ensure!(cur.is_none() && k >= last_k, "internal layout error: interleaved files");
                        while !pending.contains_key(&k) {
                            let (idx, item) = rx.recv().context("reader stopped")?;
                            pending.insert(idx, item);
                        }
                        let c = match pending.remove(&k).unwrap() {
                            Item::Data(d) => {
                                ensure!(d.len() as u64 == n.size, "{} changed size while copying", n.src.display());
                                Cur::Data(d)
                            }
                            Item::Stream => {
                                let f = File::open(&n.src).with_context(|| format!("open {}", n.src.display()))?;
                                crate::util::fadvise_sequential(&f);
                                Cur::Stream(f)
                            }
                        };
                        cur = Some((k, c));
                        last_k = k;
                    }
                    let take = (n.size - off).min(s.len);
                    match &mut cur.as_mut().unwrap().1 {
                        Cur::Data(d) => w.write(&d[off as usize..(off + take) as usize])?,
                        Cur::Stream(f) => w.write_from(f, take)?,
                    }
                    if off + take == n.size {
                        // Pad to the I/O alignment only; the rest of the cluster is slack.
                        // Never past the run's end: with clusters under 4 KB the next cluster
                        // may belong to someone else.
                        w.zeros_to((w.cur().div_ceil(4096) * 4096).min(s.dev + s.len))?;
                        if matches!(cur.take(), Some((_, Cur::Data(_)))) {
                            release(&budget, n.size.min(256 << 20));
                        }
                        files_done += 1;
                        bytes_done += n.size;
                        tick.progress(files_done, bytes_done, false);
                    }
                }
            }
        }
        ensure!(cur.is_none(), "internal layout error: a file was not fully written");
        w.finish()?; // syncs
        set_direct(&excl, false); // metadata phases are small buffered writes
    }
    stop_after("data")?;

    // ── phase 2: bitmap + new chains ──
    p2.commit(&v.dev)?;
    v.bitmap = bm;
    stop_after("alloc")?;

    // ── phase 3: link into existing chains, then directory entries ──
    p3_fat.commit(&v.dev)?;
    stop_after("fat")?;
    p3_dir.commit(&v.dev)?;

    // ── phase 4: clean ──
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

    #[test]
    fn volume_info_json() {
        let i = VolumeInfo { label: "A\"B".into(), cluster: 4096, size: 10, free: 5, files: 1, dirs: 2, dirty: false };
        assert_eq!(
            i.to_json(),
            r#"{"label":"A\"B","cluster":4096,"size":10,"free":5,"files":1,"dirs":2,"dirty":false}"#
        );
    }
}
