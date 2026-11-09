//! Device-op trace recorder: every read, write, sync and O_DIRECT toggle flashcp issues to the
//! TARGET device, as JSON lines, for the simulator ([`crate::sim`]).
//!
//! Enabled by `FLASHCP_TRACE=<file>` (or `--trace <file>`, which sets it). When off, every hook
//! is one relaxed atomic load before the syscall it wraps, so behaviour and speed are unchanged.
//!
//! # Format (version 1)
//! One JSON object per line. The first line is the header, the last one the end record:
//! ```text
//! {"v":1,"ev":"start","cmd":"pull","device":"sandisk-cruzer-usb2"}
//! {"ev":"note","key":"strategy","val":{...}}                 any JSON value
//! {"ev":"phase","name":"data","t":1234}
//! {"ev":"io","op":"w","off":1048576,"len":122880,"qd":1,"grp":0,"seq":0,"q":1,"t":1250,"d":1262}
//! {"ev":"sync","t":1300,"d":1310}
//! {"ev":"direct","on":true,"t":1311}
//! {"ev":"end","t":5000}
//! ```
//! - `t` / `d`: microseconds since the trace started, at issue and at completion (on the
//!   recording machine, i.e. usually an image on NVMe: only host time is meaningful there).
//! - `op`: `w` (write) or `r` (read); `off` is the byte offset in the target (partition), `len`
//!   the bytes of one syscall (the device may split it further, see the model).
//! - `qd`: how many ops of this group the issuing code allows in flight together, `grp` the
//!   dispatch group (0 = serial code). The simulator's dependency rule: an op with `qd == 1`
//!   or a different `grp` than the ops before it starts only after everything before it is
//!   done; within a group, op k waits for op k − qd of the same group.
//! - `seq`: the issuing code's own order inside a group (e.g. pull's chunk index); lines of
//!   one group may be written out of order by racing threads, sort by it.
//! - `q`: device ops actually in flight at issue time, including this one (informational).
//! - `sync`: fsync/fdatasync of the target: a barrier and a cache flush.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// The environment variable that turns tracing on (the value is the output file).
pub const TRACE_ENV: &str = "FLASHCP_TRACE";

static ON: AtomicBool = AtomicBool::new(false);
static SINK: OnceLock<Sink> = OnceLock::new();
static GROUPS: AtomicU32 = AtomicU32::new(1);
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

struct Sink {
    out: Mutex<BufWriter<File>>,
    t0: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
}

/// Whether tracing is on. The only cost of every hook when it is off.
#[inline(always)]
pub fn on() -> bool {
    ON.load(Relaxed)
}

fn line(s: &str) {
    if let Some(sk) = SINK.get() {
        let mut o = sk.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = o.write_all(s.as_bytes());
        let _ = o.write_all(b"\n");
    }
}

fn now() -> u64 {
    SINK.get().map_or(0, |s| s.t0.elapsed().as_micros() as u64)
}

/// Starts tracing if `FLASHCP_TRACE` is set (idempotent). `cmd` names the command; `device`
/// is the simulated device's name when `FLASHCP_SIM_DEVICE` is set. A trace file that cannot
/// be created is an error: the user asked for it.
pub fn init_from_env(cmd: &str) -> anyhow::Result<()> {
    match std::env::var_os(TRACE_ENV).filter(|p| !p.is_empty()) {
        Some(p) => init(std::path::Path::new(&p), cmd),
        None => Ok(()),
    }
}

/// Starts tracing into `path` (see [`init_from_env`]).
pub fn init(path: &std::path::Path, cmd: &str) -> anyhow::Result<()> {
    if SINK.get().is_some() {
        return Ok(());
    }
    let f = File::create(path).map_err(|e| anyhow::anyhow!("create trace file {}: {e}", path.display()))?;
    let _ = SINK.set(Sink { out: Mutex::new(BufWriter::with_capacity(1 << 20, f)), t0: Instant::now() });
    let dev = crate::sim::device::sim_device().map(|d| d.name.clone());
    line(&format!(
        r#"{{"v":1,"ev":"start","cmd":{},"device":{}}}"#,
        crate::events::json_str(cmd),
        dev.map_or("null".into(), |d| crate::events::json_str(&d))
    ));
    ON.store(true, Relaxed);
    Ok(())
}

/// Writes the end record and flushes. Safe to call when tracing is off, and more than once
/// (later calls only flush).
pub fn finish() {
    if !on() {
        return;
    }
    ON.store(false, Relaxed);
    line(&format!(r#"{{"ev":"end","t":{}}}"#, now()));
    if let Some(sk) = SINK.get() {
        let _ = sk.out.lock().unwrap_or_else(|e| e.into_inner()).flush();
    }
}

/// A new dispatch group id (for a pool of parallel writers or readers).
pub fn group() -> u32 {
    GROUPS.fetch_add(1, Relaxed)
}

/// Runs the device op `f` and records it (see the module docs for the fields).
#[inline(always)]
pub fn io<T>(op: Op, off: u64, len: u64, qd: usize, grp: u32, seq: u64, f: impl FnOnce() -> T) -> T {
    if !on() {
        return f();
    }
    let q = INFLIGHT.fetch_add(1, Relaxed) + 1;
    let t = now();
    let r = f();
    let d = now();
    INFLIGHT.fetch_sub(1, Relaxed);
    let o = if op == Op::Read { "r" } else { "w" };
    line(&format!(
        r#"{{"ev":"io","op":"{o}","off":{off},"len":{len},"qd":{qd},"grp":{grp},"seq":{seq},"q":{q},"t":{t},"d":{d}}}"#
    ));
    r
}

/// A serial (qd 1, group 0) device op.
#[inline(always)]
pub fn serial<T>(op: Op, off: u64, len: u64, f: impl FnOnce() -> T) -> T {
    io(op, off, len, 1, 0, 0, f)
}

/// Runs the target sync `f` (fsync / fdatasync) and records it.
#[inline(always)]
pub fn sync<T>(f: impl FnOnce() -> T) -> T {
    if !on() {
        return f();
    }
    let t = now();
    let r = f();
    line(&format!(r#"{{"ev":"sync","t":{t},"d":{}}}"#, now()));
    r
}

/// Records an O_DIRECT toggle on the target.
#[inline(always)]
pub fn direct(on_: bool) {
    if on() {
        line(&format!(r#"{{"ev":"direct","on":{on_},"t":{}}}"#, now()));
    }
}

/// Records a phase marker (e.g. add's "data", "alloc").
#[inline(always)]
pub fn phase(name: &str) {
    if on() {
        line(&format!(r#"{{"ev":"phase","name":{},"t":{}}}"#, crate::events::json_str(name), now()));
    }
}

/// Records a note; `json` must be a valid JSON value (e.g. pull's strategy object).
#[inline(always)]
pub fn note(key: &str, json: impl FnOnce() -> String) {
    if on() {
        line(&format!(r#"{{"ev":"note","key":{},"val":{}}}"#, crate::events::json_str(key), json()));
    }
}
