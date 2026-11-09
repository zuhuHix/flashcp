//! Per-device performance profile (sequential bandwidth, per-request costs), measured once per
//! device and cached under $XDG_CACHE_HOME/flashcp/devices, keyed by the device's serial / WWN.
//!
//! Every measurement flashcp makes (`flashcp probe`, the write micro-probe of `add`, the read
//! micro-probe of `pull`) is merged into one small text file per disk, so later runs decide
//! from the device without measuring again. The cache is purely an optimization: every error
//! (no HOME, read-only cache dir, a garbled file) is ignored and the caller uses defaults or
//! measures. Devices without a stable key (image files, loop devices, VM disks) are never
//! cached.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Used when a device has no profile (and for image files): a modest USB 3 stick. Only the
/// ratio matters to the cluster optimizer; the gap policy has its own image default.
pub const DEFAULT_SEQ_WRITE_BPS: f64 = 20e6;
pub const DEFAULT_WRITE_SKIP_S: f64 = 1e-3;
/// Writes in flight for scattered spans when nothing was measured (the old fixed value).
pub const DEFAULT_WRITE_QD: usize = 8;
/// Unmeasured writes in flight on USB Bulk-Only Transport (usb-storage): BOT runs one command
/// at a time, so a deeper queue only reorders the stream; keep it strictly ascending (2 and 8
/// in flight measured no better on a USB 2 stick, see add::probe_in_situ).
pub const BOT_WRITE_QD: usize = 1;
/// Upper bound on any write_qd we accept from the cache or a measurement.
pub const MAX_WRITE_QD: usize = 64;

const MAGIC: &str = "flashcp-devprofile 1";

/// What flashcp knows about one disk. Every field is optional: each probe fills only what it
/// measured, and [`save`] merges into what is already cached.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DevProfile {
    /// Sequential write bandwidth with big (≥ 1 MB) requests, bytes/s.
    pub seq_write_bps: Option<f64>,
    /// Extra cost of one more request (one forward skip) in an ascending write stream, seconds.
    /// A gap of g bytes is worth zero-filling iff g / seq_write_bps < write_skip_s.
    pub write_skip_s: Option<f64>,
    /// Best number of writes in flight for scattered small writes.
    pub write_qd: Option<usize>,
    /// Sequential read bandwidth, bytes/s.
    pub seq_read_bps: Option<f64>,
    /// Extra cost of one more request in a forward read stream, seconds.
    pub read_cmd_s: Option<f64>,
    /// "uas", "usb-storage", or "other".
    pub transport: Option<String>,
    /// Gap policies `add` measured in situ, per pattern class ([`pattern_key`]).
    pub patterns: BTreeMap<String, PatternChoice>,
}

/// How `add`'s data phase treats the cluster slack and free clusters between the spans it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapMode {
    /// Every gap is skipped (a new request per span).
    Skip,
    /// Every gap of free clusters is zero-filled: one sequential stream.
    Fill,
    /// Each file's write is padded to a multiple of this many bytes, the rest is skipped.
    Pad(u64),
}

/// A gap policy measured in situ for one pattern class: the mode and the writes in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternChoice {
    pub mode: GapMode,
    pub qd: usize,
}

impl PatternChoice {
    fn render(&self) -> String {
        let m = match self.mode {
            GapMode::Skip => "skip".to_string(),
            GapMode::Fill => "fill".to_string(),
            GapMode::Pad(p) => format!("pad{p}"),
        };
        format!("{m}:{}", self.qd)
    }
    fn parse(v: &str) -> Option<PatternChoice> {
        let (m, q) = v.split_once(':')?;
        let mode = match m {
            "skip" => GapMode::Skip,
            "fill" => GapMode::Fill,
            _ => {
                let p: u64 = m.strip_prefix("pad")?.parse().ok()?;
                GapMode::Pad(Some(p).filter(|p| p.is_multiple_of(4096) && (4096..=1 << 20).contains(p))?)
            }
        };
        let qd = q.parse().ok().filter(|q| (1..=MAX_WRITE_QD).contains(q))?;
        Some(PatternChoice { mode, qd })
    }
}

/// The pattern class of an add: cluster size and the median size of the files it writes,
/// rounded up to a power of two (at least 4 KB), e.g. `c32k-m8k`.
pub fn pattern_key(cluster: u64, median_file: u64) -> String {
    let m = median_file.clamp(4096, 1 << 40).next_power_of_two();
    let k = |b: u64| if b >= 1 << 20 { format!("{}m", b >> 20) } else { format!("{}k", b >> 10) };
    let c = if cluster < 1024 { format!("{cluster}b") } else { k(cluster) };
    format!("c{c}-m{}", k(m))
}

/// Whether `transport` is USB Bulk-Only Transport (the usb-storage driver).
pub fn is_bot(transport: Option<&str>) -> bool {
    transport == Some("usb-storage")
}
impl DevProfile {
    /// Fields of `newer` win; fields it lacks keep their current value.
    pub fn merge(&mut self, newer: &DevProfile) {
        fn take<T: Clone>(a: &mut Option<T>, b: &Option<T>) {
            if b.is_some() {
                *a = b.clone();
            }
        }
        take(&mut self.seq_write_bps, &newer.seq_write_bps);
        take(&mut self.write_skip_s, &newer.write_skip_s);
        take(&mut self.write_qd, &newer.write_qd);
        take(&mut self.seq_read_bps, &newer.seq_read_bps);
        take(&mut self.read_cmd_s, &newer.read_cmd_s);
        take(&mut self.transport, &newer.transport);
        for (k, v) in &newer.patterns {
            self.patterns.insert(k.clone(), *v);
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == DevProfile::default()
    }

    /// Versioned `key=value` text; f64 Display round-trips exactly.
    pub fn render(&self) -> String {
        let mut s = format!("{MAGIC}\n");
        let mut kv = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                s.push_str(&format!("{k}={v}\n"));
            }
        };
        kv("seq_write_bps", self.seq_write_bps.map(|v| v.to_string()));
        kv("write_skip_s", self.write_skip_s.map(|v| v.to_string()));
        kv("write_qd", self.write_qd.map(|v| v.to_string()));
        kv("seq_read_bps", self.seq_read_bps.map(|v| v.to_string()));
        kv("read_cmd_s", self.read_cmd_s.map(|v| v.to_string()));
        kv("transport", self.transport.clone());
        for (k, v) in &self.patterns {
            kv(&format!("pattern.{k}"), Some(v.render()));
        }
        s
    }

    /// Parses [`render`]'s format. None for another version; unknown keys and implausible
    /// values (non-finite, negative, out of range) are dropped field by field.
    pub fn parse(text: &str) -> Option<DevProfile> {
        let mut lines = text.lines();
        if lines.next()?.trim() != MAGIC {
            return None;
        }
        let pos = |v: &str, max: f64| v.parse::<f64>().ok().filter(|x| x.is_finite() && *x > 0.0 && *x <= max);
        let mut p = DevProfile::default();
        for l in lines {
            let Some((k, v)) = l.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "seq_write_bps" => p.seq_write_bps = pos(v, 1e12),
                "write_skip_s" => {
                    p.write_skip_s = v.parse::<f64>().ok().filter(|x| x.is_finite() && (0.0..=1.0).contains(x))
                }
                "write_qd" => p.write_qd = v.parse().ok().filter(|q| (1..=MAX_WRITE_QD).contains(q)),
                "seq_read_bps" => p.seq_read_bps = pos(v, 1e12),
                "read_cmd_s" => {
                    p.read_cmd_s = v.parse::<f64>().ok().filter(|x| x.is_finite() && (0.0..=1.0).contains(x))
                }
                "transport" if !v.is_empty() && v.len() <= 32 => p.transport = Some(v.to_string()),
                k if k.starts_with("pattern.") => {
                    let name = &k["pattern.".len()..];
                    let ok = !name.is_empty() && name.len() <= 32 && sanitize(name) == name;
                    if let Some(c) = PatternChoice::parse(v).filter(|_| ok) {
                        p.patterns.insert(name.to_string(), c);
                    }
                }
                _ => {}
            }
        }
        Some(p)
    }

    /// (sequential write bytes/s, seconds per skip), measured or default.
    pub fn write_cost(&self) -> (f64, f64) {
        (self.seq_write_bps.unwrap_or(DEFAULT_SEQ_WRITE_BPS), self.write_skip_s.unwrap_or(DEFAULT_WRITE_SKIP_S))
    }

    /// Both numbers the write cost model needs were measured.
    pub fn has_write_model(&self) -> bool {
        self.seq_write_bps.is_some() && self.write_skip_s.is_some()
    }
}

/// `$XDG_CACHE_HOME/flashcp/devices`, else `$HOME/.cache/flashcp/devices`. A relative
/// XDG_CACHE_HOME is invalid per the XDG spec and ignored.
fn cache_dir_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let base = match xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(x) => x,
        None => PathBuf::from(home.filter(|h| !h.is_empty())?).join(".cache"),
    };
    Some(base.join("flashcp").join("devices"))
}

pub fn cache_dir() -> Option<PathBuf> {
    cache_dir_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

/// Cached profile `key` in `dir` (None if absent or unreadable).
pub fn load_from(dir: &Path, key: &str) -> Option<DevProfile> {
    // A profile is a few hundred bytes; refuse anything big without reading it.
    let path = dir.join(key);
    if fs::metadata(&path).ok()?.len() > 4096 {
        return None;
    }
    DevProfile::parse(&fs::read_to_string(path).ok()?)
}

/// Merges `p` into the cached profile `key` in `dir`, written atomically (tmp + rename), so a
/// concurrent reader sees the old or the new file, never half of one.
pub fn save_to(dir: &Path, key: &str, p: &DevProfile) -> std::io::Result<()> {
    if p.is_empty() {
        return Ok(());
    }
    let mut all = load_from(dir, key).unwrap_or_default();
    all.merge(p);
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{key}.{}.tmp", std::process::id()));
    let r = fs::write(&tmp, all.render()).and_then(|_| fs::rename(&tmp, dir.join(key)));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Device number to look profiles up by: st_rdev of a block device. Regular files (images)
/// have none, so they are never cached, even though the disk they live on has a key.
pub fn block_dev_of(f: &fs::File) -> Option<u64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let md = f.metadata().ok()?;
    md.file_type().is_block_device().then(|| md.rdev())
}

/// The cached profile of the disk holding block device `dev` (see [`device_key`]).
pub fn load(dev: u64) -> Option<DevProfile> {
    load_from(&cache_dir()?, &device_key(dev)?)
}

/// Merges `p` into the cache of the disk holding `dev`. Silently does nothing for a device
/// without a key, and ignores every I/O error: caching never fails a command.
pub fn save(dev: u64, p: &DevProfile) {
    if let (Some(dir), Some(key)) = (cache_dir(), device_key(dev)) {
        let _ = save_to(&dir, &key, p);
    }
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Keeps `[A-Za-z0-9._-]`, maps everything else to `_` (a file name).
fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect()
}

/// A stable identity of the whole disk holding block device `dev` (st_rdev of a device node, or
/// st_dev of a file on it; a partition maps to its disk), that survives replugging and renaming
/// (sdb → sdc): the nearest USB device's vendor, product and serial (`usb-0781-5567-4C53…`),
/// else the SCSI/NVMe WWID (`wwid-…`). USB comes first because many USB bridges report one
/// fixed, non-unique WWID. None when the device has neither (a loop device, a VM disk, …).
pub fn device_key(dev: u64) -> Option<String> {
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    let mut p = fs::canonicalize(format!("/sys/dev/block/{maj}:{min}")).ok()?;
    if p.join("partition").exists() {
        p.pop();
    }
    let mut a = p.clone();
    while a.pop() {
        if let (Some(v), Some(pr), Some(s)) =
            (read_trim(&a.join("idVendor")), read_trim(&a.join("idProduct")), read_trim(&a.join("serial")))
        {
            // Root hubs carry a "serial" too (the PCI address); they have no parent USB device.
            if !a.join("../idVendor").exists() && a.file_name()?.to_string_lossy().starts_with("usb") {
                break;
            }
            return Some(sanitize(&format!("usb-{v}-{pr}-{s}")));
        }
    }
    let wwid = read_trim(&p.join("device/wwid")).or_else(|| read_trim(&p.join("wwid")))?;
    Some(sanitize(&format!("wwid-{wwid}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_chars_only() {
        assert_eq!(sanitize("t10.SanDisk\0Cruzer Blade/x"), "t10.SanDisk_Cruzer_Blade_x");
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flashcp-devprofile-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn cache_dir_prefers_absolute_xdg_then_home() {
        let d = |x: Option<&str>, h: Option<&str>| cache_dir_from(x.map(Into::into), h.map(Into::into));
        assert_eq!(d(Some("/x/c"), Some("/home/u")), Some(PathBuf::from("/x/c/flashcp/devices")));
        assert_eq!(d(Some("rel"), Some("/home/u")), Some(PathBuf::from("/home/u/.cache/flashcp/devices")));
        assert_eq!(d(None, Some("/home/u")), Some(PathBuf::from("/home/u/.cache/flashcp/devices")));
        assert_eq!(d(None, None), None);
        assert_eq!(d(None, Some("")), None);
    }

    #[test]
    fn profile_round_trips_exactly() {
        let p = DevProfile {
            seq_write_bps: Some(6_512_345.678),
            write_skip_s: Some(0.000_123_456_789),
            write_qd: Some(8),
            seq_read_bps: Some(1e9 / 3.0),
            read_cmd_s: Some(0.0),
            transport: Some("usb-storage".into()),
            patterns: [
                ("c32k-m8k".to_string(), PatternChoice { mode: GapMode::Pad(16384), qd: 2 }),
                ("c4k-m4k".to_string(), PatternChoice { mode: GapMode::Skip, qd: 1 }),
                ("c128k-m1m".to_string(), PatternChoice { mode: GapMode::Fill, qd: 8 }),
            ]
            .into(),
        };
        assert_eq!(DevProfile::parse(&p.render()), Some(p));
        assert_eq!(DevProfile::parse(&DevProfile::default().render()), Some(DevProfile::default()));
    }

    #[test]
    fn parse_rejects_other_versions_and_drops_bad_fields() {
        assert_eq!(DevProfile::parse("flashcp-devprofile 2\nwrite_qd=8\n"), None);
        assert_eq!(DevProfile::parse(""), None);
        let p = DevProfile::parse(
            "flashcp-devprofile 1\nseq_write_bps=-5\nwrite_skip_s=NaN\nwrite_qd=0\nseq_read_bps=inf\n\
             read_cmd_s=7\njunk\nfuture_key=1\ntransport=uas\npattern.a=pad5:1\npattern.b=skip:0\n\
             pattern.c=fill\npattern.d/e=skip:1\npattern.=skip:1\npattern.f=pad1048576:64\n",
        )
        .unwrap();
        let f = PatternChoice { mode: GapMode::Pad(1 << 20), qd: 64 };
        assert_eq!(
            p,
            DevProfile { transport: Some("uas".into()), patterns: [("f".into(), f)].into(), ..Default::default() }
        );
    }

    #[test]
    fn pattern_keys_bucket_cluster_and_median() {
        assert_eq!(pattern_key(32768, 8000), "c32k-m8k");
        assert_eq!(pattern_key(32768, 8193), "c32k-m16k");
        assert_eq!(pattern_key(32768, 0), "c32k-m4k");
        assert_eq!(pattern_key(512, 100), "c512b-m4k");
        assert_eq!(pattern_key(1 << 20, 3 << 20), "c1m-m4m");
        assert!(is_bot(Some("usb-storage")) && !is_bot(Some("uas")) && !is_bot(None));
    }

    #[test]
    fn save_merges_and_never_erases_other_fields() {
        let dir = tmp("merge");
        let key = "usb-0781-5567-TEST";
        assert_eq!(load_from(&dir, key), None);
        let w = DevProfile {
            seq_write_bps: Some(6.5e6),
            write_skip_s: Some(0.002),
            write_qd: Some(8),
            transport: Some("usb-storage".into()),
            ..Default::default()
        };
        save_to(&dir, key, &w).unwrap();
        assert_eq!(load_from(&dir, key), Some(w.clone()));
        // A pull-only measurement adds read fields and keeps the write fields.
        let r = DevProfile { seq_read_bps: Some(30e6), read_cmd_s: Some(0.001), ..Default::default() };
        save_to(&dir, key, &r).unwrap();
        let mut both = w.clone();
        both.seq_read_bps = r.seq_read_bps;
        both.read_cmd_s = r.read_cmd_s;
        assert_eq!(load_from(&dir, key), Some(both.clone()));
        // A newer write measurement replaces only what it measured.
        save_to(&dir, key, &DevProfile { write_qd: Some(32), ..Default::default() }).unwrap();
        both.write_qd = Some(32);
        assert_eq!(load_from(&dir, key), Some(both));
        // No temp files left behind; a garbled file reads as nothing and is replaced cleanly.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::write(dir.join(key), "garbage").unwrap();
        assert_eq!(load_from(&dir, key), None);
        save_to(&dir, key, &r).unwrap();
        assert_eq!(load_from(&dir, key), Some(r));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_errors_are_reported_to_the_caller_only() {
        // save_to returns the error (save() drops it); a file where the directory should be.
        let dir = tmp("err");
        fs::write(&dir, "not a dir").unwrap();
        let p = DevProfile { write_qd: Some(4), ..Default::default() };
        assert!(save_to(&dir, "k", &p).is_err());
        assert_eq!(load_from(&dir, "k"), None);
        fs::remove_file(&dir).unwrap();
    }
}
