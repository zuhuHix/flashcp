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
}

#[derive(Default)]
struct St {
    chunks: BTreeMap<u64, Chunk>,
    order: VecDeque<u64>,
    queue: VecDeque<(u64, usize, u64)>,
    bytes: u64,
    epoch: u64,
    stop: bool,
    last_hit: Option<Instant>,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
}

pub(crate) struct Ra {
    file: Arc<File>,
    pub(crate) strat: Strategy,
    cap: u64,
    /// Trace group of the worker reads (they run `inflight` deep among themselves).
    grp: u32,
    st: Mutex<St>,
    cv: Condvar,
}

impl Ra {
    pub(crate) fn new(file: Arc<File>, strat: Strategy, cap: u64) -> Arc<Ra> {
        let ra = Arc::new(Ra {
            file,
            strat,
            cap,
            grp: crate::sim::trace::group(),
            st: Mutex::new(St::default()),
            cv: Condvar::new(),
        });
        for _ in 0..ra.strat.inflight.max(1) {
            let r = ra.clone();
            std::thread::spawn(move || r.worker());
        }
        ra
    }

    pub(crate) fn stop(&self) {
        self.st.lock().unwrap().stop = true;
        self.cv.notify_all();
    }

    fn worker(&self) {
        loop {
            let (start, len, epoch) = {
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
                            st.bytes += j.1 as u64;
                            break j;
                        }
                        continue;
                    }
                    st = self.cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
                }
            };
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
            let mut st = self.st.lock().unwrap();
            let stale = st.epoch != epoch;
            if let Some(c) = st.chunks.get_mut(&start) {
                if stale || c.born != epoch {
                    let l = c.len as u64;
                    st.chunks.remove(&start);
                    st.bytes -= l; // counted: a worker read it
                } else {
                    match ok {
                        Ok(n) if n >= len.min(1) => c.data = Some(Arc::new(buf)),
                        _ => c.failed = true,
                    }
                }
            }
            self.cv.notify_all();
        }
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
            // skip what overlaps a cached or queued chunk
            let overlaps = st
                .chunks
                .range(..ch.start + ch.len as u64)
                .next_back()
                .is_some_and(|(s, c)| *s + c.len as u64 > ch.start);
            if overlaps {
                continue;
            }
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
                },
            );
            st.order.push_back(ch.start);
            st.queue.push_back((ch.start, ch.len, epoch));
        }
        st.last_hit.get_or_insert_with(Instant::now);
        drop(st);
        self.evict();
        self.cv.notify_all();
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
                st.bytes -= l; // ready chunks are counted
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
        let mut waited_since: Option<Instant> = None;
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
                    st.bytes -= l;
                }
                st.hits += 1;
                st.last_hit = Some(Instant::now());
                self.cv.notify_all();
                return (n, 0);
            }
            // queued: wait while a worker reads it; if none has started it soon, read directly
            // (the reader may want chunks in another order than they were queued)
            if !c.counted {
                let waited = waited_since.get_or_insert_with(Instant::now);
                if waited.elapsed() > Duration::from_millis(20) {
                    let n = (s + c.len as u64 - off).min(out.len() as u64) as usize;
                    st.misses += 1;
                    return (0, n);
                }
            }
            st = self.cv.wait_timeout(st, Duration::from_millis(5)).unwrap().0;
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
                    st.bytes -= c.len as u64;
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
        self.cv.notify_all();
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
