//! Staged (written but not yet committed) file data: per file an [`Overlay`] of non-overlapping
//! pieces over the committed content, each piece a slice of an immutable store in RAM or in
//! the spill file (`$XDG_CACHE_HOME/flashcp/stage-<pid>`, unlinked at once). A commit takes
//! `Arc`s of the pieces it writes, so later writes never change what it is writing.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

/// RAM stores grow by appends up to this size.
const APPEND_MAX: usize = 4 << 20;

pub(crate) enum Store {
    Mem(Vec<u8>),
    Spill { file: Arc<File>, off: u64 },
}

#[derive(Clone)]
pub(crate) struct Piece {
    pub(crate) store: Arc<Store>,
    /// Offset of the piece's first byte in the store.
    pub(crate) at: u64,
    pub(crate) len: u64,
    /// The write sequence number of the newest byte (a commit takes pieces up to its number).
    pub(crate) seq: u64,
}

impl Piece {
    /// Copies `out.len()` bytes from offset `off` inside the piece.
    pub(crate) fn read(&self, off: u64, out: &mut [u8]) -> io::Result<()> {
        debug_assert!(off + out.len() as u64 <= self.len);
        match &*self.store {
            Store::Mem(v) => {
                let s = (self.at + off) as usize;
                out.copy_from_slice(&v[s..s + out.len()]);
                Ok(())
            }
            Store::Spill { file, off: base } => file.read_exact_at(out, base + self.at + off),
        }
    }
    fn is_spill(&self) -> bool {
        matches!(*self.store, Store::Spill { .. })
    }
}

/// Non-overlapping pieces by file offset.
#[derive(Clone, Default)]
pub(crate) struct Overlay(pub(crate) BTreeMap<u64, Piece>);

impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Overlay({} pieces, {} bytes)", self.0.len(), self.bytes())
    }
}

impl Overlay {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.0.values().map(|p| p.len).sum()
    }
    /// Highest file offset covered + 1 (0 when empty).
    pub(crate) fn end(&self) -> u64 {
        self.0.iter().next_back().map_or(0, |(o, p)| o + p.len)
    }
    /// Removes whatever covers [a, b): pieces are trimmed or split. Returns bytes removed
    /// and how many of them were in the spill file.
    pub(crate) fn punch(&mut self, a: u64, b: u64) -> (u64, u64) {
        if a >= b {
            return (0, 0);
        }
        let mut removed = (0u64, 0u64);
        let keys: Vec<u64> = self.0.range(..b).rev().take_while(|(o, p)| *o + p.len > a).map(|(o, _)| *o).collect();
        for o in keys {
            let p = self.0.remove(&o).unwrap();
            let (pe, cut_a, cut_b) = (o + p.len, a.max(o), b.min(o + p.len));
            let gone = cut_b - cut_a;
            removed.0 += gone;
            if p.is_spill() {
                removed.1 += gone;
            }
            if o < cut_a {
                self.0.insert(o, Piece { len: cut_a - o, ..p.clone() });
            }
            if cut_b < pe {
                self.0.insert(cut_b, Piece { at: p.at + (cut_b - o), len: pe - cut_b, ..p });
            }
        }
        removed
    }
    /// Pieces intersecting [a, b), as (file offset, piece), ascending.
    pub(crate) fn range(&self, a: u64, b: u64) -> impl Iterator<Item = (u64, &Piece)> {
        let start = self.0.range(..=a).next_back().filter(|(o, p)| *o + p.len > a).map(|(o, _)| *o).unwrap_or(a);
        self.0.range(start..b).map(|(o, p)| (*o, p)).filter(move |(o, p)| *o < b && o + p.len > a)
    }
    /// Drops the pieces a commit wrote: every piece whose seq ≤ `seq`. Returns bytes removed
    /// (total, spill).
    pub(crate) fn drop_through(&mut self, seq: u64) -> (u64, u64) {
        let mut r = (0, 0);
        self.0.retain(|_, p| {
            if p.seq <= seq {
                r.0 += p.len;
                if p.is_spill() {
                    r.1 += p.len;
                }
                false
            } else {
                true
            }
        });
        r
    }
}

/// Where new pieces are stored, and the counts the write window is enforced on.
pub(crate) struct Stager {
    pub(crate) ram_cap: u64,
    pub(crate) staged: u64,
    pub(crate) spilled: u64,
    spill: Option<(Arc<File>, u64)>,
    dir: Option<std::path::PathBuf>,
}

impl Stager {
    pub(crate) fn new(ram_cap: u64, dir: Option<std::path::PathBuf>) -> Stager {
        Stager { ram_cap, staged: 0, spilled: 0, spill: None, dir }
    }

    fn spill_file(&mut self) -> io::Result<&mut (Arc<File>, u64)> {
        if self.spill.is_none() {
            let dir = match &self.dir {
                Some(d) => d.clone(),
                None => {
                    let base = std::env::var_os("XDG_CACHE_HOME")
                        .filter(|v| !v.is_empty())
                        .map(std::path::PathBuf::from)
                        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
                        .ok_or_else(|| io::Error::other("no HOME for the spill file"))?;
                    base.join("flashcp")
                }
            };
            std::fs::create_dir_all(&dir)?;
            let p = dir.join(format!("stage-{}-{:x}", std::process::id(), rand_u64()));
            use std::os::unix::fs::OpenOptionsExt;
            let f = File::options().read(true).write(true).create_new(true).mode(0o600).open(&p)?;
            let _ = std::fs::remove_file(&p);
            self.spill = Some((Arc::new(f), 0));
        }
        Ok(self.spill.as_mut().unwrap())
    }

    /// Writes `data` at file offset `off` into `ov`, as piece number `seq`.
    pub(crate) fn write(&mut self, ov: &mut Overlay, off: u64, data: &[u8], seq: u64) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let n = data.len() as u64;
        let (gone, gone_spill) = ov.punch(off, off + n);
        self.staged -= gone;
        self.spilled -= gone_spill;
        // Append to the last RAM store of this file when nothing else holds it.
        if let Some((&o, p)) = ov.0.range_mut(..off).next_back()
            && o + p.len == off
            && let Some(Store::Mem(v)) = Arc::get_mut(&mut p.store)
            && v.len() as u64 == p.at + p.len
            && v.len() + data.len() <= APPEND_MAX
            && self.staged < self.ram_cap
        {
            v.extend_from_slice(data);
            p.len += n;
            p.seq = seq;
            self.staged += n;
            return Ok(());
        }
        let store = if self.staged - self.spilled < self.ram_cap {
            let mut v = Vec::with_capacity(data.len().max(64 << 10).min(APPEND_MAX));
            v.extend_from_slice(data);
            Store::Mem(v)
        } else {
            let (f, end) = self.spill_file()?;
            f.write_all_at(data, *end)?;
            let s = Store::Spill { file: f.clone(), off: *end };
            *end += n;
            self.spilled += n;
            s
        };
        ov.0.insert(off, Piece { store: Arc::new(store), at: 0, len: n, seq });
        self.staged += n;
        Ok(())
    }

    /// Accounts for pieces removed from an overlay (bytes, spill bytes).
    pub(crate) fn released(&mut self, r: (u64, u64)) {
        self.staged -= r.0;
        self.spilled -= r.1;
        if self.spilled == 0
            && let Some((f, end)) = self.spill.as_mut()
            && Arc::strong_count(f) == 1
        {
            let _ = f.set_len(0);
            *end = 0;
        }
    }
}

pub(crate) fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    if File::open("/dev/urandom").and_then(|f| f.read_exact_at(&mut b, 0)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        return t.as_nanos() as u64 ^ std::process::id() as u64;
    }
    u64::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(ov: &Overlay, len: u64) -> Vec<Option<u8>> {
        let mut out = vec![None; len as usize];
        for (o, p) in ov.range(0, len) {
            let mut b = vec![0u8; p.len as usize];
            p.read(0, &mut b).unwrap();
            for (i, x) in b.into_iter().enumerate() {
                out[o as usize + i] = Some(x);
            }
        }
        out
    }

    #[test]
    fn writes_split_trim_and_append() {
        let mut st = Stager::new(1 << 30, None);
        let mut ov = Overlay::default();
        st.write(&mut ov, 0, &[1; 10], 1).unwrap();
        st.write(&mut ov, 10, &[2; 5], 2).unwrap(); // appended to the same store
        assert_eq!(ov.0.len(), 1);
        st.write(&mut ov, 3, &[3; 4], 3).unwrap(); // splits
        assert_eq!(ov.0.len(), 3);
        let c = content(&ov, 16);
        let want: Vec<Option<u8>> =
            [1, 1, 1, 3, 3, 3, 3, 1, 1, 1, 2, 2, 2, 2, 2].iter().map(|&x| Some(x)).chain([None]).collect();
        assert_eq!(c, want);
        assert_eq!(st.staged, 15);
        let r = ov.punch(5, 12);
        st.released(r);
        assert_eq!(st.staged, 8);
        assert_eq!(content(&ov, 15)[4..13], [Some(3), None, None, None, None, None, None, None, Some(2)]);
        let r = ov.drop_through(2);
        st.released(r);
        assert_eq!(ov.bytes(), 2);
        assert_eq!(st.staged, 2);
    }

    #[test]
    fn spill_past_the_ram_cap() {
        let d = std::env::temp_dir().join(format!("fcp-spill-{}", std::process::id()));
        let mut st = Stager::new(8, Some(d.clone()));
        let mut ov = Overlay::default();
        st.write(&mut ov, 0, &[7; 8], 1).unwrap();
        st.write(&mut ov, 100, &[9; 8], 2).unwrap();
        assert_eq!(st.spilled, 8);
        assert_eq!(content(&ov, 108)[100], Some(9));
        let r = ov.drop_through(2);
        st.released(r);
        assert_eq!((st.staged, st.spilled), (0, 0));
        let _ = std::fs::remove_dir_all(d);
    }
}
