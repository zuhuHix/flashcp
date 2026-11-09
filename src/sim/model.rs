//! Device performance model and the trace replay engine.
//!
//! # Replay
//! A trace's [`Plan`](crate::sim::replay::Plan) gives every device op its dependency and its
//! recorded host gap (see [`crate::sim::replay`]). Op i is issued at
//! `max(issue_{i−1}, done(dep_i)) + gap_i × host_scale`; the total is the last completion (or
//! issue) plus the recorded tail × `host_scale`.
//!
//! Every syscall is split into requests of at most `max_req` bytes (the kernel's
//! `max_sectors_kb`). Issued syscalls form a round-robin queue: the device takes one request
//! from the front syscall and puts it back at the end while it has requests left, so requests
//! of syscalls in flight together interleave (on BOT every switch between two of them is a
//! jump; this is why 4 × 8 MB reads in flight are much slower than 2).
//!
//! # Device
//! `lanes` command slots (BOT: 1, UAS: the queue depth). A request takes a lane at
//! `X = max(lane free, arrival)` and spends `cmd + position penalty + queue penalty` in it
//! (overlapped across lanes), then `len / bw + FTL cost` on the shared bus (serialized, FIFO).
//! With one lane this is the plain `cmd + len / bw` per request.
//!
//! Position penalties, per direction (reads and writes keep separate heads):
//! - contiguous with the previous request of that direction: 0;
//! - reads only, a request continuing a recent sequential read stream (one of the last 64):
//!   `rd.switch` if that stream is among the `rd.streams` most recently used ones (the
//!   device's read-ahead buffers still hold it), else `rd.thrash` (its read-ahead was
//!   evicted and wasted: why 4 interleaved 8 MB reads are far slower than 2 on the SanDisk,
//!   while scattered single reads only pay a jump);
//! - otherwise a jump: `fwd` (forward) or `back` (backward).
//!
//! `qpen` is charged per request dispatched while another syscall is already waiting behind it
//! (an empirical term: on the SanDisk, gapped writes with 8 in flight measured ~30 % slower than
//! with 1 in flight although BOT runs them one at a time in the same order). A sync waits for
//! the device to drain and costs `flush`. Writes go through the optional [`Ftl`].
//!
//! Every parameter is in SI units (seconds, bytes, bytes/s).

use crate::sim::ftl::{Ftl, FtlState};
use crate::sim::replay::{Dep, Plan, Trace};
use anyhow::{Result, anyhow, bail};
use std::collections::VecDeque;

const MAGIC: &str = "flashcp-simmodel 1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// USB Bulk-Only Transport (usb-storage): one command at a time.
    Bot,
    /// USB Attached SCSI (or anything with a real command queue).
    Uas,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::Bot => "bot",
            Transport::Uas => "uas",
        }
    }
}

/// Costs of one direction (read or write).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirCost {
    /// Seconds per request (command + status overhead).
    pub cmd: f64,
    /// Transfer bandwidth, bytes/s.
    pub bw: f64,
    /// Extra seconds for a forward / backward jump.
    pub fwd: f64,
    pub back: f64,
    /// Extra seconds per request dispatched while another syscall waits behind it.
    pub qpen: f64,
    /// Seconds per request on the shared bus/controller, serialized like the transfer (not
    /// overlapped across lanes): the drive's IOPS ceiling under deep queues. Redundant with
    /// `cmd` on BOT (one lane).
    pub ser: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub transport: Transport,
    pub lanes: usize,
    pub max_req: u64,
    pub rd: DirCost,
    /// Extra seconds for a read continuing a tracked (not the latest) read stream.
    pub rd_switch: f64,
    /// Read streams the device tracks (read-ahead), LRU.
    pub rd_streams: usize,
    /// Extra seconds for a read continuing a recent stream that is no longer among the
    /// `rd_streams` buffered ones (its read-ahead was wasted).
    pub rd_thrash: f64,
    pub wr: DirCost,
    /// Seconds per sync (cache flush).
    pub flush: f64,
    /// Multiplier on the recorded host time.
    pub host_scale: f64,
    pub ftl: Option<Ftl>,
}

const KEYS: &[&str] = &[
    "lanes",
    "max_req",
    "rd.cmd",
    "rd.bw",
    "rd.fwd",
    "rd.back",
    "rd.qpen",
    "rd.ser",
    "rd.switch",
    "rd.streams",
    "rd.thrash",
    "wr.cmd",
    "wr.bw",
    "wr.fwd",
    "wr.back",
    "wr.qpen",
    "wr.ser",
    "flush",
    "host_scale",
    "ftl.au",
    "ftl.open",
    "ftl.open_cost",
    "ftl.merge",
    "ftl.copy_bw",
    "ftl.gc_p",
    "ftl.gc_mean",
    "ftl.gc_sigma",
];

impl Model {
    /// The numeric parameter names [`get`](Model::get)/[`set`](Model::set) accept.
    pub fn keys() -> &'static [&'static str] {
        KEYS
    }

    pub fn get(&self, key: &str) -> Option<f64> {
        let f = self.ftl.as_ref();
        Some(match key {
            "lanes" => self.lanes as f64,
            "max_req" => self.max_req as f64,
            "rd.cmd" => self.rd.cmd,
            "rd.bw" => self.rd.bw,
            "rd.fwd" => self.rd.fwd,
            "rd.back" => self.rd.back,
            "rd.qpen" => self.rd.qpen,
            "rd.ser" => self.rd.ser,
            "rd.switch" => self.rd_switch,
            "rd.streams" => self.rd_streams as f64,
            "rd.thrash" => self.rd_thrash,
            "wr.cmd" => self.wr.cmd,
            "wr.bw" => self.wr.bw,
            "wr.fwd" => self.wr.fwd,
            "wr.back" => self.wr.back,
            "wr.qpen" => self.wr.qpen,
            "wr.ser" => self.wr.ser,
            "flush" => self.flush,
            "host_scale" => self.host_scale,
            "ftl.au" => f?.au as f64,
            "ftl.open" => f?.open as f64,
            "ftl.open_cost" => f?.open_cost,
            "ftl.merge" => f?.merge,
            "ftl.copy_bw" => f?.copy_bw,
            "ftl.gc_p" => f?.gc_p,
            "ftl.gc_mean" => f?.gc_mean,
            "ftl.gc_sigma" => f?.gc_sigma,
            _ => return None,
        })
    }

    /// Sets a numeric parameter (integers are rounded). Setting any `ftl.*` key on a model
    /// without an FTL enables one with [`Ftl::default`] values first.
    pub fn set(&mut self, key: &str, v: f64) -> Result<()> {
        if !v.is_finite() || v < 0.0 {
            bail!("{key}={v}: must be finite and ≥ 0");
        }
        let int = |v: f64, min: f64| -> Result<u64> {
            if v.round() < min {
                bail!("{key}={v}: must be ≥ {min}");
            }
            Ok(v.round() as u64)
        };
        let pos = |v: f64| -> Result<f64> {
            if v <= 0.0 {
                bail!("{key}={v}: must be > 0");
            }
            Ok(v)
        };
        if key.starts_with("ftl.") && KEYS.contains(&key) && self.ftl.is_none() {
            self.ftl = Some(Ftl::default());
        }
        let f = &mut self.ftl;
        match key {
            "lanes" => self.lanes = int(v, 1.0)? as usize,
            "max_req" => self.max_req = int(v, 512.0)?,
            "rd.cmd" => self.rd.cmd = v,
            "rd.bw" => self.rd.bw = pos(v)?,
            "rd.fwd" => self.rd.fwd = v,
            "rd.back" => self.rd.back = v,
            "rd.qpen" => self.rd.qpen = v,
            "rd.ser" => self.rd.ser = v,
            "rd.switch" => self.rd_switch = v,
            "rd.streams" => self.rd_streams = int(v, 0.0)? as usize,
            "rd.thrash" => self.rd_thrash = v,
            "wr.cmd" => self.wr.cmd = v,
            "wr.bw" => self.wr.bw = pos(v)?,
            "wr.fwd" => self.wr.fwd = v,
            "wr.back" => self.wr.back = v,
            "wr.qpen" => self.wr.qpen = v,
            "wr.ser" => self.wr.ser = v,
            "flush" => self.flush = v,
            "host_scale" => self.host_scale = v,
            "ftl.au" => f.as_mut().expect("ftl").au = int(v, 4096.0)?,
            "ftl.open" => f.as_mut().expect("ftl").open = int(v, 1.0)? as usize,
            "ftl.open_cost" => f.as_mut().expect("ftl").open_cost = v,
            "ftl.merge" => f.as_mut().expect("ftl").merge = v,
            "ftl.copy_bw" => f.as_mut().expect("ftl").copy_bw = pos(v)?,
            "ftl.gc_p" => f.as_mut().expect("ftl").gc_p = v.min(1.0),
            "ftl.gc_mean" => f.as_mut().expect("ftl").gc_mean = v,
            "ftl.gc_sigma" => f.as_mut().expect("ftl").gc_sigma = v,
            _ => bail!("unknown model parameter {key:?} (known: {})", KEYS.join(", ")),
        }
        Ok(())
    }

    /// Parses the `key=value` text format (first line `flashcp-simmodel 1`, `#` comments).
    /// Keys: `name`, `transport` (bot | uas) and every [`keys`](Model::keys) entry; missing keys
    /// keep the transport's generic preset ([`crate::sim::profiles::generic`]); `ftl=off`
    /// removes the FTL, any `ftl.*` key enables it.
    pub fn parse(text: &str) -> Result<Model> {
        let mut lines = text.lines();
        let first = lines.next().unwrap_or("").trim();
        if first != MAGIC {
            bail!("not a simulator model (first line must be {MAGIC:?})");
        }
        let mut kv: Vec<(String, String)> = Vec::new();
        for (n, l) in lines.enumerate() {
            let l = l.split('#').next().unwrap_or("").trim();
            if l.is_empty() {
                continue;
            }
            let (k, v) = l.split_once('=').ok_or_else(|| anyhow!("model line {}: expected key=value", n + 2))?;
            kv.push((k.trim().to_string(), v.trim().to_string()));
        }
        let transport = match kv.iter().find(|(k, _)| k == "transport").map(|(_, v)| v.as_str()) {
            None | Some("bot") | Some("usb-storage") => Transport::Bot,
            Some("uas") => Transport::Uas,
            Some(t) => bail!("unknown transport {t:?} (bot | uas)"),
        };
        let mut m = crate::sim::profiles::generic(transport);
        let explicit_ftl = kv.iter().any(|(k, _)| k.starts_with("ftl."));
        if explicit_ftl {
            m.ftl = Some(m.ftl.unwrap_or_default());
        }
        for (k, v) in &kv {
            match k.as_str() {
                "name" => m.name = v.clone(),
                "transport" => {}
                "ftl" => match v.as_str() {
                    "off" | "0" | "none" => m.ftl = None,
                    "on" | "1" => m.ftl = Some(m.ftl.take().unwrap_or_default()),
                    _ => bail!("ftl={v}: expected on | off"),
                },
                _ => {
                    let x: f64 = v.parse().map_err(|_| anyhow!("{k}={v}: not a number"))?;
                    m.set(k, x)?;
                }
            }
        }
        if explicit_ftl && kv.iter().any(|(k, v)| k == "ftl" && matches!(v.as_str(), "off" | "0" | "none")) {
            bail!("ftl=off together with ftl.* parameters");
        }
        Ok(m)
    }

    /// The text format; [`parse`](Model::parse) of it gives back exactly this model.
    pub fn render(&self) -> String {
        let mut s = format!("{MAGIC}\nname={}\ntransport={}\n", self.name, self.transport.name());
        for k in KEYS {
            if let Some(v) = self.get(k) {
                s.push_str(&format!("{k}={v}\n"));
            }
        }
        if self.ftl.is_none() {
            s.push_str("ftl=off\n");
        }
        s
    }
}

/// The result of one simulated run.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SimResult {
    pub secs: f64,
    /// Seconds the device spent serving requests (summed over lanes).
    pub device_busy_secs: f64,
    /// Host time from the recording, × host_scale.
    pub host_secs: f64,
    /// Per phase marker: seconds from it to the next marker (or the end).
    pub phases: Vec<(String, f64)>,
    pub requests: u64,
    /// Requests not contiguous with the previous request of their direction.
    pub jumps: u64,
    /// FTL merges of partial AUs.
    pub merges: u64,
    /// FTL GC stalls (seeded runs only).
    pub stalls: u64,
}

/// Spread of the total over seeds.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Spread {
    pub mean: f64,
    pub p10: f64,
    pub p90: f64,
    pub n: usize,
}

/// Simulates `t` on `m`; `seed` drives the FTL's GC stalls.
pub fn simulate(t: &Trace, m: &Model, seed: u64) -> SimResult {
    simulate_plan(&t.plan(), m, Some(seed))
}

/// Deterministic simulation: GC stalls charged at their mean (what calibration uses).
pub fn simulate_det(t: &Trace, m: &Model) -> SimResult {
    simulate_plan(&t.plan(), m, None)
}

/// Runs seeds 0..n; returns the run with the median total and the spread of the totals.
pub fn simulate_seeds(t: &Trace, m: &Model, seeds: usize) -> (SimResult, Spread) {
    let plan = t.plan();
    let n = seeds.max(1);
    let mut runs: Vec<SimResult> = (0..n as u64).map(|s| simulate_plan(&plan, m, Some(s))).collect();
    runs.sort_by(|a, b| a.secs.total_cmp(&b.secs));
    let q = |p: f64| runs[((n - 1) as f64 * p).round() as usize].secs;
    let spread = Spread { mean: runs.iter().map(|r| r.secs).sum::<f64>() / n as f64, p10: q(0.1), p90: q(0.9), n };
    let mid = runs.swap_remove((n - 1) / 2);
    (mid, spread)
}

/// Read streams remembered for detecting sequential access (more than the device buffers).
const STREAM_HISTORY: usize = 64;

struct Active {
    op: usize,
    off: u64,
    end: u64,
    arr: f64,
}

/// Simulates a precomputed plan (`seed` None = deterministic). Cost: O(requests).
pub fn simulate_plan(p: &Plan, m: &Model, seed: Option<u64>) -> SimResult {
    let n = p.ops.len();
    let hs = m.host_scale * 1e-6;
    let max_req = m.max_req.max(512);
    let lanes = m.lanes.max(1);
    let mut lane_free = vec![0.0f64; lanes];
    let mut bus_free = 0.0f64;
    let mut arr = vec![0.0f64; n];
    let mut done = vec![f64::NAN; n];
    let mut issued = 0usize;
    let mut queued = 0usize;
    let mut undone = 0usize;
    let mut max_done = 0.0f64;
    let mut last_issue = 0.0f64;
    let mut rr: VecDeque<Active> = VecDeque::new();
    let mut ftl = m.ftl.as_ref().map(|f| FtlState::new(f, seed));
    let mut r = SimResult::default();
    let mut rd_last: Option<u64> = None;
    let mut wr_last: Option<u64> = None;
    // Ends of recent read streams, least recently used first.
    let mut streams: Vec<u64> = Vec::with_capacity(STREAM_HISTORY + 1);
    let mut busy = 0.0f64;
    loop {
        while issued < n {
            let o = &p.ops[issued];
            let ready = match o.dep {
                Dep::All if undone == 0 => max_done,
                Dep::All => break,
                Dep::Op(j) => {
                    let d = done[j as usize];
                    if d.is_nan() {
                        break;
                    }
                    d
                }
                Dep::None => 0.0,
            };
            let a = last_issue.max(ready) + o.gap_us as f64 * hs;
            arr[issued] = a;
            last_issue = a;
            issued += 1;
            undone += 1;
        }
        let lane = (0..lanes).min_by(|&a, &b| lane_free[a].total_cmp(&lane_free[b])).unwrap_or(0);
        let lf = lane_free[lane];
        let t0 = match rr.front() {
            Some(f) => lf.max(f.arr),
            None if queued < issued => lf.max(arr[queued]),
            None => break,
        };
        while queued < issued && arr[queued] <= t0 {
            let o = &p.ops[queued];
            rr.push_back(Active { op: queued, off: o.off, end: o.off + o.len, arr: arr[queued] });
            queued += 1;
        }
        let Some(mut a) = rr.pop_front() else { break };
        let x = lf.max(a.arr);
        let o = &p.ops[a.op];
        let waiting = !rr.is_empty();
        let end;
        if o.sync {
            end = (x + m.flush).max(bus_free + m.flush);
            busy += m.flush;
            bus_free = end;
            lane_free[lane] = end;
        } else {
            let len = (a.end - a.off).min(max_req);
            let off = a.off;
            r.requests += 1;
            let (dc, last) = if o.write { (&m.wr, &mut wr_last) } else { (&m.rd, &mut rd_last) };
            let mut pen = 0.0;
            // Read streams: rank of the stream this request continues (0 = most recent).
            let mut rank = None;
            if !o.write && m.rd_streams > 0 {
                if let Some(k) = streams.iter().rposition(|&e| e == off) {
                    rank = Some(streams.len() - 1 - k);
                    streams.remove(k);
                } else if streams.len() >= STREAM_HISTORY {
                    streams.remove(0);
                }
                streams.push(off + len);
            }
            if let Some(l) = *last
                && l != off
            {
                r.jumps += 1;
                pen = match rank {
                    Some(k) if k < m.rd_streams => m.rd_switch,
                    Some(_) => m.rd_thrash,
                    None if off > l => dc.fwd,
                    None => dc.back,
                };
            }
            *last = Some(off + len);
            if waiting {
                pen += dc.qpen;
            }
            let mut xfer = len as f64 / dc.bw + dc.ser;
            if o.write
                && let Some(f) = ftl.as_mut()
            {
                xfer += f.write(off, len);
            }
            let cmd_end = x + dc.cmd + pen;
            end = cmd_end.max(bus_free) + xfer;
            busy += dc.cmd + pen + xfer;
            bus_free = end;
            lane_free[lane] = end;
            a.off += len;
        }
        if a.off < a.end {
            rr.push_back(a);
        } else {
            let k = a.op;
            done[k] = end;
            max_done = max_done.max(end);
            undone -= 1;
        }
    }
    if let Some(f) = &ftl {
        r.merges = f.counts.merges;
        r.stalls = f.counts.stalls;
    }
    let total = max_done.max(last_issue) + p.tail_us as f64 * hs;
    r.secs = total;
    r.device_busy_secs = busy;
    r.host_secs = (p.ops.iter().map(|o| o.gap_us).sum::<u64>() + p.tail_us) as f64 * hs;
    let mut marks: Vec<(String, f64)> = p
        .phases
        .iter()
        .map(|(name, idx, before)| {
            let at = if *idx < n { arr[*idx] } else { total };
            (name.clone(), (at - *before as f64 * hs).max(0.0))
        })
        .collect();
    for i in 0..marks.len() {
        let next = marks.get(i + 1).map_or(total, |m| m.1);
        marks[i].1 = (next - marks[i].1).max(0.0);
    }
    r.phases = marks;
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::replay::Ev;

    fn bot() -> Model {
        Model {
            name: "t".into(),
            transport: Transport::Bot,
            lanes: 1,
            max_req: 1000,
            rd: DirCost { cmd: 0.001, bw: 1e6, fwd: 0.0, back: 0.0, qpen: 0.0, ser: 0.0 },
            rd_switch: 0.0,
            rd_streams: 0,
            rd_thrash: 0.0,
            wr: DirCost { cmd: 0.001, bw: 1e6, fwd: 0.0, back: 0.0, qpen: 0.0, ser: 0.0 },
            flush: 0.002,
            host_scale: 1.0,
            ftl: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn io(write: bool, off: u64, len: u64, qd: usize, grp: u32, seq: u64, t: u64, d: u64) -> Ev {
        Ev::Io { write, off, len, qd, grp, seq, t, d }
    }

    fn tr(evs: Vec<Ev>, end_us: u64) -> Trace {
        Trace { cmd: "t".into(), device: None, evs, end_us, notes: vec![] }
    }

    #[test]
    fn render_parse_round_trip() {
        let mut m = bot();
        m.rd.fwd = 0.1 + 0.2; // a value with a long decimal form
        m.ftl = Some(Ftl::default());
        let back = Model::parse(&m.render()).unwrap();
        assert_eq!(back, m);
        m.ftl = None;
        assert_eq!(Model::parse(&m.render()).unwrap(), m);
        for k in Model::keys() {
            let mut x = m.clone();
            let mut with_ftl = m.clone();
            with_ftl.ftl = Some(Ftl::default());
            let v = if *k == "ftl.gc_p" { 0.5 } else { with_ftl.get(k).unwrap().max(512.0) * 2.0 };
            x.set(k, v).unwrap();
            assert_eq!(x.get(k), Some(v), "{k}");
            assert_eq!(Model::parse(&x.render()).unwrap(), x, "{k}");
        }
        assert!(Model::parse("nope").is_err());
        assert!(Model::parse("flashcp-simmodel 1\nbogus=1\n").is_err());
        let c = Model::parse("flashcp-simmodel 1\n# comment\nname=x # trailing\ntransport=uas\nrd.cmd=0.5\n").unwrap();
        assert_eq!((c.name.as_str(), c.transport, c.rd.cmd), ("x", Transport::Uas, 0.5));
    }

    #[test]
    fn qd1_serial_vs_qd2_overlap_with_host_gaps() {
        // Two reads of 1000 B (1 request each, 2 ms each), the recording shows 500 µs of host
        // work before each (t − previous d).
        let serial = tr(vec![io(false, 0, 1000, 1, 0, 0, 500, 600), io(false, 1000, 1000, 1, 0, 0, 1100, 1200)], 1200);
        let r = simulate_det(&serial, &bot());
        assert!((r.secs - (0.0005 + 0.002 + 0.0005 + 0.002)).abs() < 1e-9, "{}", r.secs);
        assert!((r.host_secs - 0.001).abs() < 1e-12);
        // Same with qd 2: the second is issued 0 µs after the first (no dependency): host work
        // overlaps the device.
        let par = tr(vec![io(false, 0, 1000, 2, 1, 0, 500, 600), io(false, 1000, 1000, 2, 1, 1, 500, 1200)], 1200);
        let r2 = simulate_det(&par, &bot());
        assert!((r2.secs - (0.0005 + 0.004)).abs() < 1e-9, "{}", r2.secs);
        // host_scale stretches only the host part
        let mut m = bot();
        m.host_scale = 3.0;
        let r3 = simulate_det(&serial, &m);
        assert!((r3.secs - (0.003 + 0.004)).abs() < 1e-9);
    }

    #[test]
    fn splits_at_max_req_and_counts_interleave_jumps() {
        let m = bot();
        let one = tr(vec![io(false, 0, 2500, 1, 0, 0, 0, 0)], 0);
        let r = simulate_det(&one, &m);
        assert_eq!((r.requests, r.jumps), (3, 0));
        assert!((r.secs - (3.0 * 0.001 + 0.0025)).abs() < 1e-9);
        // two 3-request reads in flight together interleave round-robin: every switch jumps
        let two = tr(vec![io(false, 0, 3000, 2, 1, 0, 0, 0), io(false, 10_000, 3000, 2, 1, 1, 0, 0)], 0);
        let r = simulate_det(&two, &m);
        assert_eq!((r.requests, r.jumps), (6, 5));
        // with qd 1 they run back to back: one jump (the first request of a trace is none)
        let ser = tr(vec![io(false, 0, 3000, 1, 0, 0, 0, 0), io(false, 10_000, 3000, 1, 0, 0, 0, 0)], 0);
        assert_eq!(simulate_det(&ser, &m).jumps, 1);
        // jump penalties apply per switch; a tracked stream pays the switch cost instead
        let mut mj = m.clone();
        mj.rd.fwd = 0.01;
        mj.rd.back = 0.1;
        let base = simulate_det(&two, &m).secs;
        let pen = simulate_det(&two, &mj).secs - base;
        assert!((pen - (3.0 * 0.01 + 2.0 * 0.1)).abs() < 1e-9, "{pen}");
        mj.rd_streams = 2;
        mj.rd_switch = 0.001;
        mj.rd_thrash = 1.0;
        let pen2 = simulate_det(&two, &mj).secs - base;
        assert!((pen2 - (0.01 + 4.0 * 0.001)).abs() < 1e-9, "{pen2}");
        // three interleaved streams with 2 read-ahead buffers: every request after the first
        // round continues a stream whose buffer was evicted
        let three = tr(
            vec![
                io(false, 0, 3000, 3, 1, 0, 0, 0),
                io(false, 10_000, 3000, 3, 1, 1, 0, 0),
                io(false, 20_000, 3000, 3, 1, 2, 0, 0),
            ],
            0,
        );
        mj.rd.fwd = 0.0;
        mj.rd.back = 0.0;
        let mut plain = mj.clone();
        plain.rd_thrash = 0.0;
        let thrash = simulate_det(&three, &mj).secs - simulate_det(&three, &plain).secs;
        assert!((thrash - 6.0).abs() < 1e-9, "{thrash}");
    }

    #[test]
    fn sync_is_a_barrier_with_flush() {
        let t =
            tr(vec![io(true, 0, 1000, 1, 0, 0, 0, 0), Ev::Sync { t: 0, d: 0 }, io(true, 1000, 1000, 1, 0, 0, 0, 0)], 0);
        let r = simulate_det(&t, &bot());
        assert!((r.secs - (0.002 + 0.002 + 0.002)).abs() < 1e-9, "{}", r.secs);
    }

    #[test]
    fn uas_lanes_overlap_command_cost() {
        let mut m = bot();
        m.transport = Transport::Uas;
        m.lanes = 4;
        let t = tr(vec![io(true, 0, 4000, 1, 0, 0, 0, 0)], 0);
        let r = simulate_det(&t, &m);
        // 4 requests: commands overlap (1 ms), transfers serialize (4 ms)
        assert!((r.secs - 0.005).abs() < 1e-9, "{}", r.secs);
        m.lanes = 1;
        assert!((simulate_det(&t, &m).secs - 0.008).abs() < 1e-9);
    }

    #[test]
    fn ftl_random_vs_sequential_and_seed_determinism() {
        let mut m = bot();
        m.max_req = 122880;
        m.ftl = Some(Ftl { gc_p: 0.3, gc_mean: 0.05, ..Ftl::default() });
        let seq = Trace::micro("seqw", 4096, 1, 4096 * 512).unwrap();
        let rnd = Trace::micro("randw", 4096, 1, 4096 * 512).unwrap();
        let (s, r) = (simulate_det(&seq, &m), simulate_det(&rnd, &m));
        assert!(r.secs > s.secs + 400.0 * 0.019, "{} vs {}", r.secs, s.secs);
        assert_eq!(s.merges, 0);
        // GC stalls hit block closes (the sequential stream's full AUs), seeded
        let big = Trace::micro("seqw", 1 << 20, 1, 256 << 20).unwrap();
        let a = simulate(&big, &m, 7);
        assert_eq!(a, simulate(&big, &m, 7));
        assert_ne!(a.secs, simulate(&big, &m, 8).secs);
        assert!(a.stalls > 0);
        let (_, sp) = simulate_seeds(&big, &m, 9);
        assert!(sp.p10 <= sp.mean && sp.mean <= sp.p90 && sp.n == 9);
    }

    #[test]
    fn phases_split_the_total() {
        let t = tr(
            vec![
                Ev::Phase { name: "a".into(), t: 0 },
                io(true, 0, 1000, 1, 0, 0, 100, 150),
                Ev::Phase { name: "b".into(), t: 200 },
                io(true, 1000, 1000, 1, 0, 0, 300, 350),
            ],
            400,
        );
        let r = simulate_det(&t, &bot());
        assert_eq!(r.phases.len(), 2);
        let sum: f64 = r.phases.iter().map(|p| p.1).sum();
        assert!((sum - r.secs).abs() < 1e-9);
        // b starts 100 µs before the second write's issue
        assert!((r.phases[1].1 - (0.0001 + 0.002 + 0.00005)).abs() < 1e-9, "{:?}", r.phases);
    }

    #[test]
    fn simulates_40k_ops_fast() {
        let mut m = bot();
        m.max_req = 122880;
        m.ftl = Some(Ftl::default());
        let t = Trace::micro("randw", 4096, 1, 4096 * 40_000).unwrap();
        let plan = t.plan();
        let t0 = std::time::Instant::now();
        let r = simulate_plan(&plan, &m, None);
        let el = t0.elapsed();
        assert_eq!(r.requests, 40_000);
        // generous bound for debug builds; release is well under 10 ms
        assert!(el.as_millis() < 200, "{el:?}");
    }
}
