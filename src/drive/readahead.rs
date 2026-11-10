//! Physical-order readahead of committed data, with `pull`'s per-transport strategy.
//!
//! Readers of committed data go through [`Ra::read`]: a hit in the cache is copied, a chunk
//! being read is waited for, anything else is read directly. Prefetches are planned as device
//! extents, coalesced with pull's planner (reading through gaps only up to the strategy's gap,
//! which on a BOT stick comes from its measured request overhead × bandwidth), and read by
//! `inflight` worker threads in order into a bounded cache. Every chunk is dropped once all
//! its bytes were served (files are mostly read once), the oldest first when the cache is
//! full, and every chunk overlapping a range a commit is about to write is invalidated (a
//! read in flight then is discarded on completion: the generation changed).

use crate::pull::{Ext, Strategy};
use crate::util::AlignedBuf;
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const ALIGN: u64 = 4096;

struct Chunk {
    /// Counted in `St::bytes` (popped by a worker).
    counted: bool,
    start: u64,
    len: usize,
    /// Bytes of it some reader still wants (prefetch payload).
    want: u64,
    served: u64,
    data: Option<Arc<AlignedBuf>>,
    failed: bool,
    born: u64,
    /// Unique per queued chunk: a read completes only the very chunk it reserved (a commit may
    /// have invalidated it and a new prefetch queued another at the same offset meanwhile).
    token: u64,
}

#[derive(Default)]
struct St {
    chunks: BTreeMap<u64, Chunk>,
    order: VecDeque<u64>,
    queue: VecDeque<(u64, usize, u64)>,
    bytes: u64,
    next_token: u64,
    epoch: u64,
    stop: bool,
    last_hit: Option<Instant>,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    /// Queued chunks a reader read itself before any worker took them.
    pub(crate) claimed: u64,
    /// Bytes read into the cache.
    pub(crate) read_bytes: u64,
}

pub(crate) struct Ra {
    file: Arc<File>,
    pub(crate) strat: Strategy,
    cap: u64,
    /// Trace group of the worker reads (they run `inflight` deep among themselves).
    grp: u32,
    st: Mutex<St>,
    /// Test hook: the next read waits at these two barriers before storing its chunk.
    #[cfg(test)]
    pub(crate) pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    /// Workers wait on `work` (queued chunks, room in the cache); readers on `ready` (a
    /// chunk they need was stored).
    work: Condvar,
    ready: Condvar,
}

impl Ra {
    pub(crate) fn new(file: Arc<File>, strat: Strategy, cap: u64) -> Arc<Ra> {
        let ra = Arc::new(Ra {
            file,
            strat,
            cap,
            grp: crate::sim::trace::group(),
            st: Mutex::new(St::default()),
            #[cfg(test)]
            pause: Mutex::new(None),
            work: Condvar::new(),
            ready: Condvar::new(),
        });
        for _ in 0..ra.strat.inflight.max(1) {
            let r = ra.clone();
            std::thread::spawn(move || r.worker());
        }
        ra
    }

    pub(crate) fn stop(&self) {
        self.st.lock().unwrap().stop = true;
        self.work.notify_all();
        self.ready.notify_all();
    }

    fn worker(&self) {
        loop {
            let (start, len, epoch, token) = {
                let mut st = self.st.lock().unwrap();
                loop {
                    if st.stop {
                        return;
                    }
                    // abandoned prefetches: nobody read from the cache for 3 s
                    if st.last_hit.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) && !st.queue.is_empty() {
                        let q: Vec<_> = st.queue.drain(..).collect();
                        for (s, _, _) in q {
                            st.chunks.remove(&s);
                        }
                        st.last_hit = None;
                    }
                    if st.bytes <= self.cap
                        && let Some(j) = st.queue.pop_front()
                    {
                        if let Some(c) = st.chunks.get_mut(&j.0) {
                            c.counted = true;
                            let token = c.token;
                            st.bytes += j.1 as u64;
                            break (j.0, j.1, j.2, token);
                        }
                        continue;
                    }
                    st = self.work.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
                }
            };
            self.fill(start, len, epoch, token);
        }
    }

    /// Reads a chunk a worker or a reader took (counted, out of the queue) and stores it,
    /// unless a commit invalidated it meanwhile (the epoch moved): then it is dropped.
    fn fill(&self, start: u64, len: usize, epoch: u64, token: u64) {
        let mut buf = AlignedBuf::new(len);
        let ok = crate::sim::trace::io(
            crate::sim::trace::Op::Read,
            start,
            len as u64,
            self.strat.inflight,
            self.grp,
            start,
            || read_full(&self.file, buf.as_mut(), start),
        );
        #[cfg(test)]
        if let Some((a, b)) = self.pause.lock().unwrap().take() {
            a.wait();
            b.wait();
        }
        let mut st = self.st.lock().unwrap();
        st.read_bytes += len as u64;
        let stale = st.epoch != epoch;
        // only the chunk this read reserved; if it is gone (invalidated, replaced), whoever
        // removed it already gave its bytes back
        if let Some(c) = st.chunks.get_mut(&start).filter(|c| c.token == token) {
            if stale || c.born != epoch {
                let l = c.len as u64;
                st.chunks.remove(&start);
                unbook(&mut st, l); // counted: it was taken
            } else {
                match ok {
                    Ok(n) if n >= len.min(1) => c.data = Some(Arc::new(buf)),
                    _ => c.failed = true,
                }
            }
        }
        // readers waiting for a chunk; none of the workers needs this
        self.ready.notify_all();
    }

    /// Queues reads of `exts` (device offset, length), ascending as given (the order readers
    /// will want them), coalesced with the strategy. Ranges already cached are skipped.
    pub(crate) fn prefetch(&self, ranges: &[(u64, u64)]) {
        if ranges.is_empty() {
            return;
        }
        let max = self.strat.chunk.max(64 << 10);
        let mut exts: Vec<Ext> = Vec::new();
        for &(off, len) in ranges {
            let mut a = off;
            let end = off + len;
            while a < end {
                let n = (end - a).min(max as u64 - 2 * ALIGN);
                exts.push(Ext { phys: a, foff: 0, len: n as u32, file: 0 });
                a += n;
            }
        }
        exts.sort_by_key(|e| e.phys);
        let plan = crate::pull::plan_reads(&exts, max, self.strat.gap);
        let mut st = self.st.lock().unwrap();
        let epoch = st.epoch;
        for ch in plan {
            let payload: u64 = exts[ch.exts.clone()].iter().map(|e| e.len as u64).sum();
            // a chunk queued or being read over the same bytes stays (no double read); ready or
            // failed leftovers (a reader that skipped files, an earlier pass) are replaced
            let end = ch.start + ch.len as u64;
            let over: Vec<u64> =
                st.chunks.range(..end).filter(|(s, c)| **s + c.len as u64 > ch.start).map(|(s, _)| *s).collect();
            if over.iter().any(|s| st.chunks[s].data.is_none() && !st.chunks[s].failed) {
                continue;
            }
            for o in over {
                let c = st.chunks.remove(&o).unwrap();
                if c.counted {
                    unbook(&mut st, c.len as u64);
                }
            }
            st.next_token += 1;
            let token = st.next_token;
            st.chunks.insert(
                ch.start,
                Chunk {
                    counted: false,
                    start: ch.start,
                    len: ch.len,
                    want: payload,
                    served: 0,
                    data: None,
                    failed: false,
                    born: epoch,
                    token,
                },
            );
            st.order.push_back(ch.start);
            st.queue.push_back((ch.start, ch.len, epoch));
        }
        st.last_hit = Some(Instant::now()); // a fresh queue is not an abandoned one
        drop(st);
        self.evict();
        self.work.notify_all();
        self.ready.notify_all();
    }

    fn evict(&self) {
        let mut st = self.st.lock().unwrap();
        while st.bytes > self.cap * 2 {
            let Some(s) = st.order.pop_front() else { break };
            if let Some(c) = st.chunks.get(&s)
                && c.data.is_some()
            {
                let l = c.len as u64;
                st.chunks.remove(&s);
                unbook(&mut st, l); // ready chunks are counted
            } else if st.chunks.contains_key(&s) {
                st.order.push_back(s);
                break;
            }
        }
    }

    /// Copies device bytes from `off` on into `out` from the cache, waiting for a chunk that
    /// is queued or being read. Returns the bytes copied (from `off`, at most `out.len()`),
    /// or 0 and how many bytes from `off` the caller should read itself (up to the next
    /// cached chunk).
    pub(crate) fn read_some(&self, off: u64, out: &mut [u8]) -> (usize, usize) {
        let mut st = self.st.lock().unwrap();
        loop {
            let cur = st.chunks.range(..=off).next_back().filter(|(s, c)| **s + c.len as u64 > off).map(|(s, _)| *s);
            let Some(s) = cur else {
                st.misses += 1;
                let next = st.chunks.range(off + 1..).next().map(|(s, _)| *s);
                let n = next.map_or(out.len() as u64, |n| (n - off).min(out.len() as u64));
                return (0, n as usize);
            };
            let c = st.chunks.get(&s).unwrap();
            if c.failed {
                let n = (s + c.len as u64 - off).min(out.len() as u64) as usize;
                return (0, n);
            }
            if let Some(d) = c.data.clone() {
                let a = (off - c.start) as usize;
                let n = (c.len - a).min(out.len());
                out[..n].copy_from_slice(&AlignedBuf::as_ref(&d)[a..a + n]);
                let c = st.chunks.get_mut(&s).unwrap();
                c.served += n as u64;
                if c.served >= c.want {
                    let l = c.len as u64;
                    st.chunks.remove(&s);
                    unbook(&mut st, l);
                }
                st.hits += 1;
                st.last_hit = Some(Instant::now());
                // room in the cache for one more chunk: one worker, not all of them
                self.work.notify_one();
                return (n, 0);
            }
            if !c.counted {
                // queued, and no worker has taken it: the reader takes it itself (reading it
                // once, where waiting would stall and reading around it would read it twice)
                let (len, token) = (c.len, c.token);
                st.queue.retain(|j| j.0 != s);
                st.chunks.get_mut(&s).unwrap().counted = true;
                st.bytes += len as u64;
                st.claimed += 1;
                let epoch = st.epoch;
                drop(st);
                self.fill(s, len, epoch, token);
                st = self.st.lock().unwrap();
                continue;
            }
            // a worker is reading it right now
            st = self.ready.wait_timeout(st, Duration::from_millis(50)).unwrap().0;
        }
    }

    /// Drops every cached or queued chunk overlapping device range [a, b).
    pub(crate) fn invalidate(&self, ranges: &[(u64, u64)]) {
        let mut st = self.st.lock().unwrap();
        if st.chunks.is_empty() {
            return;
        }
        st.epoch += 1;
        for &(a, len) in ranges {
            let b = a + len;
            let hit: Vec<u64> = st.chunks.range(..b).filter(|(s, c)| **s + c.len as u64 > a).map(|(s, _)| *s).collect();
            for s in hit {
                let c = st.chunks.remove(&s).unwrap();
                if c.counted {
                    unbook(&mut st, c.len as u64);
                }
            }
        }
        let keep: std::collections::HashSet<u64> = st.chunks.keys().copied().collect();
        st.queue.retain(|j| keep.contains(&j.0));
        st.order.retain(|s| keep.contains(s));
        // queued chunks born before this generation stay valid (their ranges were not hit):
        // re-stamp them so the workers keep them
        let g = st.epoch;
        let starts: Vec<u64> = st.queue.iter().map(|j| j.0).collect();
        for s in starts {
            if let Some(c) = st.chunks.get_mut(&s) {
                c.born = g;
            }
        }
        for j in st.queue.iter_mut() {
            j.2 = g;
        }
        self.work.notify_all();
        self.ready.notify_all();
    }

    /// Bytes read into the cache so far.
    #[cfg(test)]
    pub(crate) fn read_bytes(&self) -> u64 {
        self.st.lock().unwrap().read_bytes
    }

    pub(crate) fn stats(&self) -> (u64, u64) {
        let st = self.st.lock().unwrap();
        (st.hits, st.misses)
    }
}

impl Drop for Ra {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Gives back a removed chunk's bytes (never below zero: a bug in the accounting must not
/// wrap the cache bound).
fn unbook(st: &mut St, l: u64) {
    debug_assert!(st.bytes >= l, "readahead accounting: {} - {l}", st.bytes);
    st.bytes = st.bytes.saturating_sub(l);
}

fn read_full(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], off + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    // bytes past the device end read as zeros (the chunk was aligned up)
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    /// A worker reads chunk A; a commit invalidates A and rewrites its bytes; a new prefetch
    /// queues a chunk at the same offset; only then does the old read complete. It must
    /// neither remove the replacement nor touch the byte accounting, and readers get the new
    /// bytes.
    #[test]
    fn a_stale_completion_never_touches_its_replacement() {
        let p = std::env::temp_dir().join(format!("fcp-ra-race-{}", std::process::id()));
        std::fs::write(&p, vec![0x11u8; 1 << 20]).unwrap();
        let f = Arc::new(File::options().read(true).write(true).open(&p).unwrap());
        let strat = Strategy {
            transport: "other".into(),
            gap: 0,
            inflight: 1,
            direct: false,
            overhead_ms: None,
            mbps: None,
            chunk: 64 << 10,
            tail: 4096,
        };
        let ra = Ra::new(f.clone(), strat, 16 << 20);
        let (read_done, go_on) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        *ra.pause.lock().unwrap() = Some((read_done.clone(), go_on.clone()));
        ra.prefetch(&[(0, 8192)]);
        read_done.wait(); // the worker has read the old bytes and waits before storing them
        f.write_all_at(&[0x22u8; 8192], 0).unwrap();
        ra.invalidate(&[(0, 8192)]);
        ra.prefetch(&[(0, 8192)]);
        assert_eq!(ra.st.lock().unwrap().bytes, 0, "the invalidated read gave its bytes back once");
        go_on.wait(); // the stale completion runs now
        let mut out = vec![0u8; 8192];
        let mut at = 0;
        while at < out.len() {
            let (got, direct) = ra.read_some(at as u64, &mut out[at..]);
            if got == 0 {
                f.read_exact_at(&mut out[at..at + direct], at as u64).unwrap();
                at += direct;
            } else {
                at += got;
            }
        }
        assert!(out.iter().all(|&b| b == 0x22), "a reader got bytes from before the commit");
        let st = ra.st.lock().unwrap();
        let counted: u64 = st.chunks.values().filter(|c| c.counted).map(|c| c.len as u64).sum();
        assert_eq!(st.bytes, counted, "byte accounting drifted");
        drop(st);
        ra.stop();
        let _ = std::fs::remove_file(&p);
    }
}
