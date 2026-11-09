//! Device probe. Everything goes through a temporary file on the mounted filesystem, so the
//! probe never touches raw blocks and never needs root. O_DIRECT bypasses the page cache so
//! we measure the drive, not RAM.

use crate::util::{AlignedBuf, Rng, human_bytes, syncfs};
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct DeviceInfo {
    pub sysfs: Option<PathBuf>,
    pub model: Option<String>,
    pub transport: Option<String>,
    pub usb_speed_mbps: Option<String>,
    pub max_sectors_kb: Option<String>,
    pub rotational: Option<String>,
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

pub fn device_info(path: &Path) -> DeviceInfo {
    let Ok(md) = fs::metadata(path) else { return DeviceInfo::default() };
    device_info_of(md.dev())
}

/// [`device_info`] of a block device number (st_rdev of a device node, or st_dev of a file).
pub fn device_info_of(dev: u64) -> DeviceInfo {
    let mut info = DeviceInfo::default();
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    let Ok(mut p) = fs::canonicalize(format!("/sys/dev/block/{maj}:{min}")) else { return info };
    if p.join("partition").exists() {
        p.pop();
    }
    info.model = read_trim(&p.join("device/model"))
        .map(|m| format!("{} {m}", read_trim(&p.join("device/vendor")).unwrap_or_default()));
    info.max_sectors_kb = read_trim(&p.join("queue/max_sectors_kb"));
    info.rotational = read_trim(&p.join("queue/rotational"));
    // Walk up the device chain: the USB interface tells us the driver (uas vs usb-storage),
    // the USB device tells us the negotiated link speed.
    let mut a = p.clone();
    while a.pop() {
        if info.transport.is_none()
            && let Ok(d) = fs::read_link(a.join("driver"))
        {
            let name = d.file_name().unwrap().to_string_lossy().to_string();
            if name == "uas" || name == "usb-storage" {
                info.transport = Some(name);
            }
        }
        if info.usb_speed_mbps.is_none() && a.join("idVendor").exists() {
            info.usb_speed_mbps = read_trim(&a.join("speed"));
        }
    }
    info.sysfs = Some(p);
    info
}

#[derive(Debug)]
pub struct ProbeResult {
    pub info: DeviceInfo,
    pub direct_io: bool,
    /// (block size, MB/s)
    pub seq_write: Vec<(usize, f64)>,
    pub rand4k_qd1_iops: f64,
    pub rand4k_qd8_iops: f64,
    pub small_files_per_sec: f64,
    pub flush_ms_median: f64,
    /// Device-profile cache entry the measurements were saved to (see [`crate::devprofile`]).
    pub saved_as: Option<String>,
}

/// What `flashcp probe` measured, as a device profile. None for buffered I/O (that measures
/// the page cache, not the device).
pub fn profile_of(r: &ProbeResult) -> Option<crate::devprofile::DevProfile> {
    if !r.direct_io {
        return None;
    }
    let big = r.seq_write.iter().filter(|x| x.0 >= 1 << 20).map(|x| x.1 * 1e6).fold(0.0, f64::max);
    let small = r.seq_write.iter().find(|x| x.0 == 4096).map(|x| x.1 * 1e6).filter(|v| *v > 0.0);
    let qd = |a: f64, b: f64| if a > 0.0 && b > 1.5 * a { 8 } else { 1 };
    Some(crate::devprofile::DevProfile {
        seq_write_bps: (big > 0.0).then_some(big),
        // 4 KB requests back to back cost 4096/small each, the same bytes inside big requests
        // 4096/big: the difference is what one more request costs in an ascending stream.
        write_skip_s: small.filter(|_| big > 0.0).map(|s| (4096.0 / s - 4096.0 / big).max(0.0)),
        // Only QD1 and QD8 are measured: 8 if parallel writes clearly help, else 1.
        write_qd: Some(qd(r.rand4k_qd1_iops, r.rand4k_qd8_iops)),
        transport: Some(r.info.transport.clone().unwrap_or_else(|| "other".into())),
        ..Default::default()
    })
}

fn open_probe(path: &Path) -> Result<(File, bool)> {
    let direct =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).custom_flags(libc::O_DIRECT).open(path);
    match direct {
        Ok(f) => Ok((f, true)),
        Err(_) => Ok((OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?, false)),
    }
}

pub fn probe(dir: &Path, size: u64) -> Result<ProbeResult> {
    let info = device_info(dir);
    let work = dir.join(".flashcp-probe");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let r = run(&work, size, info);
    let _ = fs::remove_dir_all(&work);
    let _ = syncfs(dir);
    let mut r = r?;
    // Cache what was measured for this disk (merged; never fails the probe).
    if let (Some(p), Ok(md)) = (profile_of(&r), fs::metadata(dir)) {
        crate::devprofile::save(md.dev(), &p);
        r.saved_as = crate::devprofile::device_key(md.dev());
    }
    Ok(r)
}

fn run(work: &Path, size: u64, info: DeviceInfo) -> Result<ProbeResult> {
    let fpath = work.join("probe.bin");
    let (f, direct_io) = open_probe(&fpath).context("create probe file")?;
    let mut rng = Rng(0xF1A5);

    // 1. Sequential writes at increasing block sizes.
    let mut seq_write = Vec::new();
    for &bs in &[4usize << 10, 64 << 10, 1 << 20, 8 << 20] {
        let cap = match bs {
            b if b <= 4096 => 32 << 20,
            b if b <= 65536 => 128 << 20,
            _ => size,
        };
        let total = cap.min(size) / bs as u64 * bs as u64;
        let mut buf = AlignedBuf::new(bs);
        rng.fill(buf.as_mut());
        let t = Instant::now();
        let mut off = 0u64;
        while off < total {
            f.write_all_at(buf.as_ref(), off)?;
            off += bs as u64;
            if t.elapsed() > Duration::from_secs(10) {
                break;
            }
        }
        f.sync_data()?;
        let mbps = off as f64 / 1e6 / t.elapsed().as_secs_f64();
        seq_write.push((bs, mbps));
    }
    // Make sure the whole file exists for the random tests.
    f.set_len(size)?;
    let blocks = size / 4096;

    // 2. Random 4K writes, queue depth 1.
    let mut buf = AlignedBuf::new(4096);
    rng.fill(buf.as_mut());
    let t = Instant::now();
    let mut n = 0u64;
    while n < 20_000 && t.elapsed() < Duration::from_secs(3) {
        f.write_all_at(buf.as_ref(), rng.range(0, blocks - 1) * 4096)?;
        n += 1;
    }
    f.sync_data()?;
    let rand4k_qd1_iops = n as f64 / t.elapsed().as_secs_f64();

    // 3. Random 4K writes, 8 threads in parallel (≈ queue depth 8; only UAS can really use it).
    let t = Instant::now();
    let done: u64 = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8u64)
            .map(|k| {
                let f = &f;
                s.spawn(move || {
                    let mut rng = Rng(k * 7919 + 1);
                    let mut b = AlignedBuf::new(4096);
                    rng.fill(b.as_mut());
                    let mut n = 0u64;
                    while n < 10_000 && t.elapsed() < Duration::from_secs(3) {
                        if f.write_all_at(b.as_ref(), rng.range(0, blocks - 1) * 4096).is_err() {
                            break;
                        }
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    f.sync_data()?;
    let rand4k_qd8_iops = done as f64 / t.elapsed().as_secs_f64();

    // 4. Flush latency: 4K write + fdatasync, the cost a "safe" copier pays per file.
    let mut lat = Vec::new();
    for i in 0..100u64 {
        let t = Instant::now();
        f.write_all_at(buf.as_ref(), i * 4096)?;
        f.sync_data()?;
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let flush_ms_median = lat[lat.len() / 2];
    drop(f);
    fs::remove_file(&fpath)?;

    // 5. Small-file creation through the normal filesystem path, then one flush.
    let sdir = work.join("small");
    fs::create_dir_all(&sdir)?;
    let data = vec![0x5Au8; 4096];
    let t = Instant::now();
    let count = 2000;
    for i in 0..count {
        fs::write(sdir.join(format!("s{i:05}")), &data)?;
    }
    syncfs(work)?;
    let small_files_per_sec = count as f64 / t.elapsed().as_secs_f64();

    Ok(ProbeResult {
        info,
        direct_io,
        seq_write,
        rand4k_qd1_iops,
        rand4k_qd8_iops,
        small_files_per_sec,
        flush_ms_median,
        saved_as: None,
    })
}

pub fn report(r: &ProbeResult) {
    let i = &r.info;
    println!("── device ─────────────────────────────────────");
    println!("  model         {}", i.model.as_deref().unwrap_or("?"));
    println!("  transport     {}", i.transport.as_deref().unwrap_or("? (not USB, or unknown)"));
    println!("  USB link      {} Mb/s", i.usb_speed_mbps.as_deref().unwrap_or("?"));
    println!("  max request   {} KB", i.max_sectors_kb.as_deref().unwrap_or("?"));
    println!("  direct I/O    {}", if r.direct_io { "yes" } else { "no (buffered + fdatasync)" });
    println!("── measurements ───────────────────────────────");
    for (bs, mbps) in &r.seq_write {
        println!("  seq write {:>6}   {:>8.1} MB/s", fmt_bs(*bs), mbps);
    }
    let best_seq = r.seq_write.iter().map(|x| x.1).fold(0.0, f64::max);
    println!("  rand 4K QD1      {:>8.0} IOPS  ({:.1} MB/s)", r.rand4k_qd1_iops, r.rand4k_qd1_iops * 4096.0 / 1e6);
    println!("  rand 4K QD8      {:>8.0} IOPS  ({:.1} MB/s)", r.rand4k_qd8_iops, r.rand4k_qd8_iops * 4096.0 / 1e6);
    println!("  flush latency    {:>8.2} ms (median)", r.flush_ms_median);
    println!("  small files      {:>8.0} files/s (4 KB each, one flush at end)", r.small_files_per_sec);
    println!("── what this means ────────────────────────────");
    let small_mbps = r.small_files_per_sec * 4096.0 / 1e6;
    println!(
        "  small-file throughput is {:.0}× slower than sequential ({} vs {}/s)",
        best_seq / small_mbps.max(0.001),
        human_bytes(small_mbps * 1e6),
        human_bytes(best_seq * 1e6)
    );
    let best_bs = r.seq_write.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap().0;
    println!("  best write size: {} → use at least that as the I/O chunk", fmt_bs(best_bs));
    if r.rand4k_qd8_iops > r.rand4k_qd1_iops * 1.5 {
        println!("  parallel writes help ({:.1}×) → try several writer threads", r.rand4k_qd8_iops / r.rand4k_qd1_iops);
    } else {
        println!("  parallel writes don't help → one ordered writer is optimal");
    }
    println!(
        "  a copier that flushes per file would cap at ~{:.0} files/s; never do that",
        1000.0 / r.flush_ms_median.max(0.001)
    );
    if let (Some(key), Some(p)) = (&r.saved_as, profile_of(r)) {
        let (bps, skip) = p.write_cost();
        println!(
            "  saved device profile {key}: {}/s sequential, {:.2} ms per extra write request, {} in flight",
            human_bytes(bps),
            skip * 1e3,
            p.write_qd.unwrap_or(1)
        );
    }
}

fn fmt_bs(bs: usize) -> String {
    if bs >= 1 << 20 { format!("{}M", bs >> 20) } else { format!("{}K", bs >> 10) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(direct_io: bool) -> ProbeResult {
        // The SanDisk Cruzer Blade (USB 2, BOT) reference numbers.
        ProbeResult {
            info: DeviceInfo { transport: Some("usb-storage".into()), ..Default::default() },
            direct_io,
            seq_write: vec![(4 << 10, 2.5), (64 << 10, 5.0), (1 << 20, 6.3), (8 << 20, 6.5)],
            rand4k_qd1_iops: 48.0,
            rand4k_qd8_iops: 357.0,
            small_files_per_sec: 100.0,
            flush_ms_median: 20.0,
            saved_as: None,
        }
    }

    #[test]
    fn profile_from_probe_numbers() {
        let p = profile_of(&result(true)).unwrap();
        assert_eq!(p.seq_write_bps, Some(6.5e6));
        let skip = p.write_skip_s.unwrap();
        assert!((skip - (4096.0 / 2.5e6 - 4096.0 / 6.5e6)).abs() < 1e-12, "{skip}");
        assert_eq!(p.write_qd, Some(8));
        assert_eq!(p.transport.as_deref(), Some("usb-storage"));
        assert_eq!((p.seq_read_bps, p.read_cmd_s), (None, None), "probe measures no reads");
        let mut r = result(true);
        r.rand4k_qd8_iops = 60.0;
        assert_eq!(profile_of(&r).unwrap().write_qd, Some(1), "parallel writes do not help");
        assert_eq!(profile_of(&result(false)), None, "buffered I/O measures the page cache");
    }
}
