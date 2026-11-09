//! Per-device performance profile (sequential bandwidth, per-request costs), measured once per
//! device and cached under $XDG_CACHE_HOME/flashcp, keyed by the device's serial / WWN.

use std::fs;
use std::path::Path;

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
}
