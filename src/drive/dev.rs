//! The device under the driver: every write and sync goes through [`Dev`], so tests can inject
//! a crash after any of them ([`CrashDev`]) and rebuild what a power cut would have left.
//! Metadata reads go through `Vol`'s own handle on the same file (coherent with these writes).

use crate::sim::trace::{self, Op as TOp};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

pub(crate) trait Dev: Send + Sync {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    /// The raw file, when large sequential writes may bypass this trait (O_DIRECT stream).
    fn file(&self) -> Option<&Arc<File>> {
        None
    }
}

pub(crate) struct FileDev(pub(crate) Arc<File>);

impl Dev for FileDev {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        trace::serial(TOp::Read, off, buf.len() as u64, || self.0.read_exact_at(buf, off))
    }
    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<()> {
        trace::serial(TOp::Write, off, buf.len() as u64, || self.0.write_all_at(buf, off))
    }
    fn sync(&self) -> io::Result<()> {
        trace::sync(|| self.0.sync_all())
    }
    fn file(&self) -> Option<&Arc<File>> {
        Some(&self.0)
    }
}

/// A device that fails every write and sync from op number `fail_at` on (as a killed process
/// or a yanked drive would stop issuing them), and remembers the bytes each write since the
/// last completed sync replaced, so [`CrashDev::crash`] can rebuild a durable state.
#[cfg(test)]
pub(crate) struct CrashDev {
    pub(crate) file: Arc<File>,
    pub(crate) st: std::sync::Mutex<CrashState>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct CrashState {
    pub(crate) ops: u64,
    pub(crate) fail_at: Option<u64>,
    pub(crate) failed: bool,
    /// (offset, bytes before) of each write since the last sync.
    pub(crate) unsynced: Vec<(u64, Vec<u8>)>,
}

/// What a crash does to the writes since the last sync.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Loss {
    /// All of them reached the medium.
    None,
    /// None of them did.
    All,
    /// A random subset of their 512-byte sectors did (seeded).
    Random(u64),
}

#[cfg(test)]
impl CrashDev {
    pub(crate) fn new(file: File, fail_at: Option<u64>) -> CrashDev {
        CrashDev { file: Arc::new(file), st: std::sync::Mutex::new(CrashState { fail_at, ..Default::default() }) }
    }
    fn step(&self) -> io::Result<std::sync::MutexGuard<'_, CrashState>> {
        let mut s = self.st.lock().unwrap();
        if s.failed || s.fail_at == Some(s.ops) {
            s.failed = true;
            return Err(io::Error::other("injected crash"));
        }
        s.ops += 1;
        Ok(s)
    }
    /// Device ops (writes and syncs) issued so far.
    pub(crate) fn ops(&self) -> u64 {
        self.st.lock().unwrap().ops
    }
    /// Undoes the unsynced writes `loss` says were lost. Each 512-byte sector ends in the
    /// state after some prefix of the writes that touched it (what a device that persists
    /// whole sectors in any order can leave): none of them, all of them, or a random prefix.
    pub(crate) fn crash(&self, loss: Loss) {
        let mut s = self.st.lock().unwrap();
        let ws = std::mem::take(&mut s.unsynced);
        // per sector: how many of the writes touching it survive
        let mut touching: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for (off, pre) in &ws {
            for sec in off / 512..(off + pre.len() as u64).div_ceil(512) {
                *touching.entry(sec).or_default() += 1;
            }
        }
        let mut rng = crate::util::Rng(match loss {
            Loss::Random(seed) => seed | 1,
            _ => 1,
        });
        let mut keep: std::collections::HashMap<u64, usize> = touching
            .iter()
            .map(|(&sec, &m)| {
                let k = match loss {
                    Loss::None => m,
                    Loss::All => 0,
                    Loss::Random(_) => (rng.next() % (m as u64 + 1)) as usize,
                };
                (sec, k)
            })
            .collect();
        // writes seen so far per sector (forward), to know which are past the kept prefix
        let mut seen: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        let mut lost: Vec<(u64, Vec<u8>)> = Vec::new();
        for (off, pre) in &ws {
            let mut i = 0usize;
            while i < pre.len() {
                let at = off + i as u64;
                let n = ((512 - at % 512) as usize).min(pre.len() - i);
                let sec = at / 512;
                let c = seen.entry(sec).or_default();
                *c += 1;
                if *c > keep[&sec] {
                    lost.push((at, pre[i..i + n].to_vec()));
                }
                i += n;
            }
        }
        for (at, pre) in lost.into_iter().rev() {
            self.file.write_all_at(&pre, at).unwrap();
        }
        keep.clear();
        self.file.sync_all().unwrap();
    }
}

#[cfg(test)]
impl Dev for CrashDev {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, off)
    }
    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<()> {
        let mut s = self.step()?;
        let mut pre = vec![0u8; buf.len()];
        let len = self.file.metadata()?.len();
        if off < len {
            let n = ((len - off) as usize).min(buf.len());
            self.file.read_exact_at(&mut pre[..n], off)?;
        }
        s.unsynced.push((off, pre));
        self.file.write_all_at(buf, off)
    }
    fn sync(&self) -> io::Result<()> {
        let mut s = self.step()?;
        s.unsynced.clear();
        Ok(())
    }
}
