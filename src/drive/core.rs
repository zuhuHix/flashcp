//! The driver's in-memory file system: the inode tree as applications see it (committed state
//! plus staged changes), the mirrors of what the volume holds (directory bytes, bitmap), and
//! every POSIX operation, all without I/O beyond reading the volume. [`super::commit`] turns
//! the difference between the two into crash-ordered device writes.

use super::set::{self, ATTR_ARCHIVE, ATTR_DIR, ATTR_RO, RawTs, SetFields};
use super::stage::{Overlay, Stager};
use crate::volume::{Child, Vol, parse_dir};
use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::FileExt;
use std::time::SystemTime;

pub type Ino = u64;
pub const ROOT: Ino = 1;
/// errno values.
pub type Errno = i32;
pub type R<T> = std::result::Result<T, Errno>;

/// Where an inode's entry set lives on the volume: in directory `dir`'s data at `off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Disk {
    pub(crate) dir: Ino,
    pub(crate) off: usize,
    pub(crate) len: usize,
}

/// The committed data of an object.
#[derive(Clone, Debug, Default)]
pub(crate) struct Layout {
    pub(crate) first: u32,
    pub(crate) nfc: bool,
    pub(crate) len: u64,
    pub(crate) valid: u64,
    /// Cluster runs (start, count), resolved from the FAT on first use.
    pub(crate) runs: Option<Vec<(u32, u32)>>,
}

/// The committed bytes of a loaded directory, exactly as on the volume.
#[derive(Clone, Debug)]
pub(crate) struct DirData {
    pub(crate) clusters: Vec<u32>,
    pub(crate) nfc: bool,
    pub(crate) data: Vec<u8>,
    /// Offset of the end-of-directory marker (data.len() when full).
    pub(crate) end: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Inode {
    pub(crate) parent: Ino,
    pub(crate) name: Vec<u16>,
    pub(crate) is_dir: bool,
    pub(crate) attrs: u16,
    pub(crate) ctime: RawTs,
    pub(crate) mtime: RawTs,
    pub(crate) atime: RawTs,
    pub(crate) disk: Option<Disk>,
    pub(crate) lay: Layout,
    /// Desired size (files).
    pub(crate) size: u64,
    /// Committed bytes [0, keep) are still content (bytes past it read as zeros unless staged).
    pub(crate) keep: u64,
    pub(crate) ov: Overlay,
    /// Children by up-cased name (directories, once loaded).
    pub(crate) kids: Option<HashMap<Vec<u16>, Ino>>,
    pub(crate) dd: Option<DirData>,
    pub(crate) nlookup: u64,
    pub(crate) opens: u32,
    /// In the namespace (false once unlinked; it may still be open).
    pub(crate) linked: bool,
    /// Clusters the next commit will allocate for it (an estimate, for ENOSPC and statfs).
    pub(crate) need: u64,
    /// Data changed since the last commit (an overlay, a truncation).
    pub(crate) data_dirty: bool,
    /// The smallest size set while a commit of it runs (u64::MAX: none): bytes past it of the
    /// commit's data are not content.
    pub(crate) cut: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub size: u64,
    pub blocks: u64,
    pub is_dir: bool,
    pub read_only: bool,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    pub ctime: SystemTime,
    pub crtime: SystemTime,
    pub blksize: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SetAttr {
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub atime: Option<SystemTime>,
    pub crtime: Option<SystemTime>,
    /// chmod: only the write bits matter (none set = read-only attribute).
    pub mode: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statfs {
    pub blocks: u64,
    pub bfree: u64,
    pub bsize: u32,
    pub files: u64,
    pub namelen: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenameFlags {
    pub noreplace: bool,
    pub exchange: bool,
}

pub(crate) struct Core {
    pub(crate) vol: Vol,
    /// The bitmap as on the volume.
    pub(crate) bm: Vec<u8>,
    /// Clusters in use or allocated by the commit in flight.
    pub(crate) used: u64,
    /// Sum of `Inode::need`.
    pub(crate) reserved: u64,
    pub(crate) inodes: HashMap<Ino, Inode>,
    next_ino: Ino,
    /// Inodes that differ from the volume.
    pub(crate) dirty: BTreeSet<Ino>,
    pub(crate) stager: Stager,
    pub(crate) seq: u64,
    /// Writes refused (prepare-eject, a failed commit, a read-only mount).
    pub(crate) read_only: Option<Errno>,
    /// The root's inode never has an entry set; its attributes are not stored.
    pub(crate) label: String,
    /// The drive journal's place in the root (hidden from listings), when it exists.
    pub(crate) journal: Option<super::djournal::Place>,
    /// Commits so far.
    pub(crate) commits: u64,
}

/// Clusters a bitmap marks used (padding bits past ClusterCount ignored).
pub(crate) fn used_clusters(bm: &[u8], cc: u64) -> u64 {
    let full = (cc / 8) as usize;
    let mut used: u64 = bm[..full].iter().map(|b| b.count_ones() as u64).sum();
    for i in full as u64 * 8..cc {
        used += (bm[(i / 8) as usize] >> (i % 8) & 1) as u64;
    }
    used
}

/// A part of a read.
#[derive(Clone)]
pub(crate) enum RSrc {
    Zero,
    Base(u64),
    Piece(super::stage::Piece, u64),
}

/// Executes a read plan into `buf` (sized to it): staged pieces, zeros, committed bytes
/// (through the readahead cache when there is one).
pub(crate) fn exec_read(
    dev: &std::fs::File,
    ra: Option<&super::readahead::Ra>,
    plan: &[(usize, usize, RSrc)],
    buf: &mut [u8],
) -> R<()> {
    for (o, l, src) in plan {
        let dst = &mut buf[*o..*o + *l];
        match src {
            RSrc::Zero => {}
            RSrc::Piece(p, po) => p.read(*po, dst).map_err(|_| libc::EIO)?,
            RSrc::Base(d) => {
                let mut pos = 0usize;
                while pos < dst.len() {
                    let at = d + pos as u64;
                    let (got, direct) = match ra {
                        Some(r) => r.read_some(at, &mut dst[pos..]),
                        None => (0, dst.len() - pos),
                    };
                    if got > 0 {
                        pos += got;
                        continue;
                    }
                    let n = direct.max(1).min(dst.len() - pos);
                    crate::sim::trace::serial(crate::sim::trace::Op::Read, at, n as u64, || {
                        dev.read_exact_at(&mut dst[pos..pos + n], at)
                    })
                    .map_err(|_| libc::EIO)?;
                    pos += n;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn now() -> RawTs {
    set::raw_ts(to_unix(SystemTime::now()))
}

pub(crate) fn to_unix(t: SystemTime) -> (i64, u32) {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            (
                -(d.as_secs() as i64) - i64::from(d.subsec_nanos() > 0),
                (1_000_000_000 - d.subsec_nanos()) % 1_000_000_000,
            )
        }
    }
}

fn to_system(r: RawTs) -> SystemTime {
    let (s, n) = set::unix_ts(r);
    if s >= 0 {
        SystemTime::UNIX_EPOCH + std::time::Duration::new(s as u64, n)
    } else {
        SystemTime::UNIX_EPOCH - std::time::Duration::new((-s) as u64, 0) + std::time::Duration::new(0, n)
    }
}

pub(crate) fn utf16(name: &std::ffi::OsStr) -> R<Vec<u16>> {
    Ok(name.to_str().ok_or(libc::EINVAL)?.encode_utf16().collect())
}

impl Core {
    pub(crate) fn new(mut vol: Vol, stager: Stager) -> anyhow::Result<Core> {
        let bm = std::mem::take(&mut vol.bitmap);
        let used = used_clusters(&bm, vol.cc);
        let root_dir = vol.dirs.pop().ok_or_else(|| anyhow::anyhow!("no root directory"))?;
        let label = vol.label.clone();
        let t = now();
        let root = Inode {
            parent: ROOT,
            name: Vec::new(),
            is_dir: true,
            attrs: ATTR_DIR,
            ctime: t,
            mtime: t,
            atime: t,
            disk: None,
            lay: Layout {
                first: root_dir.clusters[0],
                nfc: false,
                len: root_dir.data.len() as u64,
                valid: root_dir.data.len() as u64,
                runs: None,
            },
            size: 0,
            keep: 0,
            ov: Overlay::default(),
            kids: None,
            dd: None,
            nlookup: 1,
            opens: 0,
            linked: true,
            need: 0,
            data_dirty: false,
            cut: u64::MAX,
        };
        let mut c = Core {
            vol,
            bm,
            used,
            reserved: 0,
            inodes: HashMap::new(),
            next_ino: ROOT + 1,
            dirty: BTreeSet::new(),
            stager,
            seq: 0,
            read_only: None,
            label,
            journal: None,
            commits: 0,
        };
        c.inodes.insert(ROOT, root);
        let children: Vec<Child> = root_dir.children.into_values().collect();
        c.journal = super::djournal::find(&children);
        let dd = DirData { clusters: root_dir.clusters, nfc: false, data: root_dir.data, end: root_dir.end };
        c.attach_children(ROOT, dd, children);
        Ok(c)
    }

    pub(crate) fn ino(&self, i: Ino) -> R<&Inode> {
        self.inodes.get(&i).ok_or(libc::ENOENT)
    }
    pub(crate) fn ino_mut(&mut self, i: Ino) -> R<&mut Inode> {
        self.inodes.get_mut(&i).ok_or(libc::ENOENT)
    }

    fn attach_children(&mut self, dir: Ino, dd: DirData, children: Vec<Child>) {
        let mut kids = HashMap::with_capacity(children.len());
        for ch in children {
            if dir == ROOT && super::djournal::is_journal(&ch) {
                continue;
            }
            let f = set::decode(&dd.data[ch.set_off..ch.set_off + ch.set_len]);
            let ino = self.next_ino;
            self.next_ino += 1;
            let k = set::key(&ch.name, &self.vol.upc);
            self.inodes.insert(
                ino,
                Inode {
                    parent: dir,
                    name: ch.name.clone(),
                    is_dir: ch.is_dir,
                    attrs: f.attrs,
                    ctime: f.ctime,
                    mtime: f.mtime,
                    atime: f.atime,
                    disk: Some(Disk { dir, off: ch.set_off, len: ch.set_len }),
                    lay: Layout { first: ch.first, nfc: ch.no_fat_chain, len: ch.len, valid: ch.valid_len, runs: None },
                    size: if ch.is_dir { 0 } else { ch.len },
                    keep: if ch.is_dir { 0 } else { ch.valid_len },
                    ov: Overlay::default(),
                    kids: None,
                    dd: None,
                    nlookup: 0,
                    opens: 0,
                    linked: true,
                    need: 0,
                    data_dirty: false,
                    cut: u64::MAX,
                },
            );
            kids.insert(k, ino);
        }
        let d = self.inodes.get_mut(&dir).unwrap();
        d.kids = Some(kids);
        d.dd = Some(dd);
    }

    /// The cluster runs of an inode's committed data (resolving the FAT chain once).
    pub(crate) fn runs(&mut self, i: Ino) -> R<Vec<(u32, u32)>> {
        let n = self.ino(i)?;
        if let Some(r) = &n.lay.runs {
            return Ok(r.clone());
        }
        let (first, nfc, len) = (n.lay.first, n.lay.nfc, n.lay.len);
        let what = format!("inode {i}");
        let r: Vec<(u32, u32)> = self
            .vol
            .runs(first, nfc, len, &what)
            .map_err(|e| {
                eprintln!("flashcp mount: {e:#}");
                libc::EIO
            })?
            .into_iter()
            .map(|(s, n)| (s as u32, n as u32))
            .collect();
        self.ino_mut(i)?.lay.runs = Some(r.clone());
        Ok(r)
    }

    /// Loads a committed directory's entries (validated by the shared parser).
    pub(crate) fn load(&mut self, dir: Ino) -> R<()> {
        let n = self.ino(dir)?;
        if !n.is_dir {
            return Err(libc::ENOTDIR);
        }
        if n.kids.is_some() {
            return Ok(());
        }
        if n.disk.is_none() && dir != ROOT {
            // A new directory: nothing on the volume yet.
            let d = self.ino_mut(dir)?;
            d.kids = Some(HashMap::new());
            return Ok(());
        }
        let (first, nfc, len) = (n.lay.first, n.lay.nfc, n.lay.len);
        let path = self.path(dir);
        let res = (|| -> anyhow::Result<(DirData, Vec<Child>)> {
            anyhow::ensure!(
                len > 0 && len.is_multiple_of(self.vol.cs) && len <= crate::volume::MAX_DIR_BYTES,
                "directory {path} has invalid size {len}"
            );
            let clusters = self.vol.object_clusters(first, nfc, len, &path)?;
            let data = self.vol.read_clusters(&clusters, len)?;
            let (end, children, _orphans) = parse_dir(&data, None, &path, self.vol.cs, false)?;
            let (children, _) = crate::volume::rekey(children, &self.vol.upc, &path, false)?;
            Ok((DirData { clusters, nfc, data, end }, children.into_values().collect()))
        })();
        match res {
            Ok((dd, ch)) => {
                self.attach_children(dir, dd, ch);
                Ok(())
            }
            Err(e) => {
                eprintln!("flashcp mount: {e:#}");
                Err(libc::EIO)
            }
        }
    }

    pub(crate) fn path(&self, mut i: Ino) -> String {
        let mut parts = Vec::new();
        while i != ROOT {
            let Some(n) = self.inodes.get(&i) else { break };
            parts.push(String::from_utf16_lossy(&n.name));
            i = n.parent;
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    pub(crate) fn attr(&self, i: Ino) -> R<Attr> {
        let n = self.ino(i)?;
        let cs = self.vol.cs;
        let size = if n.is_dir { n.dd.as_ref().map_or(cs, |d| d.data.len() as u64) } else { n.size };
        Ok(Attr {
            ino: i,
            size,
            blocks: size.div_ceil(cs) * cs / 512,
            is_dir: n.is_dir,
            read_only: n.attrs & ATTR_RO != 0,
            mtime: to_system(n.mtime),
            atime: to_system(n.atime),
            ctime: to_system(n.mtime),
            crtime: to_system(n.ctime),
            blksize: cs.min(1 << 20) as u32,
        })
    }

    pub(crate) fn lookup(&mut self, parent: Ino, name: &[u16]) -> R<Ino> {
        self.load(parent)?;
        let k = set::key(name, &self.vol.upc);
        self.ino(parent)?.kids.as_ref().unwrap().get(&k).copied().ok_or(libc::ENOENT)
    }

    pub(crate) fn readdir(&mut self, dir: Ino) -> R<Vec<(Ino, Vec<u16>, bool)>> {
        self.load(dir)?;
        let n = self.ino(dir)?;
        let mut v: Vec<(Ino, Vec<u16>, bool)> = n
            .kids
            .as_ref()
            .unwrap()
            .values()
            .map(|&k| {
                let c = &self.inodes[&k];
                (k, c.name.clone(), c.is_dir)
            })
            .collect();
        // Stable order: the volume's own order for committed entries, new ones after.
        v.sort_by_key(|(k, _, _)| {
            let c = &self.inodes[k];
            (c.disk.map_or(usize::MAX, |d| if d.dir == dir { d.off } else { usize::MAX }), *k)
        });
        Ok(v)
    }

    fn writable(&self) -> R<()> {
        match self.read_only {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub(crate) fn mark(&mut self, i: Ino) {
        self.dirty.insert(i);
    }

    fn touch_dir(&mut self, d: Ino) {
        if d != ROOT {
            let t = now();
            if let Some(n) = self.inodes.get_mut(&d) {
                n.mtime = t;
            }
            self.mark(d);
        }
    }

    pub(crate) fn free_clusters(&self) -> u64 {
        self.vol.cc.saturating_sub(self.used).saturating_sub(self.reserved)
    }

    /// Updates an inode's cluster estimate; fails with ENOSPC (changing nothing) when the
    /// volume cannot hold it.
    pub(crate) fn set_need(&mut self, i: Ino, need: u64) -> R<()> {
        let old = self.ino(i)?.need;
        if need > old && need - old > self.free_clusters() {
            return Err(libc::ENOSPC);
        }
        self.reserved = self.reserved + need - old;
        self.ino_mut(i)?.need = need;
        Ok(())
    }

    pub(crate) fn create(&mut self, parent: Ino, name: &[u16], is_dir: bool) -> R<Ino> {
        self.writable()?;
        set::check_name(name)?;
        self.load(parent)?;
        let p = self.ino(parent)?;
        if !p.is_dir {
            return Err(libc::ENOTDIR);
        }
        if !p.linked {
            return Err(libc::ENOENT);
        }
        let k = set::key(name, &self.vol.upc);
        if p.kids.as_ref().unwrap().contains_key(&k) {
            return Err(libc::EEXIST);
        }
        if parent == ROOT && super::djournal::reserved_name(&k, &self.vol.upc) {
            return Err(libc::EINVAL);
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        let t = now();
        self.inodes.insert(
            ino,
            Inode {
                parent,
                name: name.to_vec(),
                is_dir,
                attrs: if is_dir { ATTR_DIR } else { ATTR_ARCHIVE },
                ctime: t,
                mtime: t,
                atime: t,
                disk: None,
                lay: Layout::default(),
                size: 0,
                keep: 0,
                ov: Overlay::default(),
                kids: if is_dir { Some(HashMap::new()) } else { None },
                dd: None,
                nlookup: 0,
                opens: 0,
                linked: true,
                need: 0,
                data_dirty: false,
                cut: u64::MAX,
            },
        );
        if is_dir && let Err(e) = self.set_need(ino, 1) {
            self.inodes.remove(&ino);
            return Err(e);
        }
        self.ino_mut(parent)?.kids.as_mut().unwrap().insert(k, ino);
        self.mark(ino);
        self.touch_dir(parent);
        Ok(ino)
    }

    pub(crate) fn write(&mut self, i: Ino, off: u64, data: &[u8]) -> R<()> {
        self.writable()?;
        let n = self.ino(i)?;
        if n.is_dir {
            return Err(libc::EISDIR);
        }
        let end = off.checked_add(data.len() as u64).ok_or(libc::EFBIG)?;
        if end > (1u64 << 44) {
            return Err(libc::EFBIG);
        }
        let new_size = n.size.max(end);
        let need = new_size.div_ceil(self.vol.cs);
        self.set_need(i, need)?;
        self.seq += 1;
        let seq = self.seq;
        let n = self.inodes.get_mut(&i).unwrap();
        self.stager.write(&mut n.ov, off, data, seq).map_err(|e| {
            eprintln!("flashcp mount: staging: {e}");
            libc::EIO
        })?;
        n.size = new_size;
        n.mtime = now();
        n.data_dirty = true;
        self.mark(i);
        Ok(())
    }

    pub(crate) fn truncate(&mut self, i: Ino, size: u64) -> R<()> {
        self.writable()?;
        let n = self.ino(i)?;
        if n.is_dir {
            return Err(libc::EISDIR);
        }
        if size > (1u64 << 44) {
            return Err(libc::EFBIG);
        }
        self.set_need(i, size.div_ceil(self.vol.cs))?;
        let n = self.inodes.get_mut(&i).unwrap();
        n.cut = n.cut.min(size);
        if size < n.size {
            let r = n.ov.punch(size, u64::MAX);
            self.stager.released(r);
            n.keep = n.keep.min(size);
        }
        n.size = size;
        n.mtime = now();
        n.data_dirty = true;
        self.mark(i);
        Ok(())
    }

    pub(crate) fn setattr(&mut self, i: Ino, a: &SetAttr) -> R<Attr> {
        if let Some(s) = a.size {
            self.truncate(i, s)?;
        }
        if a.mtime.is_some() || a.atime.is_some() || a.crtime.is_some() || a.mode.is_some() {
            self.writable()?;
            let n = self.ino_mut(i)?;
            if let Some(t) = a.mtime {
                n.mtime = set::raw_ts(to_unix(t));
            }
            if let Some(t) = a.atime {
                n.atime = set::raw_ts(to_unix(t));
            }
            if let Some(t) = a.crtime {
                n.ctime = set::raw_ts(to_unix(t));
            }
            if let Some(m) = a.mode {
                if m & 0o222 == 0 {
                    n.attrs |= ATTR_RO;
                } else {
                    n.attrs &= !ATTR_RO;
                }
            }
            if i != ROOT {
                self.mark(i);
            }
        }
        self.attr(i)
    }

    /// What a read of [off, off + len) consists of: (offset in the result, length, source).
    pub(crate) fn read_plan(&mut self, i: Ino, off: u64, len: u32) -> R<Vec<(usize, usize, RSrc)>> {
        let n = self.ino(i)?;
        if n.is_dir {
            return Err(libc::EISDIR);
        }
        if off >= n.size {
            return Ok(Vec::new());
        }
        let end = n.size.min(off + len as u64);
        let keep = n.keep.min(end);
        let runs = if keep > off { self.runs(i)? } else { Vec::new() };
        let cs = self.vol.cs;
        let n = &self.inodes[&i];
        let mut out: Vec<(usize, usize, RSrc)> = Vec::new();
        let gap = |out: &mut Vec<(usize, usize, RSrc)>, a: u64, b: u64| {
            let mut x = a;
            while x < b {
                if x < keep {
                    let mut logical = 0u64;
                    let mut placed = false;
                    for &(s, cnt) in &runs {
                        let rl = cnt as u64 * cs;
                        if x >= logical && x < logical + rl {
                            let take = (logical + rl).min(b).min(keep) - x;
                            out.push((
                                (x - off) as usize,
                                take as usize,
                                RSrc::Base(self.vol.cluster_off(s) + (x - logical)),
                            ));
                            x += take;
                            placed = true;
                            break;
                        }
                        logical += rl;
                    }
                    if !placed {
                        return Err(libc::EIO);
                    }
                } else {
                    out.push(((x - off) as usize, (b - x) as usize, RSrc::Zero));
                    x = b;
                }
            }
            Ok(())
        };
        let mut at = off;
        for (po, p) in n.ov.range(off, end) {
            let (a, b) = (po.max(off), (po + p.len).min(end));
            if a > at {
                gap(&mut out, at, a)?;
            }
            out.push(((a - off) as usize, (b - a) as usize, RSrc::Piece(p.clone(), a - po)));
            at = b;
        }
        if at < end {
            gap(&mut out, at, end)?;
        }
        Ok(out)
    }

    /// Reads committed + staged content (directly, without readahead).
    pub(crate) fn read(&mut self, i: Ino, off: u64, len: u32) -> R<Vec<u8>> {
        let plan = self.read_plan(i, off, len)?;
        let total = plan.last().map_or(0, |p| p.0 + p.1);
        let mut buf = vec![0u8; total];
        exec_read(&self.vol.dev, None, &plan, &mut buf)?;
        Ok(buf)
    }

    fn is_ancestor(&self, a: Ino, mut b: Ino) -> bool {
        loop {
            if a == b {
                return true;
            }
            if b == ROOT {
                return false;
            }
            match self.inodes.get(&b) {
                Some(n) => b = n.parent,
                None => return false,
            }
        }
    }

    /// Takes `i` out of the namespace (it stays while open, and until its set is removed).
    fn detach(&mut self, i: Ino) {
        let n = self.inodes.get_mut(&i).unwrap();
        n.linked = false;
        self.mark(i);
    }

    pub(crate) fn unlink(&mut self, parent: Ino, name: &[u16], dir: bool) -> R<()> {
        self.writable()?;
        let i = self.lookup(parent, name)?;
        let n = self.ino(i)?;
        if dir && !n.is_dir {
            return Err(libc::ENOTDIR);
        }
        if !dir && n.is_dir {
            return Err(libc::EISDIR);
        }
        if dir {
            self.load(i)?;
            if !self.inodes[&i].kids.as_ref().unwrap().is_empty() {
                return Err(libc::ENOTEMPTY);
            }
        }
        let k = set::key(name, &self.vol.upc);
        self.ino_mut(parent)?.kids.as_mut().unwrap().remove(&k);
        self.detach(i);
        self.touch_dir(parent);
        Ok(())
    }

    pub(crate) fn rename(&mut self, p1: Ino, n1: &[u16], p2: Ino, n2: &[u16], fl: RenameFlags) -> R<()> {
        self.writable()?;
        set::check_name(n2)?;
        let src = self.lookup(p1, n1)?;
        self.load(p2)?;
        if !self.ino(p2)?.is_dir {
            return Err(libc::ENOTDIR);
        }
        if p2 == ROOT && super::djournal::reserved_name(&set::key(n2, &self.vol.upc), &self.vol.upc) {
            return Err(libc::EINVAL);
        }
        let src_dir = self.ino(src)?.is_dir;
        if src_dir && self.is_ancestor(src, p2) {
            return Err(libc::EINVAL);
        }
        let k1 = set::key(n1, &self.vol.upc);
        let k2 = set::key(n2, &self.vol.upc);
        let dst = self.ino(p2)?.kids.as_ref().unwrap().get(&k2).copied();
        if fl.exchange {
            let dst = dst.ok_or(libc::ENOENT)?;
            if dst == src {
                return Ok(());
            }
            let dst_dir = self.ino(dst)?.is_dir;
            if dst_dir && self.is_ancestor(dst, p1) {
                return Err(libc::EINVAL);
            }
            let (name1, name2) = (self.ino(src)?.name.clone(), self.ino(dst)?.name.clone());
            self.ino_mut(p1)?.kids.as_mut().unwrap().insert(k1, dst);
            self.ino_mut(p2)?.kids.as_mut().unwrap().insert(k2, src);
            let s = self.ino_mut(src)?;
            s.parent = p2;
            s.name = name2;
            let d = self.ino_mut(dst)?;
            d.parent = p1;
            d.name = name1;
            self.mark(src);
            self.mark(dst);
            self.touch_dir(p1);
            self.touch_dir(p2);
            return Ok(());
        }
        if let Some(d) = dst {
            if d == src {
                // Same object (a case-only rename, or the same name).
                if self.ino(src)?.name != n2 {
                    self.ino_mut(src)?.name = n2.to_vec();
                    self.mark(src);
                    self.touch_dir(p1);
                }
                return Ok(());
            }
            if fl.noreplace {
                return Err(libc::EEXIST);
            }
            let dn = self.ino(d)?;
            match (src_dir, dn.is_dir) {
                (true, false) => return Err(libc::ENOTDIR),
                (false, true) => return Err(libc::EISDIR),
                (true, true) => {
                    self.load(d)?;
                    if !self.inodes[&d].kids.as_ref().unwrap().is_empty() {
                        return Err(libc::ENOTEMPTY);
                    }
                }
                _ => {}
            }
            self.ino_mut(p2)?.kids.as_mut().unwrap().remove(&k2);
            self.detach(d);
        }
        self.ino_mut(p1)?.kids.as_mut().unwrap().remove(&k1);
        self.ino_mut(p2)?.kids.as_mut().unwrap().insert(k2, src);
        let s = self.ino_mut(src)?;
        s.parent = p2;
        s.name = n2.to_vec();
        self.mark(src);
        self.touch_dir(p1);
        if p2 != p1 {
            self.touch_dir(p2);
        }
        Ok(())
    }

    pub(crate) fn statfs(&self) -> Statfs {
        Statfs {
            blocks: self.vol.cc,
            bfree: self.free_clusters(),
            bsize: self.vol.cs as u32,
            files: self.inodes.len() as u64,
            namelen: 255,
        }
    }

    /// Drops an inode nobody can reach any more (unlinked, closed, forgotten, off the volume).
    pub(crate) fn gc(&mut self, i: Ino) {
        if i == ROOT {
            return;
        }
        let Some(n) = self.inodes.get(&i) else { return };
        if !n.linked && n.opens == 0 && n.nlookup == 0 && n.disk.is_none() && !self.dirty.contains(&i) {
            let n = self.inodes.remove(&i).unwrap();
            self.reserved -= n.need;
            let mut ov = n.ov;
            let r = ov.punch(0, u64::MAX);
            self.stager.released(r);
        }
    }

    /// The fields the entry set of `i` should hold now, for a layout (first, nfc, len, valid).
    pub(crate) fn fields(&self, i: Ino, first: u32, nfc: bool, len: u64, valid: u64) -> SetFields {
        let n = &self.inodes[&i];
        SetFields {
            name: n.name.clone(),
            attrs: n.attrs,
            first,
            nfc,
            len,
            valid,
            ctime: n.ctime,
            mtime: n.mtime,
            atime: n.atime,
        }
    }
}
