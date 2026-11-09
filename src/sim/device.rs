//! Simulated-device override for recording traces on an IMAGE file: `FLASHCP_SIM_DEVICE=<file>`
//! makes every strategy input flashcp reads from a real device (transport, max request size,
//! logical block, removable, the cached device profile with its measured gap policies) come
//! from that file instead, so that `flashcp build|add|pull|optimize IMAGE` issues exactly the
//! op sequence it would issue on that device. Nothing is ever saved to the profile cache while
//! it is set.
//!
//! The file is a device profile ([`crate::devprofile::DevProfile`] text format, as cached under
//! `~/.cache/flashcp/devices/`) plus a few `sim.*` keys:
//! ```text
//! flashcp-devprofile 1
//! seq_write_bps=6880000
//! write_skip_s=0.00105
//! seq_read_bps=13800000
//! read_cmd_s=0.00003
//! transport=usb-storage
//! pattern.c32k-m8k=skip:1
//! sim.name=sandisk-cruzer-usb2
//! sim.max_sectors_kb=120
//! sim.logical_block=512
//! sim.removable=1
//! sim.probe_choice=skip:1
//! sim.part_start_sectors=2048
//! ```
//! `sim.probe_choice` is what add's in-situ gap probe decides when the profile has no pattern
//! for the add (the probe's writes still run and are traced; only its timing-based decision,
//! meaningless on an image, is replaced). Without it the image's own measurement decides.

use crate::devprofile::{DevProfile, GapMode, PatternChoice};
use std::sync::OnceLock;

pub const SIM_DEVICE_ENV: &str = "FLASHCP_SIM_DEVICE";

/// The device number every lookup sees while a sim device is set (never a real device).
pub const SIM_RDEV: u64 = u64::MAX;

#[derive(Clone, Debug, Default)]
pub struct SimDevice {
    pub name: String,
    pub profile: DevProfile,
    pub max_sectors_kb: Option<u32>,
    pub logical_block: Option<u64>,
    pub removable: bool,
    pub probe_choice: Option<PatternChoice>,
    /// Partition start on the simulated disk, in 512-byte sectors (alignment of the layout).
    pub part_start_sectors: u64,
    /// Size of the simulated partition (the image size to record on), if known.
    pub size_bytes: Option<u64>,
}

impl SimDevice {
    /// Parses the file format above. Strict about the `sim.*` keys: an unknown `sim.*` key, a
    /// malformed or out-of-range value, or a key given twice is an error naming the line (a
    /// silently ignored override would record the wrong strategy). The profile part keeps
    /// [`DevProfile::parse`]'s rules.
    pub fn parse(text: &str) -> Result<SimDevice, String> {
        let profile = DevProfile::parse(text).ok_or("not a flashcp-devprofile file")?;
        let mut d = SimDevice { profile, name: "sim".into(), ..Default::default() };
        let mut seen = std::collections::HashSet::new();
        for (n, l) in text.lines().enumerate() {
            let Some((k, v)) = l.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            if !k.starts_with("sim.") {
                continue;
            }
            let bad = |what: &str| format!("line {}: {k}={v}: {what}", n + 1);
            if !seen.insert(k.to_string()) {
                return Err(bad("given twice"));
            }
            fn num<T: std::str::FromStr>(v: &str) -> Option<T> {
                v.parse().ok()
            }
            match k {
                "sim.name" => {
                    let ok = !v.is_empty()
                        && v.len() <= 64
                        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
                    if !ok {
                        return Err(bad("expected 1-64 characters of [A-Za-z0-9._-]"));
                    }
                    d.name = v.to_string();
                }
                "sim.max_sectors_kb" => {
                    d.max_sectors_kb = Some(
                        num(v).filter(|k| (1..=1 << 20).contains(k)).ok_or_else(|| bad("expected 1..1048576 (KB)"))?,
                    )
                }
                "sim.logical_block" => {
                    d.logical_block = Some(
                        num::<u64>(v)
                            .filter(|b| b.is_power_of_two() && (512..=4096).contains(b))
                            .ok_or_else(|| bad("expected 512, 1024, 2048 or 4096"))?,
                    )
                }
                "sim.removable" => {
                    d.removable = match v {
                        "1" | "true" => true,
                        "0" | "false" => false,
                        _ => return Err(bad("expected 0, 1, true or false")),
                    }
                }
                "sim.probe_choice" => {
                    d.probe_choice = Some(
                        parse_choice(v)
                            .ok_or_else(|| bad("expected skip|fill|pad<bytes, multiple of 4096>:<qd 1..64>"))?,
                    )
                }
                "sim.part_start_sectors" => {
                    d.part_start_sectors = num(v).ok_or_else(|| bad("expected a sector number"))?;
                }
                "sim.size_bytes" => {
                    d.size_bytes = Some(
                        num::<u64>(v)
                            .filter(|s| *s >= 1 << 20 && s.is_multiple_of(512))
                            .ok_or_else(|| bad("expected bytes, ≥ 1 MiB and a multiple of 512"))?,
                    )
                }
                _ => {
                    return Err(bad(
                        "unknown sim.* key (known: name, max_sectors_kb, logical_block, removable, probe_choice, \
                         part_start_sectors, size_bytes)",
                    ));
                }
            }
        }
        Ok(d)
    }
}

fn parse_choice(v: &str) -> Option<PatternChoice> {
    let (m, q) = v.split_once(':')?;
    let mode = match m {
        "skip" => GapMode::Skip,
        "fill" => GapMode::Fill,
        _ => GapMode::Pad(m.strip_prefix("pad")?.parse().ok().filter(|p: &u64| p.is_multiple_of(4096) && *p > 0)?),
    };
    Some(PatternChoice { mode, qd: q.parse().ok().filter(|q| (1..=crate::devprofile::MAX_WRITE_QD).contains(q))? })
}

/// The simulated device, if `FLASHCP_SIM_DEVICE` is set. An unreadable or invalid file is a
/// hard error (exit 2): a silently ignored override would record the wrong strategy.
pub fn sim_device() -> Option<&'static SimDevice> {
    static DEV: OnceLock<Option<SimDevice>> = OnceLock::new();
    DEV.get_or_init(|| {
        let p = std::env::var_os(SIM_DEVICE_ENV).filter(|p| !p.is_empty())?;
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| {
            eprintln!("flashcp: {SIM_DEVICE_ENV}={}: {e}", p.to_string_lossy());
            std::process::exit(2)
        });
        Some(SimDevice::parse(&text).unwrap_or_else(|e| {
            eprintln!("flashcp: {SIM_DEVICE_ENV}={}: {e}", p.to_string_lossy());
            std::process::exit(2)
        }))
    })
    .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profile_and_sim_keys() {
        let d = SimDevice::parse(
            "flashcp-devprofile 1\nseq_write_bps=6880000\ntransport=usb-storage\npattern.c32k-m8k=skip:1\n\
             sim.name=x\nsim.max_sectors_kb=120\nsim.logical_block=512\nsim.removable=1\nsim.probe_choice=fill:2\n",
        )
        .unwrap();
        assert_eq!(d.name, "x");
        assert_eq!(d.profile.seq_write_bps, Some(6.88e6));
        assert_eq!(d.profile.transport.as_deref(), Some("usb-storage"));
        assert_eq!(d.profile.patterns.len(), 1);
        assert_eq!((d.max_sectors_kb, d.logical_block, d.removable), (Some(120), Some(512), true));
        assert_eq!(d.probe_choice, Some(PatternChoice { mode: GapMode::Fill, qd: 2 }));
        assert!(SimDevice::parse("nope").is_err());
    }

    #[test]
    fn malformed_or_unknown_sim_keys_are_errors() {
        let base = "flashcp-devprofile 1\ntransport=usb-storage\n";
        for (line, needle) in [
            ("sim.max_sectors_kb=12O", "sim.max_sectors_kb"),
            ("sim.max_sectors_kb=0", "1..1048576"),
            ("sim.logical_block=513", "512, 1024"),
            ("sim.removable=yes", "0, 1, true"),
            ("sim.probe_choice=skip", "skip|fill"),
            ("sim.probe_choice=pad100:1", "skip|fill"),
            ("sim.part_start_sectors=-1", "sector number"),
            ("sim.size_bytes=1000", "1 MiB"),
            ("sim.name=a b", "[A-Za-z0-9._-]"),
            ("sim.max_sector_kb=120", "unknown sim.* key"),
        ] {
            let e = SimDevice::parse(&format!("{base}{line}\n")).unwrap_err();
            assert!(e.contains(needle) && e.contains("line 3"), "{line}: {e}");
        }
        let twice = SimDevice::parse(&format!("{base}sim.removable=1\nsim.removable=0\n")).unwrap_err();
        assert!(twice.contains("given twice"), "{twice}");
        // Both shipped device files are valid.
        for f in [include_str!("devices/sandisk.dev"), include_str!("devices/msi.dev")] {
            SimDevice::parse(f).unwrap();
        }
    }
}
