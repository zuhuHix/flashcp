//! Device-op trace recorder: every read, write, sync and O_DIRECT toggle flashcp issues to the
//! TARGET device, as JSON lines, for the simulator ([`crate::sim`]).
//!
//! Enabled by `FLASHCP_TRACE=<file>` (or `--trace <file>`, which sets it). When off, every hook
//! is one relaxed atomic load before the syscall it wraps, so behaviour and speed are unchanged.
//!
//! # Format (version 2)
//! One JSON object per line. The first line is the header, the last one the end record:
//! ```text
//! {"v":2,"ev":"start","cmd":"pull","device":"sandisk-cruzer-usb2"}
//! {"i":0,"ev":"note","key":"strategy","val":{...}}                 any JSON value
//! {"i":1,"ev":"phase","name":"data","t":1234}
//! {"i":2,"ev":"io","op":"w","off":1048576,"len":122880,"qd":1,"grp":0,"seq":0,"q":1,"t":1250,"d":1262}
//! {"i":3,"ev":"sync","t":1300,"d":1310}
//! {"i":4,"ev":"direct","on":true,"t":1311}
//! {"ev":"end","t":5000}
//! ```
//! A run that panics writes `{"ev":"aborted","t":…}` instead of `end` (a panic hook drains
//! every thread's buffer first). Readers reject a trace without a clean `end` (or with an
//! `aborted` record) unless told to accept truncated traces.
//! - `i`: global issue order (an atomic counter taken when the op is issued). Each thread
//!   buffers its own records and writes them in batches, so lines of different threads are
//!   interleaved in chunks: a reader sorts by `i` (version 1 traces had no `i` and were in
//!   file order).
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
//!
//! # What a trace contains
//! Byte offsets and lengths of device ops, timestamps, queue depths, phase names, and the
//! strategy notes (pull's strategy object, add's gap-policy text). No file names, paths or
//! contents. Offsets and sizes still reveal the shape of the data (how many files, their sizes
//! and layout), so treat a trace of a private drive as potentially sensitive.
//!
//! # Cost
//! Off: one relaxed atomic load per op. On: two clock reads, one atomic increment and a
//! formatted line appended to a thread-local buffer, which is written to the file (under a
//! mutex) every [`BATCH`] bytes and when the thread ends, so threads issuing I/O in parallel
//! never wait for each other per op.

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// A thread's buffered records are written to the file when they exceed this many bytes.
pub const BATCH: usize = 64 << 10;

/// The environment variable that turns tracing on (the value is the output file).
pub const TRACE_ENV: &str = "FLASHCP_TRACE";

static ON: AtomicBool = AtomicBool::new(false);
static SINK: OnceLock<Sink> = OnceLock::new();
static GROUPS: AtomicU32 = AtomicU32::new(1);
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static ORDER: AtomicU64 = AtomicU64::new(0);

/// One thread's unwritten records. Kept in [`BUFS`] too, so [`finish`] can drain every
/// thread's buffer whether or not the thread still runs (thread-local destructors may run
/// after a scoped thread already counts as joined, so they cannot be relied on).
type Buf = std::sync::Arc<Mutex<Vec<u8>>>;
static BUFS: Mutex<Vec<Buf>> = Mutex::new(Vec::new());

thread_local! {
    static LOCAL: RefCell<Option<Buf>> = const { RefCell::new(None) };
}

fn write_out(buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    if let Some(sk) = SINK.get() {
        let mut o = sk.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = o.write_all(buf);
    }
    buf.clear();
}

/// Appends one record (with its issue order `i`) to this thread's buffer: its own mutex,
/// uncontended except when [`finish`] drains it.
fn record(i: u64, body: std::fmt::Arguments<'_>) {
    let put = |b: &mut Vec<u8>| {
        let _ = write!(b, r#"{{"i":{i},"#);
        let _ = b.write_fmt(body);
        b.push(b'\n');
        if b.len() >= BATCH {
            write_out(b);
        }
    };
    let done = LOCAL.try_with(|l| {
        let buf = l
            .borrow_mut()
            .get_or_insert_with(|| {
                let b: Buf = Default::default();
                BUFS.lock().unwrap_or_else(|e| e.into_inner()).push(b.clone());
                b
            })
            .clone();
        put(&mut buf.lock().unwrap_or_else(|e| e.into_inner()));
    });
    if done.is_err() {
        // Thread teardown: no thread-local any more, write directly.
        let mut b = Vec::new();
        put(&mut b);
        write_out(&mut b);
    }
}

fn next_i() -> u64 {
    ORDER.fetch_add(1, Relaxed)
}
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
        r#"{{"v":2,"ev":"start","cmd":{},"device":{}}}"#,
        crate::events::json_str(cmd),
        dev.map_or("null".into(), |d| crate::events::json_str(&d))
    ));
    PANIC_AFTER.store(std::env::var(PANIC_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(0), Relaxed);
    // A panic anywhere still leaves every buffered record in the file, marked aborted.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        abort_flush();
        prev(info);
    }));
    ON.store(true, Relaxed);
    Ok(())
}

/// Test hook: panic after this many traced I/O ops (0 = never).
const PANIC_ENV: &str = "FLASHCP_TEST_TRACE_PANIC_AFTER";
static PANIC_AFTER: AtomicU64 = AtomicU64::new(0);

/// Writes every thread's buffered records to the file. `wait`: block on locks (normal end);
/// otherwise skip whatever is locked (a panic hook must never deadlock).
fn drain_all(wait: bool) {
    let bufs = if wait { Some(BUFS.lock().unwrap_or_else(|e| e.into_inner())) } else { BUFS.try_lock().ok() };
    for b in bufs.iter().flat_map(|v| v.iter()) {
        let g = if wait { Some(b.lock().unwrap_or_else(|e| e.into_inner())) } else { b.try_lock().ok() };
        if let Some(mut g) = g {
            write_out(&mut g);
        }
    }
}

/// The panic path of [`finish`]: drains the buffers and writes an `aborted` record instead of
/// `end`, so the truncated trace cannot pass for a complete one.
fn abort_flush() {
    if !on() {
        return;
    }
    ON.store(false, Relaxed);
    drain_all(false);
    if let Some(sk) = SINK.get()
        && let Ok(mut o) = sk.out.try_lock()
    {
        let _ = writeln!(o, r#"{{"ev":"aborted","t":{}}}"#, now());
        let _ = o.flush();
    }
}

/// Writes the end record and flushes. Safe to call when tracing is off, and more than once
/// (later calls only flush).
pub fn finish() {
    if !on() {
        return;
    }
    ON.store(false, Relaxed);
    drain_all(true);
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
    let i = next_i();
    let t = now();
    let r = f();
    let d = now();
    INFLIGHT.fetch_sub(1, Relaxed);
    let o = if op == Op::Read { "r" } else { "w" };
    let limit = PANIC_AFTER.load(Relaxed);
    if limit > 0 && i + 1 >= limit {
        panic!("{PANIC_ENV}: forced panic after {limit} traced ops");
    }
    record(
        i,
        format_args!(
            r#""ev":"io","op":"{o}","off":{off},"len":{len},"qd":{qd},"grp":{grp},"seq":{seq},"q":{q},"t":{t},"d":{d}}}"#
        ),
    );
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
    let i = next_i();
    let t = now();
    let r = f();
    record(i, format_args!(r#""ev":"sync","t":{t},"d":{}}}"#, now()));
    r
}

/// Records an O_DIRECT toggle on the target.
#[inline(always)]
pub fn direct(on_: bool) {
    if on() {
        record(next_i(), format_args!(r#""ev":"direct","on":{on_},"t":{}}}"#, now()));
    }
}

/// Records a phase marker (e.g. add's "data", "alloc").
#[inline(always)]
pub fn phase(name: &str) {
    if on() {
        record(next_i(), format_args!(r#""ev":"phase","name":{},"t":{}}}"#, crate::events::json_str(name), now()));
    }
}

/// Records a note; `json` must be a valid JSON value (e.g. pull's strategy object).
#[inline(always)]
pub fn note(key: &str, json: impl FnOnce() -> String) {
    if on() {
        record(next_i(), format_args!(r#""ev":"note","key":{},"val":{}}}"#, crate::events::json_str(key), json()));
    }
}
