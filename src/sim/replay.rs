//! Trace loading for the simulator: parses what [`crate::sim::trace`] records (format version 1)
//! into [`Trace`], builds synthetic probe-like traces ([`Trace::micro`]), and derives the
//! dependency structure and the host's own time ([`Trace::plan`]) that [`crate::sim::model`]
//! replays against a device model.
//!
//! # Host time
//! A recording is usually made on an image on a fast disk, so its device times mean nothing,
//! but the gaps between ops are the host's own work (scanning the source, hashing, writing
//! pull's destination files, ...). Each device op `i` (io or sync) gets a host gap
//! `t_i − max(t_{i−1}, d of what op i waited for)` (clamped ≥ 0), where "what it waited for" is
//! given by the dependency rule below; the first op's gap is its `t` (the head) and the tail is
//! `end − max(t, d over all ops)`. Replaying, op i is issued `gap_i × host_scale` after both
//! the previous op was issued and its dependency completed on the simulated device, so the
//! simulated total = simulated device time + the recorded host work, overlapped exactly as the
//! dependency rule allows.
//!
//! # Dependency rule (from the trace format)
//! An op with `qd == 1`, or a different `grp` than the io before it, or the first op after a
//! sync, waits for everything before it ([`Dep::All`]). Within a group, op k waits for op
//! k − qd of the same group ([`Dep::Op`]); the first qd − 1 ops after the group's first op only
//! wait for their turn to be issued ([`Dep::None`]). A sync is always a barrier.

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

/// One recorded event (times in µs since the trace started).
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    Io { write: bool, off: u64, len: u64, qd: usize, grp: u32, seq: u64, t: u64, d: u64 },
    Sync { t: u64, d: u64 },
    Phase { name: String, t: u64 },
    Direct { on: bool, t: u64 },
}

/// A parsed trace.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Trace {
    pub cmd: String,
    pub device: Option<String>,
    pub evs: Vec<Ev>,
    /// The end record's time (or the last time seen when there is none), µs.
    pub end_us: u64,
    /// `note` records: key and the raw JSON text of the value.
    pub notes: Vec<(String, String)>,
}

/// Totals over a trace (`host_us` is the host's own time, see the module docs).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TraceStats {
    pub reads: u64,
    pub writes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub syncs: u64,
    pub host_us: u64,
}

/// What a device op waits for before it may be issued (besides the previous op's issue).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dep {
    /// Everything before it is complete (a barrier).
    All,
    /// Op `j` (index into [`Plan::ops`]) is complete.
    Op(u32),
    /// Nothing: it only follows the previous op's issue.
    None,
}

/// One device op of a [`Plan`]. `len == 0 && !write` with `sync` set is a flush.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanOp {
    pub sync: bool,
    pub write: bool,
    pub off: u64,
    pub len: u64,
    /// Whether this op belongs to a group with qd > 1 (others may be in flight with it).
    pub par: bool,
    pub dep: Dep,
    /// Host gap before the issue, µs (see the module docs).
    pub gap_us: u64,
}

/// The replayable form of a trace: device ops with dependencies and host gaps, phase markers
/// (index of the next op and µs before it in the recording) and the tail.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Plan {
    pub ops: Vec<PlanOp>,
    /// (name, index of the first op at/after the marker, µs from the marker to that op's issue).
    pub phases: Vec<(String, usize, u64)>,
    pub tail_us: u64,
}

// ── a minimal JSON reader for the flat objects the recorder writes ──

#[derive(Debug)]
enum Val<'a> {
    Str(String),
    /// A number, bool, null, object or array: its raw text.
    Raw(&'a str),
}

struct P<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> Result<()> {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            bail!("expected '{}' at byte {}", c as char, self.i)
        }
    }
    fn string(&mut self) -> Result<String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'"' && self.b[self.i] != b'\\' {
                self.i += 1;
            }
            out.push_str(&self.s[start..self.i]);
            match self.b.get(self.i) {
                None => bail!("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => {
                    let e = *self.b.get(self.i + 1).ok_or_else(|| anyhow!("bad escape"))?;
                    self.i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) && self.s[self.i..].starts_with("\\u") {
                                self.i += 2;
                                let lo = self.hex4()?;
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => bail!("bad escape"),
                    }
                }
            }
        }
    }
    fn hex4(&mut self) -> Result<u32> {
        let h = self.s.get(self.i..self.i + 4).ok_or_else(|| anyhow!("bad \\u escape"))?;
        self.i += 4;
        u32::from_str_radix(h, 16).map_err(|_| anyhow!("bad \\u escape"))
    }
    /// Skips any JSON value and returns its raw text (strings are decoded instead).
    fn value(&mut self) -> Result<Val<'a>> {
        self.ws();
        let start = self.i;
        match self.b.get(self.i) {
            None => bail!("missing value"),
            Some(b'"') => Ok(Val::Str(self.string()?)),
            Some(b'{') | Some(b'[') => {
                let mut depth = 0usize;
                while self.i < self.b.len() {
                    match self.b[self.i] {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                return Ok(Val::Raw(&self.s[start..self.i]));
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
                bail!("unterminated object/array")
            }
            _ => {
                while self.i < self.b.len() && !matches!(self.b[self.i], b',' | b'}' | b']') {
                    self.i += 1;
                }
                let raw = self.s[start..self.i].trim();
                if raw.is_empty() {
                    bail!("missing value");
                }
                Ok(Val::Raw(raw))
            }
        }
    }
    fn object(&mut self) -> Result<Vec<(String, Val<'a>)>> {
        self.eat(b'{')?;
        let mut kv = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(kv);
        }
        loop {
            let k = self.string()?;
            self.eat(b':')?;
            kv.push((k, self.value()?));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    self.ws();
                    if self.i != self.b.len() {
                        bail!("trailing characters");
                    }
                    return Ok(kv);
                }
                _ => bail!("expected ',' or '}}' at byte {}", self.i),
            }
        }
    }
}

struct Obj<'a>(Vec<(String, Val<'a>)>);

impl Obj<'_> {
    fn get(&self, k: &str) -> Option<&Val<'_>> {
        self.0.iter().find(|(kk, _)| kk == k).map(|(_, v)| v)
    }
    fn str(&self, k: &str) -> Result<String> {
        match self.get(k) {
            Some(Val::Str(s)) => Ok(s.clone()),
            _ => bail!("missing string field {k:?}"),
        }
    }
    fn opt_str(&self, k: &str) -> Option<String> {
        match self.get(k) {
            Some(Val::Str(s)) => Some(s.clone()),
            _ => None,
        }
    }
    fn u64(&self, k: &str) -> Result<u64> {
        match self.get(k) {
            Some(Val::Raw(r)) => r.parse().map_err(|_| anyhow!("field {k:?} is not an unsigned integer: {r}")),
            _ => bail!("missing number field {k:?}"),
        }
    }
    fn raw(&self, k: &str) -> Result<String> {
        match self.get(k) {
            Some(Val::Raw(r)) => Ok(r.to_string()),
            Some(Val::Str(s)) => Ok(crate::events::json_str(s)),
            None => bail!("missing field {k:?}"),
        }
    }
}

/// Longest accepted trace line, in bytes (a record is ~150 bytes; a note a few hundred).
pub const MAX_LINE: usize = 64 << 10;
/// Most events accepted in one trace (~60 bytes each in memory: ≈1.2 GB). A 100k-file pull
/// records ~100k events.
pub const MAX_EVENTS: usize = 20_000_000;
/// Largest accepted trace file.
pub const MAX_FILE: u64 = 4 << 30;
/// Most bytes of note keys and values retained from one trace (notes are a few hundred bytes
/// each; they also count toward [`MAX_EVENTS`]).
pub const MAX_NOTE_BYTES: usize = 16 << 20;

/// Line-by-line trace parser (shared by [`Trace::parse`] and the streaming [`Trace::load`]).
#[derive(Default)]
struct Parser {
    tr: Trace,
    header: bool,
    version: u64,
    end: Option<u64>,
    last_t: u64,
    /// Issue order of each event (version 2), parallel to `tr.evs`.
    order: Vec<u64>,
    note_order: Vec<u64>,
    note_bytes: usize,
    aborted: bool,
    /// Accept a trace without a clean end record (see [`Trace::load_opts`]).
    allow_truncated: bool,
}

impl Parser {
    fn line(&mut self, n: usize, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        let ctx = || format!("trace line {n}");
        let o = Obj(P { s: line, b: line.as_bytes(), i: 0 }.object().with_context(ctx)?);
        let ev = o.str("ev").with_context(ctx)?;
        if !self.header {
            if ev != "start" {
                bail!("trace line {n}: the first record must be the start header");
            }
            let v = o.u64("v").with_context(ctx)?;
            if v != 1 && v != 2 {
                bail!("trace format version {v} is not supported (only 1 and 2)");
            }
            self.version = v;
            self.tr.cmd = o.str("cmd").with_context(ctx)?;
            self.tr.device = o.opt_str("device");
            self.header = true;
            return Ok(());
        }
        let i = if self.version >= 2 && ev != "end" && ev != "aborted" {
            o.u64("i").with_context(ctx)?
        } else {
            self.order.len() as u64
        };
        let e = match ev.as_str() {
            "io" => {
                let op = o.str("op").with_context(ctx)?;
                let write = match op.as_str() {
                    "w" => true,
                    "r" => false,
                    _ => bail!("trace line {n}: unknown op {op:?}"),
                };
                let qd = o.u64("qd").with_context(ctx)?.max(1) as usize;
                Ev::Io {
                    write,
                    off: o.u64("off").with_context(ctx)?,
                    len: o.u64("len").with_context(ctx)?,
                    qd,
                    grp: o.u64("grp").with_context(ctx)? as u32,
                    seq: o.u64("seq").with_context(ctx)?,
                    t: o.u64("t").with_context(ctx)?,
                    d: o.u64("d").with_context(ctx)?,
                }
            }
            "sync" => Ev::Sync { t: o.u64("t").with_context(ctx)?, d: o.u64("d").with_context(ctx)? },
            "phase" => Ev::Phase { name: o.str("name").with_context(ctx)?, t: o.u64("t").with_context(ctx)? },
            "direct" => {
                let on = match o.raw("on").with_context(ctx)?.as_str() {
                    "true" => true,
                    "false" => false,
                    r => bail!("trace line {n}: direct.on is not a bool: {r}"),
                };
                Ev::Direct { on, t: o.u64("t").with_context(ctx)? }
            }
            "note" => {
                let (k, v) = (o.str("key").with_context(ctx)?, o.raw("val").with_context(ctx)?);
                self.note_bytes += k.len() + v.len();
                if self.note_bytes > MAX_NOTE_BYTES {
                    bail!("trace line {n}: notes exceed {MAX_NOTE_BYTES} bytes (the limit)");
                }
                if self.tr.evs.len() + self.tr.notes.len() >= MAX_EVENTS {
                    bail!("trace has more than {MAX_EVENTS} events (the limit)");
                }
                self.tr.notes.push((k, v));
                self.note_order.push(i);
                return Ok(());
            }
            "end" => {
                self.end = Some(o.u64("t").with_context(ctx)?);
                return Ok(());
            }
            "aborted" => {
                self.aborted = true;
                return Ok(());
            }
            _ => return Ok(()),
        };
        if self.tr.evs.len() + self.tr.notes.len() >= MAX_EVENTS {
            bail!("trace has more than {MAX_EVENTS} events (the limit)");
        }
        self.last_t = self.last_t.max(ev_last_time(&e));
        self.tr.evs.push(e);
        self.order.push(i);
        Ok(())
    }

    fn finish(mut self) -> Result<Trace> {
        if !self.header {
            bail!("empty trace (no start header)");
        }
        if (self.aborted || self.end.is_none()) && !self.allow_truncated {
            bail!(
                "trace is truncated ({}): the recorded run did not finish cleanly; pass --allow-truncated to replay \
                 what it holds",
                if self.aborted { "the run panicked" } else { "no end record" }
            );
        }
        if self.version >= 2 {
            let mut ix: Vec<usize> = (0..self.tr.evs.len()).collect();
            ix.sort_by_key(|&k| self.order[k]);
            let mut evs: Vec<Option<Ev>> = std::mem::take(&mut self.tr.evs).into_iter().map(Some).collect();
            self.tr.evs = ix.iter().map(|&k| evs[k].take().expect("each event once")).collect();
            let mut nx: Vec<usize> = (0..self.tr.notes.len()).collect();
            nx.sort_by_key(|&k| self.note_order[k]);
            let notes = std::mem::take(&mut self.tr.notes);
            self.tr.notes = nx.iter().map(|&k| notes[k].clone()).collect();
        }
        self.tr.end_us = self.end.unwrap_or(self.last_t).max(self.last_t);
        self.tr.sort_groups();
        Ok(self.tr)
    }
}

impl Trace {
    /// Parses a version-1 or -2 trace (see [`crate::sim::trace`]). Version-2 records are put
    /// back in issue order (`i`); then each consecutive run of ios of the same group with
    /// qd > 1 is stably sorted by `seq`. Unknown event kinds are ignored; a missing end record
    /// takes the last time seen. Limits as in [`Trace::load`].
    pub fn parse(text: &str) -> Result<Trace> {
        Self::parse_opts(text, false)
    }

    /// [`Trace::parse`]; `allow_truncated` accepts a trace without a clean end record.
    pub fn parse_opts(text: &str, allow_truncated: bool) -> Result<Trace> {
        let mut p = Parser { allow_truncated, ..Default::default() };
        for (n, line) in text.lines().enumerate() {
            if line.len() > MAX_LINE {
                bail!("trace line {}: longer than {MAX_LINE} bytes", n + 1);
            }
            p.line(n + 1, line)?;
        }
        p.finish()
    }

    /// Streams a trace file: at most [`MAX_LINE`] bytes per line, [`MAX_EVENTS`] events and
    /// [`MAX_FILE`] bytes, so a huge or hostile file fails with a clear error instead of
    /// exhausting memory.
    pub fn load(path: &Path) -> Result<Trace> {
        Self::load_opts(path, false)
    }

    /// [`Trace::load`]; `allow_truncated` accepts a trace without a clean end record (a run
    /// that panicked or was killed): its ops are replayed as far as they go.
    pub fn load_opts(path: &Path, allow_truncated: bool) -> Result<Trace> {
        use std::io::{BufRead, Read};
        let f = std::fs::File::open(path).with_context(|| format!("read trace {}", path.display()))?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len > MAX_FILE {
            bail!("trace {} is {len} bytes, more than the {MAX_FILE}-byte limit", path.display());
        }
        let mut r = std::io::BufReader::with_capacity(1 << 20, f);
        let mut p = Parser { allow_truncated, ..Default::default() };
        let mut buf = Vec::new();
        let mut n = 0usize;
        loop {
            buf.clear();
            // Bounded: never buffers more than MAX_LINE + 1 bytes of one line.
            let got = (&mut r)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut buf)
                .with_context(|| format!("read trace {}", path.display()))?;
            if got == 0 {
                break;
            }
            n += 1;
            if buf.len() > MAX_LINE && buf.last() != Some(&b'\n') {
                bail!("trace {} line {n}: longer than {MAX_LINE} bytes", path.display());
            }
            let line =
                std::str::from_utf8(&buf).with_context(|| format!("trace {} line {n}: not UTF-8", path.display()))?;
            p.line(n, line.trim_end_matches(['\n', '\r']))
                .with_context(|| format!("parse trace {}", path.display()))?;
        }
        p.finish().with_context(|| format!("parse trace {}", path.display()))
    }

    fn sort_groups(&mut self) {
        let key = |e: &Ev| match e {
            Ev::Io { qd, grp, .. } if *qd > 1 => Some(*grp),
            _ => None,
        };
        let mut i = 0;
        while i < self.evs.len() {
            let Some(g) = key(&self.evs[i]) else {
                i += 1;
                continue;
            };
            let mut j = i + 1;
            while j < self.evs.len() && key(&self.evs[j]) == Some(g) {
                j += 1;
            }
            self.evs[i..j].sort_by_key(|e| if let Ev::Io { seq, .. } = e { *seq } else { 0 });
            i = j;
        }
    }

    /// A synthetic probe-like trace with zero host time:
    /// - `seqw`: `bytes / bs` writes of `bs` at ascending offsets, then a sync;
    /// - `randw`: the same at random 4 KB-aligned offsets over 1 GB (seeded, like the probe), then a sync;
    /// - `flush`: `bytes / bs` times (a `bs` write at ascending offsets + a sync);
    /// - `seqr`: `bytes / bs` reads of `bs` at ascending offsets.
    ///
    /// With `qd > 1` the ops form one group (op k waits for op k − qd).
    pub fn micro(kind: &str, bs: u64, qd: usize, bytes: u64) -> Result<Trace> {
        if bs == 0 || bytes < bs {
            bail!("micro trace: need bs > 0 and bytes ≥ bs");
        }
        let n = bytes / bs;
        let qd = qd.max(1);
        let grp = if qd > 1 { 1 } else { 0 };
        let io = |write: bool, off: u64, i: u64| Ev::Io {
            write,
            off,
            len: bs,
            qd,
            grp,
            seq: if qd > 1 { i } else { 0 },
            t: 0,
            d: 0,
        };
        let mut evs = Vec::with_capacity(n as usize * 2 + 1);
        match kind {
            "seqw" => {
                evs.extend((0..n).map(|i| io(true, i * bs, i)));
                evs.push(Ev::Sync { t: 0, d: 0 });
            }
            "randw" => {
                let mut rng = crate::util::Rng(0xF1A5);
                let blocks = (1u64 << 30) / 4096;
                let span = blocks.saturating_sub(bs.div_ceil(4096)).max(1);
                evs.extend((0..n).map(|i| io(true, rng.range(0, span) * 4096, i)));
                evs.push(Ev::Sync { t: 0, d: 0 });
            }
            "flush" => {
                for i in 0..n {
                    evs.push(io(true, i * bs, i));
                    evs.push(Ev::Sync { t: 0, d: 0 });
                }
            }
            "seqr" => evs.extend((0..n).map(|i| io(false, i * bs, i))),
            _ => bail!("micro trace: unknown kind {kind:?} (seqw | randw | flush | seqr)"),
        }
        Ok(Trace { cmd: format!("micro-{kind}"), device: None, evs, end_us: 0, notes: Vec::new() })
    }

    pub fn stats(&self) -> TraceStats {
        let mut s = TraceStats::default();
        for e in &self.evs {
            match e {
                Ev::Io { write: true, len, .. } => {
                    s.writes += 1;
                    s.write_bytes += len;
                }
                Ev::Io { write: false, len, .. } => {
                    s.reads += 1;
                    s.read_bytes += len;
                }
                Ev::Sync { .. } => s.syncs += 1,
                _ => {}
            }
        }
        let p = self.plan();
        s.host_us = p.ops.iter().map(|o| o.gap_us).sum::<u64>() + p.tail_us;
        if p.ops.is_empty() {
            s.host_us = self.end_us;
        }
        s
    }

    /// The replayable form (dependencies and host gaps, see the module docs).
    pub fn plan(&self) -> Plan {
        let mut p = Plan::default();
        let mut pend_phases: Vec<(String, u64)> = Vec::new();
        // Recording-side state.
        let mut rec_done: Vec<u64> = Vec::new(); // d of every op so far
        let mut max_d = 0u64;
        let mut max_t = 0u64;
        let mut prev_t: Option<u64> = None;
        // Group state: (grp, qd, indices of the group's ops).
        let mut cur: Option<(u32, Vec<u32>)> = None;
        let mut after_sync = false;
        for e in &self.evs {
            let (op, t, d, grp, qd) = match *e {
                Ev::Io { write, off, len, qd, grp, t, d, .. } => {
                    (PlanOp { sync: false, write, off, len, par: qd > 1, dep: Dep::All, gap_us: 0 }, t, d, grp, qd)
                }
                Ev::Sync { t, d } => (
                    PlanOp { sync: true, write: false, off: 0, len: 0, par: false, dep: Dep::All, gap_us: 0 },
                    t,
                    d,
                    0,
                    1,
                ),
                Ev::Phase { ref name, t } => {
                    pend_phases.push((name.clone(), t));
                    continue;
                }
                Ev::Direct { .. } => continue,
            };
            let idx = p.ops.len() as u32;
            let mut op = op;
            let mut dep_d = max_d;
            if op.sync || qd <= 1 || after_sync || cur.as_ref().is_none_or(|(g, _)| *g != grp) {
                op.dep = Dep::All;
                cur = if op.sync || qd <= 1 { None } else { Some((grp, vec![idx])) };
            } else {
                let (_, members) = cur.as_mut().expect("group");
                let k = members.len();
                if k >= qd {
                    let j = members[k - qd];
                    op.dep = Dep::Op(j);
                    dep_d = rec_done[j as usize];
                } else {
                    op.dep = Dep::None;
                    dep_d = 0;
                }
                members.push(idx);
            }
            after_sync = op.sync;
            let ready = prev_t.unwrap_or(0).max(dep_d);
            op.gap_us = t.saturating_sub(ready);
            for (name, pt) in pend_phases.drain(..) {
                p.phases.push((name, idx as usize, t.saturating_sub(pt).min(op.gap_us)));
            }
            prev_t = Some(prev_t.map_or(t, |pt| pt.max(t)));
            rec_done.push(d);
            max_d = max_d.max(d);
            max_t = max_t.max(t);
            p.ops.push(op);
        }
        let last = max_d.max(max_t);
        for (name, pt) in pend_phases.drain(..) {
            p.phases.push((name, p.ops.len(), self.end_us.saturating_sub(pt)));
        }
        p.tail_us = self.end_us.saturating_sub(last);
        p
    }
}

fn ev_last_time(e: &Ev) -> u64 {
    match *e {
        Ev::Io { t, d, .. } | Ev::Sync { t, d } => t.max(d),
        Ev::Phase { t, .. } | Ev::Direct { t, .. } => t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"v":1,"ev":"start","cmd":"pull","device":"sandisk-cruzer-usb2"}
{"ev":"note","key":"strategy","val":{"transport":"usb-storage","gap_kb":0,"s":"a,}\"b","arr":[1,{"x":2}]}}
{"ev":"phase","name":"data","t":1234}
{"ev":"io","op":"r","off":1048576,"len":122880,"qd":2,"grp":1,"seq":1,"q":1,"t":1250,"d":1262}
{"ev":"io","op":"r","off":0,"len":122880,"qd":2,"grp":1,"seq":0,"q":2,"t":1251,"d":1263}
{"ev":"sync","t":1300,"d":1310}
{"ev":"direct","on":true,"t":1311}
{"ev":"io","op":"w","off":4096,"len":512,"qd":1,"grp":0,"seq":0,"q":1,"t":1400,"d":1410}
{"ev":"end","t":5000}
"#;

    #[test]
    fn parses_every_record_and_sorts_groups() {
        let t = Trace::parse(SAMPLE).unwrap();
        assert_eq!(t.cmd, "pull");
        assert_eq!(t.device.as_deref(), Some("sandisk-cruzer-usb2"));
        assert_eq!(t.end_us, 5000);
        assert_eq!(t.notes.len(), 1);
        assert_eq!(t.notes[0].1, r#"{"transport":"usb-storage","gap_kb":0,"s":"a,}\"b","arr":[1,{"x":2}]}"#);
        assert_eq!(t.evs.len(), 6);
        assert!(matches!(t.evs[1], Ev::Io { seq: 0, off: 0, .. }));
        assert!(matches!(t.evs[2], Ev::Io { seq: 1, off: 1048576, .. }));
        assert_eq!(t.evs[4], Ev::Direct { on: true, t: 1311 });
        let s = t.stats();
        assert_eq!((s.reads, s.writes, s.read_bytes, s.write_bytes, s.syncs), (2, 1, 245760, 512, 1));
        assert!(Trace::parse("").is_err());
        assert!(Trace::parse(r#"{"v":3,"ev":"start","cmd":"x","device":null}"#).is_err());
        assert!(Trace::parse(r#"{"ev":"io"}"#).is_err());
        let no_dev =
            Trace::parse("{\"v\":1,\"ev\":\"start\",\"cmd\":\"x\",\"device\":null}\n{\"ev\":\"end\",\"t\":0}").unwrap();
        assert_eq!(no_dev.device, None);
    }

    #[test]
    fn plan_dependencies_and_host_gaps() {
        let t = Trace::parse(SAMPLE).unwrap();
        let p = t.plan();
        assert_eq!(p.ops.len(), 4);
        // first op of the group: barrier, head = its t (the sorted first op, t 1251)
        assert_eq!(p.ops[0].dep, Dep::All);
        assert_eq!(p.ops[0].gap_us, 1251);
        assert_eq!(p.ops[1].dep, Dep::None);
        assert_eq!(p.ops[1].gap_us, 0); // issued 1250 < previous t 1251
        assert!(p.ops[2].sync && p.ops[2].dep == Dep::All);
        assert_eq!(p.ops[2].gap_us, 1300 - 1263);
        assert_eq!(p.ops[3].dep, Dep::All);
        assert_eq!(p.ops[3].gap_us, 1400 - 1310);
        assert_eq!(p.tail_us, 5000 - 1410);
        assert_eq!(p.phases, vec![("data".to_string(), 0, 17)]);
        assert_eq!(t.stats().host_us, 1251 + 37 + 90 + 3590);
    }

    #[test]
    fn group_waits_for_k_minus_qd() {
        let mut s = String::from("{\"v\":1,\"ev\":\"start\",\"cmd\":\"x\",\"device\":null}\n");
        for k in 0..5 {
            s.push_str(&format!(
                "{{\"ev\":\"io\",\"op\":\"r\",\"off\":{},\"len\":4096,\"qd\":2,\"grp\":3,\"seq\":{k},\"q\":1,\"t\":{},\"d\":{}}}\n",
                k * 4096,
                k * 10,
                k * 10 + 15
            ));
        }
        s.push_str("{\"ev\":\"end\",\"t\":60}\n");
        let p = Trace::parse(&s).unwrap().plan();
        let deps: Vec<Dep> = p.ops.iter().map(|o| o.dep).collect();
        assert_eq!(deps, vec![Dep::All, Dep::None, Dep::Op(0), Dep::Op(1), Dep::Op(2)]);
        // op 2 at t 20 waited for op 0 (d 15) and op 1's issue (10): gap 5
        assert_eq!(p.ops[2].gap_us, 5);
    }

    #[test]
    fn micro_traces() {
        let t = Trace::micro("seqw", 4096, 1, 40960).unwrap();
        assert_eq!(t.evs.len(), 11);
        assert_eq!(t.stats().host_us, 0);
        let r = Trace::micro("randw", 4096, 1, 4096 * 100).unwrap();
        assert_eq!(r, Trace::micro("randw", 4096, 1, 4096 * 100).unwrap());
        assert!(r.evs.iter().all(|e| match e {
            Ev::Io { off, .. } => off % 4096 == 0 && *off < 1 << 30,
            _ => true,
        }));
        let f = Trace::micro("flush", 4096, 1, 4096 * 3).unwrap();
        assert_eq!(f.stats().syncs, 3);
        let q = Trace::micro("seqr", 1 << 20, 4, 8 << 20).unwrap();
        assert_eq!(q.plan().ops[4].dep, Dep::Op(0));
        assert!(Trace::micro("nope", 4096, 1, 4096).is_err());
    }

    #[test]
    fn v2_records_are_put_back_in_issue_order() {
        // Two threads' batches, interleaved in chunks: file order is not issue order.
        let t = Trace::parse(concat!(
            r#"{"v":2,"ev":"start","cmd":"pull","device":null}"#,
            "\n",
            r#"{"i":2,"ev":"io","op":"r","off":300,"len":100,"qd":1,"grp":0,"seq":0,"q":1,"t":30,"d":31}"#,
            "\n",
            r#"{"i":0,"ev":"io","op":"r","off":100,"len":100,"qd":1,"grp":0,"seq":0,"q":1,"t":10,"d":11}"#,
            "\n",
            r#"{"i":3,"ev":"note","key":"b","val":2}"#,
            "\n",
            r#"{"i":1,"ev":"note","key":"a","val":1}"#,
            "\n",
            r#"{"ev":"end","t":40}"#,
        ))
        .unwrap();
        let offs: Vec<u64> = t.evs.iter().map(|e| if let Ev::Io { off, .. } = e { *off } else { 0 }).collect();
        assert_eq!(offs, [100, 300]);
        assert_eq!(t.notes[0].0, "a");
        // v2 records must carry `i`.
        let bad = "{\"v\":2,\"ev\":\"start\",\"cmd\":\"x\",\"device\":null}\n{\"ev\":\"sync\",\"t\":1,\"d\":2}";
        assert!(Trace::parse(bad).is_err());
    }

    #[test]
    fn load_streams_and_refuses_overlong_lines() {
        let dir = std::env::temp_dir().join(format!("flashcp-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok.jsonl");
        std::fs::write(&ok, SAMPLE).unwrap();
        assert_eq!(Trace::load(&ok).unwrap().evs.len(), Trace::parse(SAMPLE).unwrap().evs.len());
        let long = dir.join("long.jsonl");
        let mut text = String::from("{\"v\":1,\"ev\":\"start\",\"cmd\":\"x\",\"device\":null}\n");
        text.push_str(&"x".repeat(MAX_LINE + 10));
        std::fs::write(&long, text).unwrap();
        let e = format!("{:#}", Trace::load(&long).unwrap_err());
        assert!(e.contains("longer than"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn notes_count_toward_the_caps() {
        // A notes-only flood: refused once the retained note bytes pass the cap.
        let mut s = String::from("{\"v\":1,\"ev\":\"start\",\"cmd\":\"x\",\"device\":null}\n");
        let val = "x".repeat(60_000);
        for _ in 0..(MAX_NOTE_BYTES / val.len() + 2) {
            s.push_str(&format!("{{\"ev\":\"note\",\"key\":\"k\",\"val\":\"{val}\"}}\n"));
        }
        s.push_str("{\"ev\":\"end\",\"t\":1}\n");
        let e = format!("{:#}", Trace::parse(&s).unwrap_err());
        assert!(e.contains("notes exceed"), "{e}");
    }

    #[test]
    fn truncated_traces_are_rejected_unless_allowed() {
        let head = "{\"v\":1,\"ev\":\"start\",\"cmd\":\"x\",\"device\":null}\n\
                    {\"ev\":\"sync\",\"t\":1,\"d\":2}\n";
        for (text, why) in
            [(head.to_string(), "no end record"), (format!("{head}{{\"ev\":\"aborted\",\"t\":3}}\n"), "panicked")]
        {
            let e = format!("{:#}", Trace::parse(&text).unwrap_err());
            assert!(e.contains("truncated") && e.contains(why) && e.contains("--allow-truncated"), "{e}");
            assert_eq!(Trace::parse_opts(&text, true).unwrap().evs.len(), 1);
        }
    }
}
