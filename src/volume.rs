//! The validating exFAT reader shared by `add`, `info` and `pull`: boot region and checksum,
//! FAT (through a bounded page cache), allocation bitmap, up-case table, directory parsing with
//! entry-set checksum checks, and the reachability walk that checks every referenced cluster
//! against the bitmap.

use crate::exfat::{checksum16, checksum32, upcase_from};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;

pub(crate) const EOC: u32 = 0xFFFF_FFFF;
/// The spec caps a directory at 256 MB.
pub(crate) const MAX_DIR_BYTES: u64 = 256 << 20;
/// Largest ClusterCount the spec allows.
pub(crate) const MAX_CLUSTER_COUNT: u64 = 0xFFFF_FFF5;
/// FAT cache: FAT_CACHE_PAGES pages of FAT_PAGE bytes (4 MB), whatever the volume size.
pub(crate) const FAT_PAGE: u64 = 64 << 10;
pub(crate) const FAT_CACHE_PAGES: usize = 64;
pub(crate) const FLAG_ACTIVE_FAT: u16 = 1;
pub(crate) const FLAG_DIRTY: u16 = 2;
pub(crate) const FLAG_MEDIA_FAILURE: u16 = 4;
/// ClearToZero: the spec says to clear it before modifying the file system.
pub(crate) const FLAG_CLEAR_TO_ZERO: u16 = 8;

// ───────────────────────── parsed volume ─────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct Child {
    pub(crate) name: Vec<u16>,
    pub(crate) is_dir: bool,
    pub(crate) first: u32,
    pub(crate) no_fat_chain: bool,
    pub(crate) len: u64,
    /// ValidDataLength: bytes past it read as zeros.
    pub(crate) valid_len: u64,
    /// LastModified timestamp, 10 ms increment and UTC offset, as stored.
    pub(crate) mtime: (u32, u8, u8),
    /// Byte offset and length of this child's entry set inside its parent's data.
    pub(crate) set_off: usize,
    pub(crate) set_len: usize,
    /// FileAttributes (ReadOnly 0x01, Hidden 0x02, System 0x04, Directory 0x10, Archive 0x20).
    pub(crate) attrs: u16,
}

pub(crate) struct Dir {
    pub(crate) clusters: Vec<u32>,
    pub(crate) no_fat_chain: bool,
    pub(crate) data: Vec<u8>,
    /// Byte offset of the end-of-directory marker (or data.len() if the directory is full).
    pub(crate) end: usize,
    /// Up-cased name → child.
    pub(crate) children: HashMap<Vec<u16>, Child>,
    /// (parent dir index, set offset, set length) of this directory's own entry set.
    pub(crate) entry: Option<(usize, usize, usize)>,
    pub(crate) path: String,
    /// Up-cased path components from the root (the key into `Vol::kept`).
    pub(crate) key: Vec<Vec<u16>>,
    /// Further live entry sets whose up-cased name equals one in `children` (the window of an
    /// interrupted `flashcp sync`; only with [`Vol::tolerate_dups`], otherwise refused).
    pub(crate) dups: Vec<Child>,
    /// Offsets of orphan secondaries: in-use secondary entries left after a deleted file entry
    /// (0x05) by an interrupted `flashcp sync`, within that entry's SecondaryCount. Skipped.
    pub(crate) orphans: Vec<usize>,
}

#[derive(Default)]
pub(crate) struct RootExtras {
    pub(crate) bitmaps: Vec<[u8; 32]>,
    pub(crate) upcase: Vec<[u8; 32]>,
    pub(crate) label: String,
}

pub(crate) struct Vol {
    pub(crate) dev: File,
    pub(crate) dev_len: u64,
    pub(crate) bps: u64,
    pub(crate) cs: u64,
    pub(crate) fat_off: u64,
    pub(crate) heap_off: u64,
    pub(crate) cc: u64,
    pub(crate) flags: u16,
    pub(crate) sector0: Vec<u8>,
    /// VolumeLength in bytes (validated ≤ the device length): nothing past it is read.
    pub(crate) vol_bytes: u64,
    /// Bounded FAT page cache (FIFO eviction): at most FAT_CACHE_PAGES × FAT_PAGE bytes.
    pub(crate) fat_cache: HashMap<u64, Vec<u8>>,
    pub(crate) fat_order: std::collections::VecDeque<u64>,
    /// Directories parsed by the verification walk that planning needs, by `Dir::key`.
    pub(crate) kept: HashMap<Vec<Vec<u16>>, usize>,
    pub(crate) bitmap: Vec<u8>,
    pub(crate) bitmap_clusters: Vec<u32>,
    pub(crate) upcase_clusters: Vec<u32>,
    pub(crate) upc: Vec<u16>,
    pub(crate) label: String,
    pub(crate) dirs: Vec<Dir>,
    /// Accept duplicate names (into `Dir::dups`) and the zero-length directory entries
    /// `fsck.exfat -y` leaves of one: only `sync`, which repairs them, sets it.
    pub(crate) tolerate_dups: bool,
}

pub(crate) fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub(crate) fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl Vol {
    /// A copy of the geometry with its own handle on the device (no caches, bitmap,
    /// directories or up-case table), for reads and offsets on another thread.
    pub(crate) fn view(&self) -> Result<Vol> {
        Ok(Vol {
            dev: self.dev.try_clone().context("dup the device handle")?,
            dev_len: self.dev_len,
            bps: self.bps,
            cs: self.cs,
            fat_off: self.fat_off,
            heap_off: self.heap_off,
            cc: self.cc,
            flags: self.flags,
            sector0: self.sector0.clone(),
            vol_bytes: self.vol_bytes,
            fat_cache: HashMap::new(),
            fat_order: Default::default(),
            kept: HashMap::new(),
            bitmap: Vec::new(),
            bitmap_clusters: self.bitmap_clusters.clone(),
            upcase_clusters: Vec::new(),
            upc: Vec::new(),
            label: String::new(),
            dirs: Vec::new(),
            tolerate_dups: self.tolerate_dups,
        })
    }

    pub(crate) fn read_at(&self, off: u64, len: u64) -> Result<Vec<u8>> {
        ensure!(
            off.checked_add(len).is_some_and(|e| e <= self.dev_len),
            "volume structure at byte {off} lies outside the device"
        );
        let mut v = vec![0u8; len as usize];
        crate::sim::trace::serial(crate::sim::trace::Op::Read, off, len, || self.dev.read_exact_at(&mut v, off))
            .with_context(|| format!("read {len} bytes at {off}"))?;
        Ok(v)
    }

    /// Parses and validates the boot region, bitmap, up-case table and root directory.
    /// `read_bitmap: false` validates the bitmap's entry and chain but does not load it (for
    /// read-only walks that never consult it; `bitmap` stays empty).
    pub(crate) fn open(dev: File, dev_len: u64, allow_dirty: bool, read_bitmap: bool) -> Result<Vol> {
        Vol::open_with(dev, dev_len, allow_dirty, read_bitmap, false)
    }

    /// [`Vol::open`], optionally tolerating duplicate names (see [`Vol::tolerate_dups`]).
    pub(crate) fn open_with(
        dev: File,
        dev_len: u64,
        allow_dirty: bool,
        read_bitmap: bool,
        tolerate_dups: bool,
    ) -> Result<Vol> {
        let v = Vol::open_boot(dev, dev_len, allow_dirty, tolerate_dups)?;
        Vol::open_rest(v, read_bitmap)
    }

    /// Parses and validates the boot region only (no directory is read).
    pub(crate) fn open_boot(dev: File, dev_len: u64, allow_dirty: bool, tolerate_dups: bool) -> Result<Vol> {
        ensure!(dev_len >= 24 * 512, "target is too small to hold an exFAT volume");
        let mut s0 = vec![0u8; 512];
        crate::sim::trace::serial(crate::sim::trace::Op::Read, 0, 512, || dev.read_exact_at(&mut s0, 0))
            .context("read boot sector")?;
        ensure!(&s0[3..11] == b"EXFAT   ", "target does not hold an exFAT volume (no EXFAT signature)");
        ensure!(s0[11..64].iter().all(|&b| b == 0), "boot sector MustBeZero area is not zero");
        ensure!(s0[510..512] == [0x55, 0xAA], "boot sector has no 0x55AA signature");
        let bps_shift = s0[108] as u32;
        ensure!((9..=12).contains(&bps_shift), "unsupported BytesPerSectorShift {bps_shift}");
        let spc_shift = s0[109] as u32;
        ensure!(spc_shift <= 25 - bps_shift, "unsupported SectorsPerClusterShift {spc_shift}");
        ensure!(s0[105] == 1, "unsupported exFAT revision {}.{:02}", s0[105], s0[104]);
        ensure!(s0[110] == 1, "volume has {} FATs (TexFAT); only single-FAT exFAT is supported", s0[110]);
        let bps = 1u64 << bps_shift;
        let cs = bps << spc_shift;
        ensure!(dev_len >= 12 * bps, "target is too small for its boot region");
        let mut region = vec![0u8; 12 * bps as usize];
        let rl = region.len() as u64;
        crate::sim::trace::serial(crate::sim::trace::Op::Read, 0, rl, || dev.read_exact_at(&mut region, 0))
            .context("read boot region")?;
        let want = checksum32(&region[..11 * bps as usize], &[106, 107, 112], 0);
        for w in region[11 * bps as usize..].as_chunks::<4>().0 {
            ensure!(u32::from_le_bytes(*w) == want, "boot region checksum mismatch; refusing to modify this volume");
        }
        let vol_sectors = le64(&region, 72);
        let fat_off = le32(&region, 80) as u64 * bps;
        let fat_len = le32(&region, 84) as u64 * bps;
        let heap_off = le32(&region, 88) as u64 * bps;
        let cc = le32(&region, 92) as u64;
        let root = le32(&region, 96);
        let flags = le16(&region, 106);
        // Spec limits, checked before any arithmetic uses them.
        ensure!((1..=MAX_CLUSTER_COUNT).contains(&cc), "ClusterCount {cc} is outside the exFAT limit");
        ensure!(vol_sectors.checked_mul(bps).is_some_and(|b| b <= dev_len), "VolumeLength is larger than the target");
        ensure!(fat_off >= 24 * bps && fat_len >= (cc + 2) * 4, "inconsistent FAT geometry (FatLength too small)");
        ensure!(
            heap_off >= fat_off + fat_len && heap_off + cc * cs <= vol_sectors * bps,
            "inconsistent heap geometry (ClusterHeapOffset + ClusterCount × cluster > VolumeLength)"
        );
        ensure!((2..cc + 2).contains(&(root as u64)), "root directory cluster {root} out of range");
        ensure!(flags & FLAG_ACTIVE_FAT == 0, "volume uses the second FAT (TexFAT); not supported");
        ensure!(flags & FLAG_MEDIA_FAILURE == 0, "volume is flagged with a media failure; refusing to write");
        ensure!(
            allow_dirty || flags & FLAG_DIRTY == 0,
            "volume is marked dirty (still mounted, or not cleanly unmounted); unmount it or run fsck.exfat first"
        );
        let v = Vol {
            dev,
            dev_len,
            bps,
            cs,
            fat_off,
            heap_off,
            cc,
            flags,
            sector0: region[..bps as usize].to_vec(),
            vol_bytes: vol_sectors * bps,
            fat_cache: HashMap::new(),
            fat_order: Default::default(),
            kept: HashMap::new(),
            bitmap: Vec::new(),
            bitmap_clusters: Vec::new(),
            upcase_clusters: Vec::new(),
            upc: Vec::new(),
            label: String::new(),
            dirs: Vec::new(),
            tolerate_dups,
        };
        Ok(v)
    }

    pub(crate) fn open_rest(mut v: Vol, read_bitmap: bool) -> Result<Vol> {
        let (cs, cc, tolerate_dups) = (v.cs, v.cc, v.tolerate_dups);
        let root = le32(&v.sector0, 96);
        let root_clusters = v.chain(root, "root directory", MAX_DIR_BYTES / cs)?;
        ensure!(root_clusters.len() as u64 * cs <= MAX_DIR_BYTES, "root directory is larger than 256 MB");
        let data = v.read_clusters(&root_clusters, root_clusters.len() as u64 * cs)?;
        let mut extras = RootExtras::default();
        let (end, children, orphans) = parse_dir(&data, Some(&mut extras), "/", cs, tolerate_dups)?;

        ensure!(!extras.bitmaps.is_empty(), "root directory has no allocation bitmap entry");
        ensure!(
            extras.bitmaps.len() == 1 && extras.bitmaps[0][1] & 1 == 0,
            "volume has two allocation bitmaps (TexFAT); not supported"
        );
        let bm = extras.bitmaps[0];
        let bm_len = le64(&bm, 24);
        ensure!(bm_len == cc.div_ceil(8), "allocation bitmap length {bm_len} does not match the cluster count");
        v.bitmap_clusters = v.object_clusters(le32(&bm, 20), false, bm_len, "allocation bitmap")?;
        if read_bitmap {
            v.bitmap = v.read_clusters(&v.bitmap_clusters, bm_len)?;
        }

        ensure!(extras.upcase.len() == 1, "root directory must have exactly one up-case table entry");
        let uc = extras.upcase[0];
        let uc_len = le64(&uc, 24);
        ensure!((2..=2 * 65536).contains(&uc_len), "implausible up-case table length {uc_len}");
        let uc_clusters = v.object_clusters(le32(&uc, 20), false, uc_len, "up-case table")?;
        let raw = v.read_clusters(&uc_clusters, uc_len)?;
        v.upcase_clusters = uc_clusters;
        ensure!(checksum32(&raw, &[], 0) == le32(&uc, 4), "up-case table checksum mismatch");
        v.upc = upcase_from(&raw);
        v.label = extras.label;
        let (children, dups) = rekey(children, &v.upc, "/", tolerate_dups)?;
        v.dirs.push(Dir {
            clusters: root_clusters,
            no_fat_chain: false,
            data,
            end,
            children,
            entry: None,
            path: String::new(),
            key: Vec::new(),
            dups,
            orphans,
        });
        v.kept.insert(Vec::new(), 0);
        Ok(v)
    }

    pub(crate) fn valid(&self, c: u32) -> bool {
        c >= 2 && (c as u64) < self.cc + 2
    }

    pub(crate) fn fat(&mut self, c: u32) -> Result<u32> {
        let off = self.fat_off + c as u64 * 4;
        let page = off / FAT_PAGE;
        if !self.fat_cache.contains_key(&page) {
            let start = page * FAT_PAGE;
            let len = FAT_PAGE.min(self.dev_len - start);
            let p = self.read_at(start, len)?;
            if self.fat_order.len() >= FAT_CACHE_PAGES {
                let old = self.fat_order.pop_front().unwrap();
                self.fat_cache.remove(&old);
            }
            self.fat_cache.insert(page, p);
            self.fat_order.push_back(page);
        }
        debug_assert!(self.fat_cache.len() <= FAT_CACHE_PAGES);
        let p = &self.fat_cache[&page];
        Ok(le32(p, (off - page * FAT_PAGE) as usize))
    }

    /// A FAT chain up to EOC, refused once it exceeds `max` clusters (a loop, or simply too
    /// long), so a crafted chain can never make this walk or allocate more than `max`.
    pub(crate) fn chain(&mut self, first: u32, what: &str, max: u64) -> Result<Vec<u32>> {
        let mut v = Vec::new();
        let mut c = first;
        loop {
            ensure!(self.valid(c), "{what}: FAT chain points at invalid cluster {c:#x}");
            v.push(c);
            ensure!(v.len() as u64 <= max.min(self.cc), "{what}: FAT chain is too long (or has a loop)");
            let n = self.fat(c)?;
            if n == EOC {
                return Ok(v);
            }
            c = n;
        }
    }

    /// Clusters of an object of `len` bytes; the chain must match the length exactly.
    pub(crate) fn object_clusters(&mut self, first: u32, no_fat_chain: bool, len: u64, what: &str) -> Result<Vec<u32>> {
        let n = len.div_ceil(self.cs);
        if n == 0 {
            return Ok(Vec::new());
        }
        if no_fat_chain {
            let run = contiguous_run(first, n, self.cc)
                .with_context(|| format!("{what}: contiguous extent runs past the end of the volume"))?;
            return Ok(run.map(|c| c as u32).collect());
        }
        // Walk exactly n clusters, then demand EOC: bounded by the length, whatever the FAT says.
        let mut v = Vec::with_capacity(n.min(self.cc) as usize);
        let mut c = first;
        for k in 0..n {
            ensure!(self.valid(c), "{what}: FAT chain points at invalid cluster {c:#x}");
            v.push(c);
            let next = self.fat(c)?;
            if k + 1 == n {
                ensure!(next == EOC, "{what}: FAT chain is longer than its length needs ({n} clusters)");
            } else {
                ensure!(next != EOC, "{what}: FAT chain has {} clusters, its length needs {n}", k + 1);
                c = next;
            }
        }
        Ok(v)
    }

    pub(crate) fn cluster_off(&self, c: u32) -> u64 {
        self.heap_off + (c as u64 - 2) * self.cs
    }

    /// Reads an object straight into one buffer of `len` bytes (no per-run copies, so the
    /// peak is `len`, which matters for a bitmap of up to 512 MB).
    pub(crate) fn read_clusters(&self, clusters: &[u32], len: u64) -> Result<Vec<u8>> {
        ensure!(len <= clusters.len() as u64 * self.cs, "object is shorter than its length");
        let mut out = vec![0u8; len as usize];
        let (mut i, mut done) = (0, 0usize);
        while i < clusters.len() && done < out.len() {
            let mut j = i + 1;
            while j < clusters.len() && clusters[j] == clusters[j - 1] + 1 {
                j += 1;
            }
            let want = ((j - i) as u64 * self.cs).min((out.len() - done) as u64);
            let off = self.cluster_off(clusters[i]);
            ensure!(
                off.checked_add(want).is_some_and(|e| e <= self.dev_len),
                "volume structure at byte {off} lies outside the device"
            );
            let dst = &mut out[done..done + want as usize];
            crate::sim::trace::serial(crate::sim::trace::Op::Read, off, want, || self.dev.read_exact_at(dst, off))
                .with_context(|| format!("read {want} bytes at {off}"))?;
            done += want as usize;
            i = j;
        }
        Ok(out)
    }

    /// A child directory of `parent`, as parsed and verified by the walk (never re-read, so
    /// planning sees exactly what was verified); its index in `dirs`.
    pub(crate) fn load_child(&self, parent: usize, ch: &Child) -> Result<usize> {
        let mut key = self.dirs[parent].key.clone();
        key.push(upcase(&ch.name, &self.upc));
        self.kept.get(&key).copied().ok_or_else(|| {
            anyhow!(
                "internal error: {}/{} was not kept by the verification walk",
                self.dirs[parent].path,
                String::from_utf16_lossy(&ch.name)
            )
        })
    }

    pub(crate) fn read_child(&mut self, parent: usize, parent_path: &str, ch: &Child) -> Result<Dir> {
        let path = format!("{parent_path}/{}", String::from_utf16_lossy(&ch.name));
        ensure!(
            ch.len > 0 && ch.len.is_multiple_of(self.cs) && ch.len <= MAX_DIR_BYTES,
            "directory {path} has invalid size {}",
            ch.len
        );
        let clusters = self.object_clusters(ch.first, ch.no_fat_chain, ch.len, &path)?;
        let data = self.read_clusters(&clusters, ch.len)?;
        let (end, children, orphans) = parse_dir(&data, None, &path, self.cs, self.tolerate_dups)?;
        let (children, dups) = rekey(children, &self.upc, &path, self.tolerate_dups)?;
        Ok(Dir {
            clusters,
            no_fat_chain: ch.no_fat_chain,
            data,
            end,
            children,
            entry: Some((parent, ch.set_off, ch.set_len)),
            path,
            key: Vec::new(),
            dups,
            orphans,
        })
    }

    /// Walks everything the volume references (root, bitmap, up-case table, every directory
    /// and every file stream, NoFatChain runs and FAT chains alike, bounded by DataLength) and
    /// checks it against the allocation bitmap. Refuses a referenced cluster that the bitmap
    /// calls free, a cluster referenced twice (cross-link), and chains that loop, leave the
    /// heap, or do not match DataLength. Allocated-but-unreferenced (leaked) clusters are fine:
    /// allocation only ever uses clusters the bitmap calls free. Returns objects checked.
    ///
    /// Directories whose up-cased path is in `wanted` are kept in `dirs` (indexed by `kept`) for
    /// planning; the rest are dropped as soon as they are walked, so memory stays bounded.
    pub(crate) fn verify_allocation(&mut self, wanted: &HashSet<Vec<Vec<u16>>>) -> Result<u64> {
        self.verify_allocation_with(&|k| wanted.contains(k))
    }

    /// [`Vol::verify_allocation`] keeping the directories whose up-cased path `keep` accepts
    /// (it must accept every prefix of an accepted path).
    pub(crate) fn verify_allocation_with(&mut self, keep: &dyn Fn(&[Vec<u16>]) -> bool) -> Result<u64> {
        let mut refd = cluster_set(self.cc);
        let mut objects = 0u64;
        let sys = [
            (self.dirs[0].clusters.clone(), "the root directory"),
            (self.bitmap_clusters.clone(), "the allocation bitmap"),
            (self.upcase_clusters.clone(), "the up-case table"),
        ];
        for (clusters, what) in &sys {
            for &c in clusters {
                claim(&mut refd, &self.bitmap, c, &|| what.to_string())?;
            }
            objects += 1;
        }
        let root_dups = (self.dirs[0].dups.clone(), self.dirs[0].children.clone());
        objects += self.claim_dups(&mut refd, "", &root_dups.0, &root_dups.1)?;
        // (key, path, children, index in dirs if kept)
        type Work = (Vec<Vec<u16>>, String, Vec<Child>, Option<usize>);
        let mut stack: Vec<Work> =
            vec![(Vec::new(), String::new(), self.dirs[0].children.values().cloned().collect(), Some(0))];
        while let Some((key, path, kids, kept_at)) = stack.pop() {
            for ch in kids {
                objects += 1;
                let what = || format!("{path}/{}", String::from_utf16_lossy(&ch.name));
                if ch.is_dir {
                    let mut d = self.read_child(kept_at.unwrap_or(0), &path, &ch)?;
                    for &c in &d.clusters {
                        claim(&mut refd, &self.bitmap, c, &what)?;
                    }
                    let mut ckey = key.clone();
                    ckey.push(upcase(&ch.name, &self.upc));
                    let grand: Vec<Child> = d.children.values().cloned().collect();
                    let dpath = d.path.clone();
                    objects += self.claim_dups(&mut refd, &dpath, &d.dups, &d.children)?;
                    // A wanted directory's parent is wanted too (prefixes), so kept_at is Some.
                    let at = if kept_at.is_some() && keep(&ckey) {
                        d.key = ckey.clone();
                        self.dirs.push(d);
                        let i = self.dirs.len() - 1;
                        self.kept.insert(ckey.clone(), i);
                        Some(i)
                    } else {
                        None
                    };
                    stack.push((ckey, dpath, grand, at));
                } else {
                    self.claim_stream(&mut refd, &ch, &what)?;
                }
            }
        }
        Ok(objects)
    }

    /// Claims the clusters of a directory's duplicate entry sets. A duplicate sharing its
    /// twin's FirstCluster (a relocated entry set: one object under two sets) is claimed once,
    /// through the twin; a zero-length directory (what `fsck.exfat -y` leaves of such a pair)
    /// owns nothing. A duplicate directory with clusters of its own is no state flashcp leaves.
    fn claim_dups(
        &mut self,
        refd: &mut [u8],
        path: &str,
        dups: &[Child],
        children: &HashMap<Vec<u16>, Child>,
    ) -> Result<u64> {
        for ch in dups {
            let twin = &children[&upcase(&ch.name, &self.upc)];
            let what = || format!("{path}/{}", String::from_utf16_lossy(&ch.name));
            if ch.first != 0 && ch.first == twin.first {
                ensure!(
                    ch.is_dir == twin.is_dir && ch.len == twin.len && ch.no_fat_chain == twin.no_fat_chain,
                    "{}: two entries share a first cluster but differ; run fsck.exfat",
                    what()
                );
                continue;
            }
            if ch.is_dir {
                ensure!(ch.len == 0, "{}: a duplicate directory with clusters of its own; run fsck.exfat", what());
                continue;
            }
            self.claim_stream(refd, ch, &what)?;
        }
        Ok(dups.len() as u64)
    }

    /// Claims a file's clusters without materializing them (files can be huge).
    pub(crate) fn claim_stream(&mut self, refd: &mut [u8], ch: &Child, what: &dyn Fn() -> String) -> Result<()> {
        let n = ch.len.div_ceil(self.cs);
        if n == 0 {
            return Ok(());
        }
        if ch.no_fat_chain {
            let run = contiguous_run(ch.first, n, self.cc)
                .with_context(|| format!("{}: contiguous extent runs past the end of the volume", what()))?;
            for c in run {
                claim(refd, &self.bitmap, c as u32, what)?; // c < cc + 2 ≤ 0xFFFF_FFF7
            }
            return Ok(());
        }
        let mut c = ch.first;
        for k in 0..n {
            ensure!(self.valid(c), "{}: FAT chain points at invalid cluster {c:#x}", what());
            // A loop revisits a cluster, which claim() reports as a cross-link.
            claim(refd, &self.bitmap, c, what)?;
            let next = self.fat(c)?;
            if k + 1 == n {
                ensure!(next == EOC, "{}: FAT chain is longer than its DataLength", what());
            } else {
                ensure!(next != EOC, "{}: FAT chain is shorter than its DataLength", what());
                c = next;
            }
        }
        Ok(())
    }

    /// The clusters of a file of `len` bytes as ascending-in-file-order runs (start, count),
    /// adjacent clusters merged. Bounded by the length, like [`Vol::object_clusters`].
    pub(crate) fn runs(&mut self, first: u32, no_fat_chain: bool, len: u64, what: &str) -> Result<Vec<(u64, u64)>> {
        let n = len.div_ceil(self.cs);
        if n == 0 {
            return Ok(Vec::new());
        }
        if no_fat_chain {
            let r = contiguous_run(first, n, self.cc)
                .with_context(|| format!("{what}: contiguous extent runs past the end of the volume"))?;
            return Ok(vec![(r.start, n)]);
        }
        let mut out: Vec<(u64, u64)> = Vec::new();
        let mut c = first;
        for k in 0..n {
            ensure!(self.valid(c), "{what}: FAT chain points at invalid cluster {c:#x}");
            match out.last_mut() {
                Some(l) if l.0 + l.1 == c as u64 => l.1 += 1,
                _ => out.push((c as u64, 1)),
            }
            let next = self.fat(c)?;
            if k + 1 == n {
                ensure!(next == EOC, "{what}: FAT chain is longer than its DataLength");
            } else {
                ensure!(next != EOC, "{what}: FAT chain is shorter than its DataLength");
                c = next;
            }
        }
        Ok(out)
    }

    pub(crate) fn free_clusters(&self) -> u64 {
        let used: u64 = self.bitmap.iter().map(|b| b.count_ones() as u64).sum();
        // Bits past ClusterCount in the last byte are not clusters.
        let pad = self.bitmap.len() as u64 * 8 - self.cc;
        let last = *self.bitmap.last().unwrap_or(&0);
        let pad_used = if pad > 0 { (last >> (8 - pad)).count_ones() as u64 } else { 0 };
        self.cc - (used - pad_used)
    }
}

/// Clusters [first, first + n) of a contiguous (NoFatChain) object, in u64 so nothing can
/// overflow; None unless the whole run lies inside the heap (2 ..= cc + 1).
pub(crate) fn contiguous_run(first: u32, n: u64, cc: u64) -> Result<std::ops::Range<u64>> {
    let first = first as u64;
    ensure!(first >= 2 && n >= 1 && first.checked_add(n).is_some_and(|e| e <= cc + 2), "out of range");
    Ok(first..first + n)
}

/// A visited-cluster bitset (one bit per heap cluster), as used by the reachability walks.
pub(crate) fn cluster_set(cc: u64) -> Vec<u8> {
    vec![0u8; cc.div_ceil(8) as usize]
}

/// Sets cluster `c` (≥ 2, inside the heap) in a [`cluster_set`]; false if it was already set.
pub(crate) fn mark(set: &mut [u8], c: u32) -> bool {
    let i = (c - 2) as usize;
    let (byte, bit) = (i / 8, 1u8 << (i % 8));
    let fresh = set[byte] & bit == 0;
    set[byte] |= bit;
    fresh
}

/// Sets clusters [first, first + n) (inside the heap); on a cluster that was already set,
/// returns it (the bits before it stay set). Whole bytes go eight at a time.
pub(crate) fn mark_range(set: &mut [u8], first: u32, n: u64) -> std::result::Result<(), u32> {
    let (mut i, end) = (first as u64 - 2, first as u64 - 2 + n);
    while i < end {
        if i.is_multiple_of(8) && end - i >= 8 {
            let b = &mut set[(i / 8) as usize];
            if *b != 0 {
                return Err((i + 2 + b.trailing_zeros() as u64) as u32);
            }
            *b = 0xFF;
            i += 8;
        } else {
            if !mark(set, (i + 2) as u32) {
                return Err((i + 2) as u32);
            }
            i += 1;
        }
    }
    Ok(())
}

/// Marks cluster `c` referenced; refuses cross-links and clusters the bitmap calls free.
pub(crate) fn claim(refd: &mut [u8], bitmap: &[u8], c: u32, what: &dyn Fn() -> String) -> Result<()> {
    let i = (c - 2) as usize;
    let (byte, bit) = (i / 8, 1u8 << (i % 8));
    ensure!(
        mark(refd, c),
        "cluster {c} is used twice (cross-linked), again by {}; refusing to modify this volume (run fsck.exfat)",
        what()
    );
    ensure!(
        bitmap[byte] & bit != 0,
        "cluster {c} used by {} is marked free in the allocation bitmap; refusing to modify this volume (run fsck.exfat)",
        what()
    );
    Ok(())
}

/// The name of the on-volume journal of a running `flashcp sync` (see [`crate::journal`]): a
/// regular file in the root directory, Hidden | System | Archive.
pub(crate) const SYNC_JOURNAL_NAME: &str = ".flashcp-sync-journal";
pub(crate) const SYNC_JOURNAL_ATTRS: u16 = 0x02 | 0x04 | 0x20;

/// Whether `ch`, a child of the ROOT directory, is the on-volume sync journal: that exact name,
/// a file, Hidden and System. Every command that lists user files skips it.
pub(crate) fn is_sync_journal(ch: &Child) -> bool {
    !ch.is_dir && ch.attrs & 0x06 == 0x06 && ch.name.iter().copied().eq(SYNC_JOURNAL_NAME.encode_utf16())
}

/// Whether an entry set's SetChecksum is right.
pub(crate) fn checksum_ok_set(set: &[u8]) -> bool {
    set.len() >= 64 && checksum16(set, true) == le16(set, 2)
}

/// Whether a raw, live entry set (file entry first) is the on-volume sync journal's: a file,
/// Hidden and System, named exactly [`SYNC_JOURNAL_NAME`].
pub(crate) fn is_sync_journal_set(set: &[u8]) -> bool {
    let name: Vec<u16> = SYNC_JOURNAL_NAME.encode_utf16().collect();
    let entries = name.len().div_ceil(15);
    if set.len() != 32 * (2 + entries) || set[0] != 0x85 || set[1] as usize != 1 + entries || set[32] != 0xC0 {
        return false;
    }
    if le16(set, 4) & 0x16 != 0x06 || set[35] as usize != name.len() {
        return false;
    }
    (0..entries).all(|k| set[64 + 32 * k] == 0xC1)
        && name.iter().enumerate().all(|(i, &c)| le16(set, 64 + 32 * (i / 15) + 2 + 2 * (i % 15)) == c)
}

/// Refuses a source tree that would put an entry named like the sync journal (any case) into
/// the volume's root: `to` is the destination inside the volume ("" or None for the root).
pub(crate) fn refuse_journal_name(src: &std::path::Path, to: Option<&str>) -> Result<()> {
    if to.unwrap_or("").split('/').any(|c| !c.is_empty() && c != ".") {
        return Ok(());
    }
    let Ok(rd) = std::fs::read_dir(src) else { return Ok(()) };
    for e in rd.flatten() {
        if e.file_name().to_str().is_some_and(|n| n.eq_ignore_ascii_case(SYNC_JOURNAL_NAME)) {
            bail!(
                "{} would be written to the volume's root as {SYNC_JOURNAL_NAME}, the name flashcp sync keeps its \
                 on-volume journal under; rename it (nothing was written)",
                e.path().display()
            );
        }
    }
    Ok(())
}

/// Children of a directory by up-cased name.
pub(crate) type Children = HashMap<Vec<u16>, Child>;

/// The hint for a volume in the window of an interrupted `flashcp sync`.
pub(crate) const SYNC_HINT: &str =
    "an interrupted flashcp sync: run `flashcp sync --recover <target>` or re-run the sync";

/// Children by up-cased name. Two live sets with one up-cased name are refused, unless
/// `tolerate` (sync): then, per name, one set stays in the map (a directory with clusters if
/// there is one, else the lowest offset) and the others are returned as duplicates. A
/// zero-length directory (only accepted by a tolerant parse) must have such a twin.
pub(crate) fn rekey(children: Vec<Child>, upc: &[u16], path: &str, tolerate: bool) -> Result<(Children, Vec<Child>)> {
    let mut m: HashMap<Vec<u16>, Child> = HashMap::with_capacity(children.len());
    let mut dups = Vec::new();
    let shown = |c: &Child| format!("{}/{}", path.trim_end_matches('/'), String::from_utf16_lossy(&c.name));
    for c in children {
        let key = upcase(&c.name, upc);
        match m.get_mut(&key) {
            None => {
                m.insert(key, c);
            }
            Some(_) if !tolerate => {
                bail!("{} is there twice (two entries with one name; {SYNC_HINT}; otherwise run fsck.exfat)", shown(&c))
            }
            Some(have) => {
                // A directory that owns clusters wins over a zero-length one.
                if have.is_dir && have.len == 0 && !(c.is_dir && c.len == 0) {
                    dups.push(std::mem::replace(have, c));
                } else {
                    dups.push(c);
                }
            }
        }
    }
    for c in m.values() {
        ensure!(
            !(c.is_dir && c.len == 0),
            "directory {}: DataLength 0 is not a non-zero multiple of the cluster size; run fsck.exfat",
            shown(c)
        );
    }
    Ok((m, dups))
}

pub(crate) fn upcase(name: &[u16], upc: &[u16]) -> Vec<u16> {
    name.iter().map(|&c| upc[c as usize]).collect()
}

/// Parses a directory's entries. Returns (end-of-directory offset, children). Unknown benign
/// entries are skipped; unknown critical ones are refused.
/// Every file's ValidDataLength ≤ DataLength and every directory's ValidDataLength ==
/// DataLength, a non-zero multiple of the cluster size `cs` (≤ 256 MB) is checked here, for
/// every entry.
///
/// Orphan secondaries (in-use secondary entries after a deleted file entry 0x05, within its
/// SecondaryCount: what an interrupted `flashcp sync` leaves between its unlink and free
/// phases) are skipped and their offsets returned. With `lax`, a directory entry with
/// DataLength 0 and no cluster (what `fsck.exfat -y` makes of the second of two sets sharing
/// clusters) is returned as such; [`rekey`] then demands its twin.
pub(crate) type Parsed = (usize, Vec<Child>, Vec<usize>);

pub(crate) fn parse_dir(
    data: &[u8],
    mut root: Option<&mut RootExtras>,
    path: &str,
    cs: u64,
    lax: bool,
) -> Result<Parsed> {
    let mut kids = Vec::new();
    let mut orphans = Vec::new();
    let mut i = 0usize;
    let at = |i: usize| format!("directory {path}, entry {}", i / 32);
    while i + 32 <= data.len() {
        let t = data[i];
        if t == 0 {
            return Ok((i, kids, orphans));
        }
        if t & 0x80 == 0 {
            // Unused / deleted entry. After a deleted file entry, in-use secondaries within its
            // SecondaryCount are orphans; the scan stops at the first primary or end marker.
            let sc = if t == 0x05 { data[i + 1] as usize } else { 0 };
            i += 32;
            for _ in 0..sc {
                if i + 32 > data.len() || data[i] & 0x40 == 0 {
                    break;
                }
                if data[i] & 0x80 != 0 {
                    orphans.push(i);
                }
                i += 32;
            }
            continue;
        }
        let secondary = t & 0x40 != 0;
        let benign = t & 0x20 != 0;
        ensure!(!secondary, "{}: secondary entry {t:#04x} outside an entry set", at(i));
        match (t, root.as_deref_mut()) {
            (0x81, Some(r)) => r.bitmaps.push(data[i..i + 32].try_into().unwrap()),
            (0x82, Some(r)) => r.upcase.push(data[i..i + 32].try_into().unwrap()),
            (0x83, Some(r)) => {
                let n = (data[i + 1] as usize).min(11);
                let units: Vec<u16> = (0..n).map(|j| le16(data, i + 2 + 2 * j)).collect();
                r.label = String::from_utf16_lossy(&units);
            }
            (0x85, _) => {
                let sc = data[i + 1] as usize;
                ensure!((2..=18).contains(&sc), "{}: file entry with {sc} secondary entries", at(i));
                let len = 32 * (1 + sc);
                ensure!(i + len <= data.len(), "{}: entry set runs past the end of the directory", at(i));
                let set = &data[i..i + len];
                ensure!(
                    checksum16(set, true) == le16(set, 2),
                    "{}: entry set checksum mismatch; run fsck.exfat",
                    at(i)
                );
                ensure!(set[32] == 0xC0, "{}: file entry is not followed by a stream extension", at(i));
                let name_len = set[35] as usize;
                let name_entries = name_len.div_ceil(15);
                ensure!(name_len > 0 && name_entries < sc, "{}: bad name length", at(i));
                let mut name = Vec::with_capacity(name_len);
                for k in 0..sc - 1 {
                    let e = &set[64 + 32 * k..96 + 32 * k];
                    if k < name_entries {
                        ensure!(e[0] == 0xC1, "{}: missing file name entry", at(i));
                        for j in 0..15 {
                            if name.len() < name_len {
                                name.push(le16(e, 2 + 2 * j));
                            }
                        }
                    } else {
                        ensure!(
                            e[0] & 0x80 == 0 || e[0] & 0x20 != 0,
                            "{}: unknown critical secondary entry {:#04x}",
                            at(i),
                            e[0]
                        );
                    }
                }
                let flags = set[33];
                let (is_dir, data_len, valid_len) = (le16(set, 4) & 0x10 != 0, le64(set, 56), le64(set, 40));
                // Every stream entry, before anyone branches on its type or skips it.
                let shown = || format!("{}/{}", path.trim_end_matches('/'), String::from_utf16_lossy(&name));
                if is_dir && lax && data_len == 0 && valid_len == 0 && le32(set, 52) == 0 {
                    // fsck.exfat's remains of a cross-linked twin: rekey checks there is one.
                } else if is_dir {
                    ensure!(
                        valid_len == data_len,
                        "directory {}: ValidDataLength {valid_len} differs from DataLength {data_len}; run fsck.exfat",
                        shown()
                    );
                    ensure!(
                        data_len > 0 && data_len.is_multiple_of(cs) && data_len <= MAX_DIR_BYTES,
                        "directory {}: DataLength {data_len} is not a non-zero multiple of the cluster size \
                         ({cs}) up to 256 MB; run fsck.exfat",
                        shown()
                    );
                } else {
                    ensure!(
                        valid_len <= data_len,
                        "{}: ValidDataLength {valid_len} exceeds DataLength {data_len}; run fsck.exfat",
                        shown()
                    );
                }
                kids.push(Child {
                    name,
                    is_dir,
                    first: le32(set, 52),
                    no_fat_chain: flags & 0x02 != 0,
                    len: data_len,
                    valid_len,
                    mtime: (le32(set, 12), set[21], set[23]),
                    set_off: i,
                    set_len: len,
                    attrs: le16(set, 4),
                });
                i += len;
                continue;
            }
            _ if benign => {
                // Benign primary (volume GUID, TexFAT padding, …): skip it and its secondaries.
                i += 32 * (1 + data[i + 1] as usize);
                continue;
            }
            _ => bail!("{}: unknown critical entry type {t:#04x}; refusing to modify this volume", at(i)),
        }
        i += 32;
    }
    Ok((data.len().min(i), kids, orphans))
}

/// Decodes a stored timestamp (DOS-style field, 10 ms increment, UTC offset byte) to UTC
/// (secs, nanos). An offset not marked valid is taken as UTC. None for impossible fields.
pub(crate) fn decode_ts(t: u32, tens_ms: u8, offset: u8) -> Option<(i64, u32)> {
    let (sec2, min, hour) = (t & 0x1F, (t >> 5) & 0x3F, (t >> 11) & 0x1F);
    let (day, month, year) = ((t >> 16) & 0x1F, (t >> 21) & 0x0F, 1980 + ((t >> 25) & 0x7F) as i64);
    if sec2 > 29 || min > 59 || hour > 23 || !(1..=31).contains(&day) || !(1..=12).contains(&month) || tens_ms > 199 {
        return None;
    }
    // days_from_civil (H. Hinnant)
    let (m, d) = (month as i64, day as i64);
    let y = if m <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let local = days * 86_400 + hour as i64 * 3600 + min as i64 * 60 + sec2 as i64 * 2 + (tens_ms / 100) as i64;
    let q = if offset & 0x80 != 0 { ((offset << 1) as i8 >> 1) as i64 } else { 0 };
    Some((local - q * 900, (tens_ms % 100) as u32 * 10_000_000))
}

/// The quick check shared by `sync` and `pull --update`: whether a local mtime (UTC secs,
/// nanos) and a stored exFAT LastModified timestamp (field, 10 ms increment, UTC offset byte)
/// name the same instant at exFAT's precision. The local time is first encoded the way
/// flashcp would store it (10 ms, clamped to 1980..=2107), so a file written by flashcp (or
/// pulled by it) always matches itself. A stored time whose UTC offset is not recorded (bit 7
/// clear: written by a driver that only knew local time) is accepted when it differs by a
/// whole number of quarter hours, at most 14 h, with the same sub-second part. An undecodable
/// stored timestamp never matches.
pub(crate) fn same_mtime(local: (i64, u32), stored: (u32, u8, u8)) -> bool {
    let (t, t10) = crate::exfat::ts(local.0, local.1);
    let (Some(a), Some(b)) = (decode_ts(t, t10, 0x80), decode_ts(stored.0, stored.1, stored.2)) else {
        return false;
    };
    if a == b {
        return true;
    }
    let d = a.0 - b.0;
    stored.2 & 0x80 == 0 && a.1 == b.1 && d % 900 == 0 && d.abs() <= 14 * 3600
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exfat::{Node, entry_set, finish_set};

    #[test]
    fn same_mtime_at_exfat_precision() {
        let (t, t10) = crate::exfat::ts(1_700_000_001, 123_456_789);
        // 10 ms precision: anything inside the same 10 ms matches, the next 10 ms does not.
        assert!(same_mtime((1_700_000_001, 129_999_999), (t, t10, 0x80)));
        assert!(!same_mtime((1_700_000_001, 130_000_000), (t, t10, 0x80)));
        assert!(!same_mtime((1_700_000_002, 123_456_789), (t, t10, 0x80)));
        // A recorded UTC offset is applied: local 10:00 at UTC+1 is 09:00 UTC.
        let (lt, lt10) = crate::exfat::ts(1_700_000_000 + 3600, 0);
        assert!(same_mtime((1_700_000_000, 0), (lt, lt10, 0x80 | 4)));
        assert!(!same_mtime((1_700_000_000 + 3600, 0), (lt, lt10, 0x80 | 4)));
        // No recorded offset: a whole-quarter-hour shift up to 14 h is the time zone.
        assert!(same_mtime((1_700_000_000, 0), (lt, lt10, 0)));
        assert!(!same_mtime((1_700_000_000 + 1, 0), (lt, lt10, 0)));
        let (ft, ft10) = crate::exfat::ts(1_700_000_000 + 15 * 3600, 0);
        assert!(!same_mtime((1_700_000_000, 0), (ft, ft10, 0)));
        // Before 1980 clamps to the exFAT epoch, as flashcp stores it; garbage never matches.
        let (et, et10) = crate::exfat::ts(0, 0);
        assert!(same_mtime((100, 5), (et, et10, 0x80)));
        assert!(!same_mtime((1_700_000_000, 0), (0, 0, 0x80)));
    }
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn mark_range_finds_the_first_overlap() {
        let mut set = cluster_set(64);
        assert_eq!(mark_range(&mut set, 5, 20), Ok(()));
        assert!(!mark(&mut set, 5) && !mark(&mut set, 24) && mark(&mut set, 25));
        assert_eq!(mark_range(&mut set, 30, 30), Ok(()));
        assert_eq!(mark_range(&mut set, 2, 64), Err(5));
        let mut set = cluster_set(64);
        assert!(mark(&mut set, 40));
        assert_eq!(mark_range(&mut set, 10, 50), Err(40));
        assert_eq!(mark_range(&mut set, 2, 0), Ok(()));
    }

    #[test]
    fn decode_ts_inverts_ts() {
        for (secs, ns) in
            [(315_532_800i64, 0u32), (1_709_164_799, 990_000_000), (1_700_000_001, 123_456_789), (4_354_819_199, 0)]
        {
            let (t, t10) = crate::exfat::ts(secs, ns);
            assert_eq!(decode_ts(t, t10, 0x80), Some((secs, ns / 10_000_000 * 10_000_000)));
        }
        let (t, t10) = crate::exfat::ts(1_700_000_000, 0);
        // UTC+1 (4 quarter hours) and UTC-5 (-20): the stored local time minus the offset.
        assert_eq!(decode_ts(t, t10, 0x80 | 4), Some((1_700_000_000 - 3600, 0)));
        assert_eq!(decode_ts(t, t10, 0x80 | (-20i8 as u8 & 0x7F)), Some((1_700_000_000 + 5 * 3600, 0)));
        assert_eq!(decode_ts(t, t10, 0), Some((1_700_000_000, 0)));
        assert_eq!(decode_ts(0, 0, 0x80), None);
        assert_eq!(decode_ts(t, 200, 0x80), None);
    }
    #[test]
    fn claim_refuses_free_and_cross_linked() {
        let bitmap = [0b0000_0011u8]; // clusters 2 and 3 allocated
        let mut refd = [0u8];
        let w = || "f".to_string();
        claim(&mut refd, &bitmap, 2, &w).unwrap();
        assert!(format!("{:#}", claim(&mut refd, &bitmap, 2, &w).unwrap_err()).contains("cross-linked"));
        assert!(format!("{:#}", claim(&mut refd, &bitmap, 4, &w).unwrap_err()).contains("marked free"));
    }

    #[test]
    fn contiguous_run_boundaries() {
        let cc = MAX_CLUSTER_COUNT;
        assert_eq!(contiguous_run(2, 1, 16).unwrap(), 2..3);
        assert_eq!(contiguous_run(2, 16, 16).unwrap(), 2..18);
        assert!(contiguous_run(2, 17, 16).is_err());
        assert!(contiguous_run(1, 1, 16).is_err());
        assert!(contiguous_run(0, 1, 16).is_err());
        assert!(contiguous_run(2, 0, 16).is_err());
        // Last legal cluster of the largest volume, and runs that would overflow u32.
        let last = (cc + 1) as u32;
        assert_eq!(contiguous_run(last, 1, cc).unwrap(), cc + 1..cc + 2);
        assert!(contiguous_run(last, 2, cc).is_err());
        assert!(contiguous_run(u32::MAX, 1, cc).is_err());
        assert!(contiguous_run(u32::MAX - 1, u32::MAX as u64, cc).is_err());
        assert!(contiguous_run(2, u64::MAX, cc).is_err());
    }

    /// A 32 MB image holding the mkfs.exfat reference boot region with ClusterCount patched.
    fn boot_image(tag: &str, cluster_count: u32) -> (PathBuf, File) {
        let mut b = include_bytes!("testdata/mkfs_boot.bin").to_vec();
        b[92..96].copy_from_slice(&cluster_count.to_le_bytes());
        let c = checksum32(&b[..11 * 512], &[106, 107, 112], 0);
        for i in 0..128 {
            b[11 * 512 + i * 4..11 * 512 + i * 4 + 4].copy_from_slice(&c.to_le_bytes());
        }
        let p = std::env::temp_dir().join(format!("flashcp-boot-{tag}-{}.img", std::process::id()));
        let f = File::options().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        f.set_len(32 << 20).unwrap();
        f.write_all_at(&b, 0).unwrap();
        (p, f)
    }

    #[test]
    fn cluster_count_over_spec_limit_is_refused() {
        for (tag, cc, refused) in [("max", 0xFFFF_FFF5u32, false), ("over", 0xFFFF_FFF6, true), ("zero", 0, true)] {
            let (p, f) = boot_image(tag, cc);
            let e = format!("{:#}", Vol::open(f, 32 << 20, false, true).err().expect("geometry should not fit 32 MB"));
            assert_eq!(e.contains("ClusterCount"), refused, "cc {cc:#x}: {e}");
            fs::remove_file(p).unwrap();
        }
    }

    #[test]
    fn fat_cache_stays_bounded() {
        // A fake 256 MB "FAT" at offset 0 whose entry i holds i ^ 0x5A5A5A5A.
        let p = std::env::temp_dir().join(format!("flashcp-fat-{}.img", std::process::id()));
        let f = File::options().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        let cc: u64 = 64 << 20;
        let mut chunk = Vec::with_capacity(4 << 20);
        for base in (0..cc).step_by(1 << 20) {
            chunk.clear();
            for i in base..base + (1 << 20) {
                chunk.extend_from_slice(&((i as u32) ^ 0x5A5A_5A5A).to_le_bytes());
            }
            f.write_all_at(&chunk, base * 4).unwrap();
        }
        let mut v = Vol {
            dev: f,
            dev_len: cc * 4,
            bps: 512,
            cs: 4096,
            fat_off: 0,
            heap_off: cc * 4,
            cc: cc - 2,
            flags: 0,
            sector0: vec![],
            vol_bytes: cc * 4,
            fat_cache: HashMap::new(),
            fat_order: Default::default(),
            kept: HashMap::new(),
            bitmap: vec![],
            bitmap_clusters: vec![],
            upcase_clusters: vec![],
            upc: vec![],
            label: String::new(),
            dirs: vec![],
            tolerate_dups: false,
        };
        // Stride through the whole 256 MB FAT (4096 distinct pages), then revisit.
        for round in 0..2 {
            for c in (2..cc - 2).step_by(16 << 10) {
                assert_eq!(v.fat(c as u32).unwrap(), (c as u32) ^ 0x5A5A_5A5A, "round {round} entry {c}");
                assert!(v.fat_cache.len() <= FAT_CACHE_PAGES);
            }
        }
        let bytes: usize = v.fat_cache.values().map(|p| p.len()).sum();
        assert!(bytes <= FAT_CACHE_PAGES * FAT_PAGE as usize, "cache holds {bytes} bytes");
        fs::remove_file(p).unwrap();
    }

    fn set(name: &str, is_dir: bool, first: u32, len: u64, nfc: bool) -> Vec<u8> {
        let upc = crate::exfat::upcase_map();
        let n = Node {
            name: name.encode_utf16().collect(),
            src: PathBuf::new(),
            is_dir,
            size: len,
            mtime: (1_700_000_000, 0),
            children: vec![],
            first_cluster: first,
            clusters: 0,
        };
        let mut e = entry_set(&n, &upc);
        if !nfc {
            e[33] = 0x01;
        }
        finish_set(e, len, len)
    }

    #[test]
    fn parse_dir_reads_sets_skips_deleted_and_benign() {
        let mut d = Vec::new();
        d.extend(set("a.txt", false, 10, 5, true));
        let mut deleted = set("gone", false, 11, 5, true);
        for k in 0..deleted.len() / 32 {
            deleted[k * 32] &= 0x7F;
        }
        d.extend(deleted);
        let mut guid = [0u8; 32];
        guid[0] = 0xA0;
        d.extend(guid);
        d.extend(set("Sub Dir", true, 12, 4096, false));
        let end = d.len();
        d.resize(4096, 0);
        let (e, kids, orphans) = parse_dir(&d, None, "/", 4096, false).unwrap();
        assert!(orphans.is_empty());
        assert_eq!(e, end);
        assert_eq!(kids.len(), 2);
        assert_eq!(String::from_utf16_lossy(&kids[1].name), "Sub Dir");
        assert!(kids[1].is_dir && !kids[1].no_fat_chain && kids[1].len == 4096 && kids[1].first == 12);
        assert!(kids[0].no_fat_chain && !kids[0].is_dir);
    }

    #[test]
    fn orphan_secondaries_after_a_deleted_file_entry_are_skipped_and_reported() {
        let mut d = Vec::new();
        let mut gone = set("a long name that needs two name entries", false, 10, 5, true);
        gone[0] &= 0x7F; // only the primary deleted: 3 orphans
        let sc = gone[1] as usize;
        d.extend(&gone);
        let mut half = set("half", false, 11, 5, true);
        half[0] &= 0x7F;
        half[32] &= 0x7F; // stream already cleared, name entry still in use: 1 orphan
        d.extend(&half);
        d.extend(set("live", false, 12, 5, true));
        let mut dead = set("dead", false, 13, 5, true);
        for k in 0..dead.len() / 32 {
            dead[32 * k] &= 0x7F;
        }
        d.extend(dead);
        d.resize(4096, 0);
        let (_, kids, orphans) = parse_dir(&d, None, "/", 4096, false).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(String::from_utf16_lossy(&kids[0].name), "live");
        let first: Vec<usize> = (1..=sc).map(|k| 32 * k).collect();
        assert_eq!(orphans[..sc], first[..]);
        assert_eq!(orphans[sc..], [gone.len() + 64]);
        // A deleted entry's SecondaryCount bounds the scan: a stray secondary past it is refused.
        let mut d = set("x", false, 10, 5, true);
        d[0] &= 0x7F;
        d.extend(&d.clone()[32..64]);
        d.resize(4096, 0);
        assert!(format!("{:#}", parse_dir(&d, None, "/", 4096, false).unwrap_err()).contains("outside an entry set"));
    }

    #[test]
    fn duplicates_are_refused_with_a_hint_unless_tolerated() {
        let upc = crate::exfat::upcase_map();
        let mut d = Vec::new();
        d.extend(set("Docs", true, 12, 4096, false));
        d.extend(set("x.txt", false, 20, 5, true));
        let mut zero = set("docs", true, 0, 4096, false);
        // What fsck.exfat -y makes of the second set sharing clusters: DataLength 0, no cluster.
        zero[32 + 8..32 + 16].fill(0);
        zero[32 + 20..32 + 32].fill(0);
        let c = checksum16(&zero, true);
        zero[2..4].copy_from_slice(&c.to_le_bytes());
        d.extend(zero);
        d.extend(set("X.TXT", false, 21, 5, true));
        d.resize(4096, 0);
        assert!(format!("{:#}", parse_dir(&d, None, "/", 4096, false).unwrap_err()).contains("DataLength 0"));
        let (_, kids, _) = parse_dir(&d, None, "/", 4096, true).unwrap();
        let e = format!("{:#}", rekey(kids.clone(), &upc, "/", false).unwrap_err());
        assert!(e.contains("flashcp sync --recover"), "{e}");
        let (m, dups) = rekey(kids, &upc, "/", true).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(dups.len(), 2);
        let key: Vec<u16> = "DOCS".encode_utf16().collect();
        assert_eq!(m[&key].len, 4096, "the directory with clusters is the one in the map");
        // A zero-length directory without a twin is refused even when tolerated.
        let mut d = Vec::new();
        let mut z = set("lonely", true, 0, 4096, false);
        z[32 + 8..32 + 16].fill(0);
        z[32 + 24..32 + 32].fill(0);
        let c = checksum16(&z, true);
        z[2..4].copy_from_slice(&c.to_le_bytes());
        d.extend(z);
        d.resize(4096, 0);
        let (_, kids, _) = parse_dir(&d, None, "/", 4096, true).unwrap();
        assert!(rekey(kids, &upc, "/", true).is_err());
    }

    #[test]
    fn parse_dir_refuses_bad_checksum_and_unknown_critical() {
        let mut d = set("a.txt", false, 10, 5, true);
        d[64 + 2] ^= 1;
        d.resize(4096, 0);
        assert!(format!("{:#}", parse_dir(&d, None, "/", 4096, false).unwrap_err()).contains("checksum"));
        let mut d = vec![0u8; 4096];
        d[0] = 0x86;
        assert!(format!("{:#}", parse_dir(&d, None, "/", 4096, false).unwrap_err()).contains("unknown critical"));
    }
}
