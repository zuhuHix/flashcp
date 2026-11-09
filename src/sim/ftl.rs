//! A small flash-translation-layer model for cheap USB sticks (hybrid / block-mapped FTL with a
//! few open allocation units), used by [`crate::sim::model`] for write requests.
//!
//! # What it models
//! The medium is cut into allocation units (AUs, `au` bytes, typically 4 MiB on USB 2 sticks).
//! The controller keeps up to `open` AUs open for writing (LRU). Per write piece (a request is
//! split at AU boundaries):
//! - **sequential**: the piece starts at the open AU's write pointer → free (the plain bus and
//!   command costs of the model already cover it). A new AU opened at its start, or right after
//!   an open AU (a stream crossing into the next AU), is also a sequential open: free.
//! - **forward gap** inside an open AU (gapped writes, e.g. `add` skipping cluster slack): free
//!   now, but the AU becomes *partial*: when it is closed later (evicted from the open set),
//!   the controller must copy everything that was not written into the new block, a
//!   **deferred merge** costing `merge + (au − written) / copy_bw`.
//! - **random open** (a jump to an AU that is not open and does not continue one) or a
//!   **backward** write inside an open AU: the write goes through the controller's page-mapped
//!   log, costing `open_cost` immediately; such an AU is never merged (the log's own cleanup
//!   is folded into `open_cost`). This is what makes random 4 KB QD1 writes ~20 ms each.
//! - **GC stalls**: every block close (a complete sequential AU or a merge) triggers garbage collection
//!   with probability `gc_p`, stalling a lognormal time with mean `gc_mean` and log-sigma
//!   `gc_sigma` (seeded). Deterministic mode charges `gc_p × gc_mean` per close instead.
//!
//! AUs still open when the trace ends are not charged (the stick merges them in idle time).
//!
//! # Why this shape
//! - Random 4 KB QD1 at 48 IOPS (20.8 ms) against 1.64 ms for a sequential 4 KB write: the
//!   difference, ~19 ms, is `open_cost`.
//! - `add` onto 32 KB clusters with every gap skipped (one ascending stream, ~2.7 MB of gaps per
//!   4 MiB AU) measured far slower than its bytes and commands explain, but faster than
//!   zero-filling the gaps over the bus: the stick copies the gaps internally faster than USB 2
//!   can write them, i.e. `copy_bw` > the bus write bandwidth.
//! - Sequential streams (build, probe seq writes) never pay anything but GC.

use crate::util::Rng;

/// FTL parameters (see the module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct Ftl {
    /// Allocation unit, bytes.
    pub au: u64,
    /// AUs open for writing at once (LRU).
    pub open: usize,
    /// Seconds per random open / backward write (page-mapped log path).
    pub open_cost: f64,
    /// Fixed seconds per deferred merge of a partial AU.
    pub merge: f64,
    /// Internal copy bandwidth of a merge, bytes/s.
    pub copy_bw: f64,
    /// Probability that closing an AU triggers a GC stall.
    pub gc_p: f64,
    /// Mean GC stall, seconds.
    pub gc_mean: f64,
    /// Log-sigma of the lognormal GC stall.
    pub gc_sigma: f64,
}

impl Default for Ftl {
    /// A generic cheap USB 2 stick (the SanDisk fit's shape, see [`crate::sim::profiles`]).
    fn default() -> Self {
        Ftl {
            au: 4 << 20,
            open: 2,
            open_cost: 0.019,
            merge: 0.005,
            copy_bw: 12e6,
            gc_p: 0.0,
            gc_mean: 0.25,
            gc_sigma: 0.8,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Au {
    idx: u64,
    wp: u64,
    written: u64,
    partial: bool,
    logged: bool,
    used: u64,
}

/// Counters an FTL run reports.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FtlCounts {
    pub merges: u64,
    pub stalls: u64,
    pub random_opens: u64,
}

/// The FTL's running state during one simulation.
pub struct FtlState<'a> {
    p: &'a Ftl,
    open: Vec<Au>,
    clock: u64,
    /// None: deterministic (GC at its mean).
    rng: Option<Rng>,
    pub counts: FtlCounts,
}

impl<'a> FtlState<'a> {
    pub fn new(p: &'a Ftl, seed: Option<u64>) -> Self {
        FtlState {
            p,
            open: Vec::with_capacity(p.open.max(1) + 1),
            clock: 0,
            rng: seed.map(|s| Rng(s.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xF7F7_0001)),
            counts: FtlCounts::default(),
        }
    }

    /// The extra seconds one write request of `len` bytes at `off` costs.
    pub fn write(&mut self, off: u64, len: u64) -> f64 {
        let au = self.p.au.max(4096);
        let mut cost = 0.0;
        let (mut o, end) = (off, off + len);
        while o < end {
            let idx = o / au;
            let in_au = o - idx * au;
            let piece = (end - o).min(au - in_au);
            cost += self.piece(idx, in_au, piece);
            o += piece;
        }
        cost
    }

    fn piece(&mut self, idx: u64, o: u64, l: u64) -> f64 {
        let au = self.p.au.max(4096);
        self.clock += 1;
        let mut cost = 0.0;
        if let Some(k) = self.open.iter().position(|a| a.idx == idx) {
            let a = &mut self.open[k];
            a.used = self.clock;
            if o == a.wp {
                a.wp += l;
            } else if o > a.wp {
                a.partial = true;
                a.wp = o + l;
            } else {
                // A rewrite behind the write pointer: through the log.
                cost += self.p.open_cost;
                a.logged = true;
                self.counts.random_opens += 1;
            }
            a.written = (a.written + l).min(au);
            if a.wp >= au && !a.partial && !a.logged {
                self.open.swap_remove(k);
                cost += self.close_cost(None);
            }
            return cost;
        }
        let seq = o == 0 || self.open.iter().any(|a| a.idx + 1 == idx && !a.logged);
        let mut a = Au { idx, wp: o + l, written: l, partial: o > 0, logged: false, used: self.clock };
        if !seq {
            cost += self.p.open_cost;
            a.logged = true;
            self.counts.random_opens += 1;
        }
        if self.open.len() >= self.p.open.max(1) {
            let k = (0..self.open.len()).min_by_key(|&k| self.open[k].used).expect("open AU");
            let old = self.open.swap_remove(k);
            cost += self.close_cost(Some(old));
        }
        if a.wp >= au && !a.partial && !a.logged {
            cost += self.close_cost(None);
        } else {
            self.open.push(a);
        }
        cost
    }

    /// Cost of closing an AU: the merge of a partial one (None = a complete sequential AU)
    /// plus the GC chance.
    fn close_cost(&mut self, a: Option<Au>) -> f64 {
        let au = self.p.au.max(4096);
        let mut c = 0.0;
        if let Some(a) = a
            && !a.logged
            && (a.partial || a.wp < au)
        {
            self.counts.merges += 1;
            c += self.p.merge + au.saturating_sub(a.written) as f64 / self.p.copy_bw.max(1.0);
        }
        // Logged (random) AUs never got a block of their own: no close, no GC.
        if self.p.gc_p > 0.0 && self.p.gc_mean > 0.0 && !a.is_some_and(|a| a.logged) {
            match &mut self.rng {
                None => c += self.p.gc_p * self.p.gc_mean,
                Some(r) => {
                    if unit(r) < self.p.gc_p {
                        self.counts.stalls += 1;
                        let s = self.p.gc_sigma.max(0.0);
                        let mu = self.p.gc_mean.ln() - s * s / 2.0;
                        c += (mu + s * normal(r)).exp();
                    }
                }
            }
        }
        c
    }
}

fn unit(r: &mut Rng) -> f64 {
    (r.next() >> 11) as f64 / (1u64 << 53) as f64
}

fn normal(r: &mut Rng) -> f64 {
    let u1 = unit(r).max(1e-300);
    let u2 = unit(r);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftl() -> Ftl {
        Ftl { au: 1 << 20, open: 2, open_cost: 0.02, merge: 0.01, copy_bw: 1e6, gc_p: 0.0, ..Ftl::default() }
    }

    #[test]
    fn sequential_stream_is_free() {
        let p = ftl();
        let mut s = FtlState::new(&p, None);
        let c: f64 = (0..64u64).map(|i| s.write(i * 120 * 1024, 120 * 1024)).sum();
        assert_eq!(c, 0.0);
        assert_eq!(s.counts, FtlCounts::default());
    }

    #[test]
    fn random_writes_pay_open_cost_without_merges() {
        let p = ftl();
        let mut s = FtlState::new(&p, None);
        let mut r = Rng(1);
        let c: f64 = (0..100).map(|_| s.write(r.range(1, 1000) * (1 << 20) + 8192, 4096)).sum();
        assert!(c >= 100.0 * 0.02 * 0.95, "{c}");
        assert_eq!(s.counts.merges, 0);
    }

    #[test]
    fn gapped_stream_merges_on_close() {
        let p = ftl();
        let mut s = FtlState::new(&p, None);
        // 4 KB every 32 KB over 4 AUs: AUs 0 and 1 get evicted (2 open), each 1/8 written.
        let c: f64 = (0..128u64).map(|i| s.write(i * 32768, 4096)).sum();
        assert_eq!(s.counts.merges, 2);
        let per = 0.01 + (1u64 << 20) as f64 * 7.0 / 8.0 / 1e6;
        assert!((c - 2.0 * per).abs() < 1e-9, "{c} vs {}", 2.0 * per);
        // a backward write inside an open AU goes through the log
        let c2 = s.write(3 * (1 << 20), 4096);
        assert!((c2 - 0.02).abs() < 1e-12);
    }

    #[test]
    fn gc_is_seeded() {
        let p = Ftl { gc_p: 0.5, gc_mean: 0.1, ..ftl() };
        let run = |seed| {
            let mut s = FtlState::new(&p, Some(seed));
            (0..400u64).map(|i| s.write(i * (1 << 19), 1 << 19)).sum::<f64>()
        };
        assert_eq!(run(3), run(3));
        assert_ne!(run(3), run(4));
        let mut d = FtlState::new(&p, None);
        let det: f64 = (0..400u64).map(|i| d.write(i * (1 << 19), 1 << 19)).sum();
        assert!((det - 200.0 * 0.05).abs() < 1e-9, "{det}");
    }
}
