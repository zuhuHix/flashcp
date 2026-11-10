//! Built-in device models, conversion from flashcp's cached device profiles, and model lookup.
//!
//! Two profiles are FITTED to real measurements (the derivations are in their doc comments and
//! the numbers are in `scenarios.txt`): `sandisk-cruzer-usb2` and `msi-datamag-uas`. The other
//! three (`usb2-cheap`, `usb3-midrange`, `nvme-usb`) are GUESSES at typical hardware of that
//! class, for what-if comparisons only: no drive of those classes was measured.

use crate::devprofile::DevProfile;
use crate::sim::ftl::Ftl;
use crate::sim::model::{DirCost, Model, Transport};
use anyhow::{Context, Result, bail};
use std::path::Path;

const NAMES: &[&str] =
    &["sandisk-cruzer-usb2", "msi-datamag-uas", "kingston-dt3-usb3", "usb2-cheap", "usb3-midrange", "nvme-usb"];

pub fn builtin_names() -> &'static [&'static str] {
    NAMES
}

/// A built-in model by name (also the aliases `sandisk` and `msi`).
pub fn builtin(name: &str) -> Option<Model> {
    Some(match name {
        "sandisk-cruzer-usb2" | "sandisk" => sandisk(),
        "msi-datamag-uas" | "msi" => msi(),
        "kingston-dt3-usb3" | "kingston" => kingston(),
        "usb2-cheap" => usb2_cheap(),
        "usb3-midrange" => usb3_midrange(),
        "nvme-usb" => nvme_usb(),
        _ => return None,
    })
}

/// The calibration PRIOR of a drive: its built-in profile with every parameter that was
/// derived from the end-to-end scenarios (rather than from `flashcp probe`, sysfs or the
/// device's cached profile) reset to the transport's generic guess. Calibrating from this,
/// the held-out (cross-validated) predictions never benefit from a hand-tuned value that has
/// seen the very point being predicted.
///
/// SanDisk, kept (probe / sysfs / cached profile): `wr.cmd`, `wr.bw` (seq write sizes),
/// `rd.bw` (cached read probe), `max_req` (sysfs), `ftl.open_cost` (random 4K), `flush`, `ftl.au`,
/// `ftl.open`. Reset: `rd.cmd`, `rd.fwd`, `rd.back`, `rd.switch`, `rd.thrash`, `wr.qpen`,
/// `ftl.merge`, `ftl.copy_bw`, `ftl.gc_*`. MSI: only the probe numbers were used, so the prior
/// is the profile itself.
pub fn calibration_prior(name: &str) -> Option<Model> {
    let mut m = builtin(name)?;
    if m.name == "sandisk-cruzer-usb2" {
        let g = usb2_cheap();
        m.rd.cmd = g.rd.cmd;
        m.rd.fwd = g.rd.fwd;
        m.rd.back = g.rd.back;
        m.rd_switch = g.rd_switch;
        m.rd_thrash = g.rd_thrash;
        // Positive so that the log-space fit can move it.
        m.wr.qpen = 0.0005;
        if let (Some(f), Some(gf)) = (m.ftl.as_mut(), g.ftl.as_ref()) {
            f.merge = gf.merge;
            f.copy_bw = gf.copy_bw;
            f.gc_p = gf.gc_p;
            f.gc_mean = gf.gc_mean;
            f.gc_sigma = gf.gc_sigma;
        }
        m.name = "sandisk-cruzer-usb2 (probe-only prior)".into();
    }
    Some(m)
}

/// The generic preset of a transport: what [`Model::parse`] and [`from_devprofile`] start from
/// for every parameter they are not given (BOT: [`usb2_cheap`], UAS: [`usb3_uas_generic`]).
pub fn generic(t: Transport) -> Model {
    match t {
        Transport::Bot => usb2_cheap(),
        Transport::Uas => usb3_uas_generic(),
    }
}

/// SanDisk Cruzer Blade 8 GB, USB 2.0, usb-storage (BOT), max request 120 KB. FITTED.
///
/// - Writes, from `flashcp probe` (seq 4K 2.5 / 64K 6.2 / 1M 6.4 / 8M 6.5 MB/s): a request
///   costs `wr.cmd + len / wr.bw`. 4 KB: 1.64 ms; 8 MB = 69 requests at 6.5 MB/s. Solving
///   both: `wr.cmd` ≈ 1.05 ms, `wr.bw` ≈ 6.88 MB/s (also the "speed council" fit and the cached
///   `write_skip_s`). Then 64K → 6.2 and 1M → 6.45 MB/s, as measured.
/// - Random 4 KB QD1 at 48 IOPS = 20.8 ms each, 19.2 ms more than a sequential 4 KB write:
///   `ftl.open_cost`. `ftl.au` 4 MiB (add's in-situ probe uses 4 MB units; typical for this
///   class), 2 open AUs.
/// - Gapped writes (add on 32 KB clusters, gaps skipped, 96–114 s) cost ~50 s more than their
///   bytes and commands; zero-filling instead (more bytes over the bus) is slower still
///   (120–173 s): the stick merges each partially written AU by copying its unwritten part
///   internally at `ftl.copy_bw`, faster than the bus. `ftl.merge`/`ftl.copy_bw` were
///   calibrated on add's scenarios (see `calib`), which also pulls them toward add into an
///   empty 32 KB volume (64 s, which needs almost no merge cost: the stick's history matters).
/// - GC: the ±40 % run-to-run spread (same mkfs+cp 110 s and 180 s) is modelled as rare
///   lognormal stalls at AU closes; `ftl.gc_p × ftl.gc_mean` is small so that the
///   deterministic (mean) model still meets the sequential probe numbers.
/// - Reads: pull of the flashcp layout (208 MB, ~1960 contiguous 120 KB requests, 16.0 s) and
///   of the kernel layout (181 MB, 20 200 requests that all jump forward, 27.0 s) give
///   `rd.bw` ≈ 13.8 MB/s (the cached probe), `rd.cmd` ≈ 0.27 ms and `rd.fwd` ≈ 0.4 ms. Two 8 MB
///   reads in flight (interleaved 120 KB requests, 17.6 s) cost ~0.9 ms per switch: the stick
///   tracks 2 read streams (`rd.streams`, `rd.switch`). Four in flight (27.9 s) miss the
///   tracker, 3 forward jumps and one backward jump per round: `rd.back` ≈ 27 ms.
/// - `wr.qpen`: add with 8 writes in flight (128–148 s) against 1 (96–114 s), same order on
///   the wire: ~1.6 ms per write dispatched with others queued (empirical).
/// - `flush`: 100 × (4 KB write + fdatasync) took 0.135 s, less than our 4 KB write alone, so
///   the flush itself is ~0 (0.1 ms kept so log-space calibration can move it).
pub fn sandisk() -> Model {
    Model {
        name: "sandisk-cruzer-usb2".into(),
        transport: Transport::Bot,
        lanes: 1,
        max_req: 122880,
        rd: DirCost { cmd: 0.0002, bw: 13.8e6, fwd: 0.0004, back: 0.0005, qpen: 0.0, ser: 0.0 },
        rd_switch: 0.001,
        rd_streams: 2,
        rd_thrash: 0.004,
        wr: DirCost { cmd: 0.00105, bw: 6.88e6, fwd: 0.0, back: 0.0, qpen: 0.0016, ser: 0.0 },
        flush: 0.0001,
        host_scale: 1.0,
        ftl: Some(Ftl {
            au: 4 << 20,
            open: 2,
            open_cost: 0.0192,
            merge: 0.005,
            copy_bw: 12e6,
            gc_p: 0.03,
            gc_mean: 0.5,
            gc_sigma: 1.0,
        }),
    }
}

/// MSI DATAMAG 20Gbps SSD, UAS, 1 TB. FITTED to the probe; host time dominates its
/// end-to-end runs (0.3–2.3 s) and comes from the trace.
///
/// - 4 KB writes QD1 at 97 MB/s = 42.2 µs each → `wr.cmd` = 42.2 − 4.1 (transfer) − 20
///   (`ser`, below) ≈ 18 µs.
/// - 8 MB writes at 999 MB/s: 8 requests of 1 MiB (max_sectors_kb 1024, a GUESS: typical UAS
///   bridge) whose commands overlap in the lanes, so 8.397 ms = 18 µs + 8 × (20 µs + 1 MiB /
///   `wr.bw`) → `wr.bw` ≈ 1.02 GB/s.
/// - Reads were not probed: the same costs are assumed (`rd.*` = `wr.*`), a GUESS consistent
///   with pull's 0.37 s for 169 MB (host-bound).
/// - `ser` 20 µs per request, serialized: the controller's IOPS ceiling (~50k IOPS) that deep
///   queues cannot hide. A GUESS (the drive was never probed at depth): without it, 20k
///   small reads at 32 in flight would cost almost nothing and a kernel-layout pull (one read
///   per file) would rank level with a flashcp-layout pull, against the measurements. The
///   calibration fits it.
/// - `lanes` 32: the UAS queue depth pull's 32 readers use. No FTL: an SSD with DRAM shows no
///   AU effects at these sizes. `flush` 0.1 ms (GUESS).
pub fn msi() -> Model {
    let d = DirCost { cmd: 18e-6, bw: 1.02e9, fwd: 0.0, back: 0.0, qpen: 0.0, ser: 20e-6 };
    Model {
        name: "msi-datamag-uas".into(),
        transport: Transport::Uas,
        lanes: 32,
        max_req: 1 << 20,
        rd: d,
        rd_switch: 0.0,
        rd_streams: 0,
        rd_thrash: 0.0,
        wr: d,
        flush: 0.0001,
        host_scale: 1.0,
        ftl: None,
    }
}

/// GUESS: a cheap USB 2 stick (BOT, 120 KB requests, ~4 MB/s writes, ~20 MB/s reads, 4 MiB AUs
/// with 2 open, slow random writes). Also the generic BOT preset.
pub fn usb2_cheap() -> Model {
    Model {
        name: "usb2-cheap".into(),
        transport: Transport::Bot,
        lanes: 1,
        max_req: 122880,
        rd: DirCost { cmd: 0.0005, bw: 20e6, fwd: 0.0005, back: 0.01, qpen: 0.0, ser: 0.0 },
        rd_switch: 0.001,
        rd_streams: 2,
        rd_thrash: 0.004,
        wr: DirCost { cmd: 0.0015, bw: 4e6, fwd: 0.0, back: 0.0, qpen: 0.0, ser: 0.0 },
        flush: 0.001,
        host_scale: 1.0,
        ftl: Some(Ftl {
            au: 4 << 20,
            open: 2,
            open_cost: 0.03,
            merge: 0.01,
            copy_bw: 8e6,
            gc_p: 0.03,
            gc_mean: 0.5,
            gc_sigma: 1.0,
        }),
    }
}

/// GUESS: a mid-range USB 3 stick (still BOT, as most sticks are; 512 KB requests, ~40 MB/s
/// writes, ~150 MB/s reads, 8 MiB AUs with 4 open).
pub fn usb3_midrange() -> Model {
    Model {
        name: "usb3-midrange".into(),
        transport: Transport::Bot,
        lanes: 1,
        max_req: 512 << 10,
        rd: DirCost { cmd: 0.00015, bw: 150e6, fwd: 0.0001, back: 0.002, qpen: 0.0, ser: 0.0 },
        rd_switch: 0.0002,
        rd_streams: 4,
        rd_thrash: 0.001,
        wr: DirCost { cmd: 0.0003, bw: 40e6, fwd: 0.0, back: 0.0, qpen: 0.0, ser: 0.0 },
        flush: 0.001,
        host_scale: 1.0,
        ftl: Some(Ftl {
            au: 8 << 20,
            open: 4,
            open_cost: 0.005,
            merge: 0.003,
            copy_bw: 60e6,
            gc_p: 0.02,
            gc_mean: 0.2,
            gc_sigma: 1.0,
        }),
    }
}

/// GUESS: a USB 3 UAS drive of no particular kind (the generic UAS preset): 32 lanes, 1 MiB
/// requests, 60 µs commands, 400 MB/s.
pub fn usb3_uas_generic() -> Model {
    let d = DirCost { cmd: 60e-6, bw: 400e6, fwd: 0.0, back: 0.0, qpen: 0.0, ser: 50e-6 };
    Model {
        name: "uas-generic".into(),
        transport: Transport::Uas,
        lanes: 32,
        max_req: 1 << 20,
        rd: d,
        rd_switch: 0.0,
        rd_streams: 0,
        rd_thrash: 0.0,
        wr: d,
        flush: 0.0005,
        host_scale: 1.0,
        ftl: None,
    }
}

/// GUESS: an NVMe SSD in a 10 Gb/s USB enclosure (UAS, 32 lanes, ~950 MB/s, 25 µs commands).
pub fn nvme_usb() -> Model {
    let d = DirCost { cmd: 25e-6, bw: 950e6, fwd: 0.0, back: 0.0, qpen: 0.0, ser: 10e-6 };
    Model {
        name: "nvme-usb".into(),
        transport: Transport::Uas,
        lanes: 32,
        max_req: 1 << 20,
        rd: d,
        rd_switch: 0.0,
        rd_streams: 0,
        rd_thrash: 0.0,
        wr: DirCost { bw: 900e6, ..d },
        flush: 0.0003,
        host_scale: 1.0,
        ftl: None,
    }
}

/// A model from a cached device profile (`flashcp probe`, add's write probe, pull's read probe).
/// Unknowns come from the transport's [`generic`] preset.
///
/// The profile's write model is "bytes / seq_write_bps + skips × write_skip_s" with
/// seq_write_bps measured with big requests, i.e. already including one command per
/// `max_req`. On BOT: `wr.cmd = write_skip_s` and `1 / wr.bw = 1 / seq_write_bps − wr.cmd /
/// max_req`; on UAS the commands overlap, so `wr.bw = seq_write_bps`. Reads: `rd.bw =
/// seq_read_bps`, `rd.cmd = read_cmd_s`. A measured `write_qd` > 1 on UAS sets the lanes.
pub fn from_devprofile(p: &DevProfile, name: &str) -> Model {
    let t = if p.transport.as_deref() == Some("uas") { Transport::Uas } else { Transport::Bot };
    let mut m = generic(t);
    m.name = name.to_string();
    if let Some(s) = p.write_skip_s {
        m.wr.cmd = s;
    }
    if let Some(bps) = p.seq_write_bps {
        let inv = 1.0 / bps - m.wr.cmd / m.max_req as f64;
        m.wr.bw = if t == Transport::Uas || inv <= 0.0 { bps } else { 1.0 / inv };
    }
    if let Some(b) = p.seq_read_bps {
        m.rd.bw = b;
    }
    if let Some(c) = p.read_cmd_s {
        m.rd.cmd = c;
    }
    if t == Transport::Uas
        && let Some(q) = p.write_qd.filter(|&q| q > 1)
    {
        m.lanes = q;
    }
    m
}

/// A builtin name, a model file (`flashcp-simmodel 1`), or a device profile file
/// (`flashcp-devprofile 1`, e.g. a `~/.cache/flashcp/devices/` entry or a `src/sim/devices/*.dev`
/// file, whose `sim.name` / `sim.max_sectors_kb` are used when present).
pub fn resolve(spec: &str) -> Result<Model> {
    if let Some(m) = builtin(spec) {
        return Ok(m);
    }
    let path = Path::new(spec);
    if !path.exists() {
        bail!("unknown device model {spec:?}: not a builtin ({}) nor a file", NAMES.join(", "));
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let first = text.lines().next().unwrap_or("").trim();
    if first.starts_with("flashcp-simmodel") {
        return Model::parse(&text).with_context(|| format!("model file {}", path.display()));
    }
    let parsed = crate::sim::device::SimDevice::parse(&text);
    if let Err(e) = &parsed
        && first.starts_with("flashcp-devprofile")
    {
        bail!("device file {}: {e}", path.display());
    }
    if let Ok(d) = parsed {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "device".into());
        let has_name = text.lines().any(|l| l.trim_start().starts_with("sim.name="));
        return Ok(from_sim_device(&d, if has_name { &d.name } else { &stem }));
    }
    bail!("{}: neither a flashcp-simmodel nor a flashcp-devprofile file", path.display())
}

/// A model from a sim-device file: [`from_devprofile`] plus its `sim.max_sectors_kb`.
pub fn from_sim_device(d: &crate::sim::device::SimDevice, name: &str) -> Model {
    {
        let mut m = from_devprofile(&d.profile, name);
        if let Some(kb) = d.max_sectors_kb.filter(|&k| k >= 1) {
            let old = m.max_req;
            m.max_req = kb as u64 * 1024;
            // keep the measured big-request rate: redo the BOT bandwidth for the new size
            if m.transport == Transport::Bot
                && old != m.max_req
                && let Some(bps) = d.profile.seq_write_bps
            {
                let inv = 1.0 / bps - m.wr.cmd / m.max_req as f64;
                m.wr.bw = if inv > 0.0 { 1.0 / inv } else { bps };
            }
        }
        m
    }
}

/// Kingston DataTraveler 3.0 128 GB, USB 3.0 5 Gbps, usb-storage (BOT), max request 1 MB.
/// PROBE-ONLY: built from what flashcp cached for the stick (`flashcp probe`'s writes, pull's
/// read micro-probe; `devices/kingston.dev`) on the generic BOT preset, exactly the model a
/// user gets from `--profile ~/.cache/flashcp/devices/<key>`. It was used for the blind
/// prediction before any end-to-end run; the end-to-end runs are in scenarios.txt, and
/// `flashcp sim calibrate --drive kingston` fits it (see SIMULATION.md).
pub fn kingston() -> Model {
    let d = crate::sim::device::SimDevice::parse(include_str!("devices/kingston.dev"))
        .expect("the shipped kingston.dev is valid");
    from_sim_device(&d, "kingston-dt3-usb3")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::model::simulate_det;
    use crate::sim::replay::Trace;

    #[test]
    fn builtins_round_trip_and_aliases() {
        for n in builtin_names() {
            let m = builtin(n).unwrap();
            assert_eq!(&m.name, n);
            assert_eq!(Model::parse(&m.render()).unwrap(), m, "{n}");
        }
        assert_eq!(builtin("sandisk"), builtin("sandisk-cruzer-usb2"));
        assert_eq!(builtin("msi"), builtin("msi-datamag-uas"));
        assert!(builtin("nope").is_none());
    }

    fn rate(m: &Model, kind: &str, bs: u64, bytes: u64) -> f64 {
        let t = Trace::micro(kind, bs, 1, bytes).unwrap();
        bytes as f64 / simulate_det(&t, m).secs / 1e6
    }

    #[test]
    fn sandisk_meets_its_probe() {
        let m = sandisk();
        for (bs, mbps) in [(4096u64, 2.5), (65536, 6.2), (1 << 20, 6.4), (8 << 20, 6.5)] {
            let r = rate(&m, "seqw", bs, 64 << 20);
            assert!((r / mbps - 1.0).abs() < 0.08, "seq {bs}: {r} vs {mbps}");
        }
        let iops = 512.0 / simulate_det(&Trace::micro("randw", 4096, 1, 512 * 4096).unwrap(), &m).secs;
        assert!((iops / 48.0 - 1.0).abs() < 0.1, "{iops}");
    }

    #[test]
    fn msi_meets_its_probe() {
        let m = msi();
        let r8 = rate(&m, "seqw", 8 << 20, 1 << 30);
        let r4 = rate(&m, "seqw", 4096, 256 << 20);
        assert!((r8 / 999.0 - 1.0).abs() < 0.03, "{r8}");
        assert!((r4 / 97.0 - 1.0).abs() < 0.03, "{r4}");
    }

    #[test]
    fn devprofile_conversion_and_resolve() {
        let dev = concat!(env!("CARGO_MANIFEST_DIR"), "/src/sim/devices/sandisk.dev");
        let m = resolve(dev).unwrap();
        assert_eq!(m.name, "sandisk-cruzer-usb2");
        assert_eq!((m.transport, m.max_req), (Transport::Bot, 122880));
        assert!((m.wr.cmd - 0.00105).abs() < 1e-12);
        assert!((m.wr.bw / 6.88e6 - 1.0).abs() < 0.01, "{}", m.wr.bw);
        assert!((m.rd.bw - 13.8e6).abs() < 1.0);
        let u = from_devprofile(
            &DevProfile {
                transport: Some("uas".into()),
                seq_write_bps: Some(5e8),
                write_qd: Some(8),
                ..Default::default()
            },
            "x",
        );
        assert_eq!((u.transport, u.lanes, u.wr.bw), (Transport::Uas, 8, 5e8));
        assert!(resolve("sandisk").is_ok());
        assert!(resolve("/nonexistent/model").is_err());
    }
}
