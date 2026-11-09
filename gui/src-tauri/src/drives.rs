//! Pure selection of USB drives from udisks objects, the trust gate, and fake drives for dev runs.
//! No D-Bus here, so it is unit-tested.

use crate::trust::{self, TrustEntry};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

pub const NOT_TRUSTED_MESSAGE: &str = "This drive isn't trusted yet. Use “Trust this drive…” first.";
pub const NO_SERIAL_MESSAGE: &str = "This drive doesn't report a usable serial number or WWN, so flashcp \
     can't remember it safely. You can still copy to it from the command line: sudo flashcp add <folder> /dev/sdX";

/// A udisks Block object, reduced to the fields the filter needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawBlock {
    pub object_path: String,
    pub device: String,
    pub id_type: String,
    pub id_label: String,
    pub size: u64,
    pub hint_system: bool,
    pub hint_ignore: bool,
    pub drive_path: String,
    pub mount_points: Vec<String>,
    pub is_partition: bool,
    pub is_partition_table: bool,
    pub has_filesystem: bool,
}

/// A udisks Drive object (the whole disk), reduced to the fields the filter needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawDrive {
    pub connection_bus: String,
    pub removable: bool,
    pub media_removable: bool,
    pub vendor: String,
    pub model: String,
    /// udisks Drive.Serial.
    pub serial: String,
    /// udisks Drive.WWN.
    pub wwn: String,
    /// udisks Drive.Size, in bytes, for the whole disk.
    pub drive_size: u64,
}

/// What the frontend shows. Serialized as camelCase.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    pub device: String,
    pub label: String,
    pub fs_type: String,
    pub is_exfat: bool,
    pub size: u64,
    pub mount_point: Option<String>,
    pub model: String,
    pub whole_disk: bool,
    /// Identity key (same rule as the helper, see weak.rs): the serial unless it is a
    /// placeholder, else the WWN unless that is one too, else "" (can't be trusted).
    pub serial: String,
    /// udisks Drive.WWN, "-" when none. Part of the identity tuple sent to flashcp-trust.
    pub wwn: String,
    /// udisks Drive.Model alone (no vendor), "-" when none. Part of the identity tuple.
    pub id_model: String,
    /// Whole-disk size in bytes (udisks Drive.Size). Part of the identity tuple.
    pub drive_size: u64,
    /// The trust file has this serial and this drive size. A UI hint only; the helper decides.
    pub trusted: bool,
}

/// USB, removable, not hinted as system/ignored, non-empty blocks. A whole disk that has a
/// partition table is skipped (its partitions are listed); an unpartitioned disk is listed
/// as a whole disk.
pub fn select_drives(blocks: &[RawBlock], drives: &HashMap<String, RawDrive>, trust: &[TrustEntry]) -> Vec<Drive> {
    let mut out: Vec<Drive> = blocks
        .iter()
        .filter_map(|b| {
            let d = drives.get(&b.drive_path)?;
            if d.connection_bus != "usb" || !(d.removable || d.media_removable) {
                return None;
            }
            if b.hint_system || b.hint_ignore || b.size == 0 {
                return None;
            }
            if b.is_partition_table {
                return None;
            }
            let fs_type = if b.has_filesystem { b.id_type.clone() } else { String::new() };
            let model = format!("{} {}", d.vendor.trim(), d.model.trim()).trim().to_string();
            let wwn = if d.wwn.trim().is_empty() { "-".to_string() } else { d.wwn.clone() };
            let id_model = if d.model.trim().is_empty() { "-".to_string() } else { d.model.trim().to_string() };
            let serial = crate::weak::identity_key(&d.serial, &wwn).unwrap_or("").to_string();
            let trusted = trust::is_trusted(trust, &serial, d.drive_size);
            Some(Drive {
                id: b.object_path.clone(),
                device: b.device.clone(),
                label: b.id_label.clone(),
                is_exfat: fs_type == "exfat",
                fs_type,
                size: b.size,
                mount_point: b.mount_points.first().cloned(),
                model,
                whole_disk: !b.is_partition,
                serial,
                wwn,
                id_model,
                drive_size: d.drive_size,
                trusted,
            })
        })
        .collect();
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

/// The text the user must type to confirm an erase: the label of an exFAT drive when it has
/// one, otherwise the identity key. The helper applies the same rule.
pub fn expected_confirm(drive: &Drive) -> String {
    if drive.is_exfat && !drive.label.is_empty() {
        drive.label.clone()
    } else {
        drive.serial.clone()
    }
}

/// Refuses a drive that must not reach the helper: no identity, or not trusted yet.
pub fn require_trusted(drive: &Drive) -> Result<(), String> {
    if drive.serial.is_empty() {
        return Err(NO_SERIAL_MESSAGE.to_string());
    }
    if !drive.trusted {
        return Err(NOT_TRUSTED_MESSAGE.to_string());
    }
    Ok(())
}

const FAKE_USB_ID: &str = "/org/freedesktop/UDisks2/block_devices/sdz1";
const FAKE_OLD_ID: &str = "/org/freedesktop/UDisks2/block_devices/sdy1";

/// Fake drives that trust_drive marked in this process (dev mode only).
static FAKE_TRUSTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Marks a fake drive as trusted for the rest of this process.
pub fn fake_trust(id: &str) {
    let mut marked = FAKE_TRUSTED.lock().unwrap_or_else(PoisonError::into_inner);
    if !marked.iter().any(|m| m == id) {
        marked.push(id.to_string());
    }
}

/// Two fake drives for dev runs (FLASHCP_GUI_FAKE_DRIVES=1). FAKEUSB starts trusted, OLDSTICK does not.
pub fn fake_drives() -> Vec<Drive> {
    let marked = FAKE_TRUSTED.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let trusted = |id: &str| id == FAKE_USB_ID || marked.iter().any(|m| m == id);
    vec![
        Drive {
            id: FAKE_USB_ID.into(),
            device: "/dev/sdz1".into(),
            label: "FAKEUSB".into(),
            fs_type: "exfat".into(),
            is_exfat: true,
            size: 32_010_928_128,
            mount_point: Some("/tmp/flashcp-fake-mount".into()),
            model: "Fake USB".into(),
            whole_disk: false,
            serial: "FAKE0001".into(),
            wwn: "-".into(),
            id_model: "Fake USB".into(),
            drive_size: 32_015_122_432,
            trusted: trusted(FAKE_USB_ID),
        },
        Drive {
            id: FAKE_OLD_ID.into(),
            device: "/dev/sdy1".into(),
            label: "OLDSTICK".into(),
            fs_type: "vfat".into(),
            is_exfat: false,
            size: 7_980_000_000,
            mount_point: None,
            model: "Fake USB".into(),
            whole_disk: false,
            serial: "OLD0002".into(),
            wwn: "-".into(),
            id_model: "Fake USB".into(),
            drive_size: 8_004_829_184,
            trusted: trusted(FAKE_OLD_ID),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb() -> RawDrive {
        RawDrive {
            connection_bus: "usb".into(),
            removable: true,
            media_removable: true,
            vendor: "SanDisk".into(),
            model: "Ultra ".into(),
            serial: "SN123".into(),
            wwn: "0x5000cca".into(),
            drive_size: 32_000_000_000,
        }
    }

    fn sata() -> RawDrive {
        RawDrive { connection_bus: "sata".into(), vendor: "ATA".into(), model: "SSD".into(), ..Default::default() }
    }

    fn part(path: &str, dev: &str, drive: &str, fs: &str, label: &str) -> RawBlock {
        RawBlock {
            object_path: path.into(),
            device: dev.into(),
            id_type: fs.into(),
            id_label: label.into(),
            size: 32_000_000_000,
            drive_path: drive.into(),
            is_partition: true,
            has_filesystem: !fs.is_empty(),
            ..Default::default()
        }
    }

    fn drives() -> HashMap<String, RawDrive> {
        let mut m = HashMap::new();
        m.insert("/drv/usb".to_string(), usb());
        m.insert("/drv/sata".to_string(), sata());
        m
    }

    fn trust_for(key: &str, size: u64) -> Vec<TrustEntry> {
        vec![TrustEntry { key: key.into(), size }]
    }

    #[test]
    fn usb_exfat_partition_is_included() {
        let blocks = [part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "PHOTOS")];
        let out = select_drives(&blocks, &drives(), &[]);
        assert_eq!(out.len(), 1);
        let d = &out[0];
        assert_eq!(d.id, "/b/sdb1");
        assert_eq!(d.device, "/dev/sdb1");
        assert_eq!(d.label, "PHOTOS");
        assert_eq!(d.fs_type, "exfat");
        assert!(d.is_exfat);
        assert!(!d.whole_disk);
        assert_eq!(d.model, "SanDisk Ultra");
        assert_eq!(d.mount_point, None);
        assert_eq!(d.serial, "SN123");
        assert!(!d.trusted);
    }

    #[test]
    fn mount_point_is_first_mount() {
        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.mount_points = vec!["/media/u/X".into(), "/other".into()];
        let out = select_drives(&[b], &drives(), &[]);
        assert_eq!(out[0].mount_point.as_deref(), Some("/media/u/X"));
    }

    #[test]
    fn internal_disks_are_excluded() {
        let b = part("/b/sda1", "/dev/sda1", "/drv/sata", "exfat", "SYS");
        assert!(select_drives(&[b], &drives(), &[]).is_empty());

        // a SATA/NVMe disk that is not on the usb bus, even if flagged removable
        let mut internal = sata();
        internal.removable = true;
        let mut m = HashMap::new();
        m.insert("/drv/nvme".to_string(), internal);
        let b = part("/b/nvme0n1p1", "/dev/nvme0n1p1", "/drv/nvme", "vfat", "");
        assert!(select_drives(&[b], &m, &[]).is_empty());

        // an empty connection bus (e.g. a virtual or unknown drive) is excluded too
        let mut m = HashMap::new();
        m.insert(
            "/drv/x".to_string(),
            RawDrive { connection_bus: String::new(), removable: true, ..Default::default() },
        );
        let b = part("/b/x1", "/dev/x1", "/drv/x", "exfat", "X");
        assert!(select_drives(&[b], &m, &[]).is_empty());
    }

    #[test]
    fn non_removable_usb_is_excluded() {
        let mut m = HashMap::new();
        m.insert("/drv/usb".to_string(), RawDrive { connection_bus: "usb".into(), ..Default::default() });
        let b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        assert!(select_drives(&[b], &m, &[]).is_empty());
    }

    #[test]
    fn hint_system_and_ignore_are_excluded() {
        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.hint_system = true;
        assert!(select_drives(&[b], &drives(), &[]).is_empty());

        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.hint_ignore = true;
        assert!(select_drives(&[b], &drives(), &[]).is_empty());
    }

    #[test]
    fn partitioned_whole_disk_is_excluded_but_partition_listed() {
        let whole = RawBlock {
            object_path: "/b/sdb".into(),
            device: "/dev/sdb".into(),
            size: 32_000_000_000,
            drive_path: "/drv/usb".into(),
            is_partition_table: true,
            ..Default::default()
        };
        let p1 = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "A");
        let out = select_drives(&[whole, p1], &drives(), &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].device, "/dev/sdb1");
    }

    #[test]
    fn unpartitioned_usb_disk_is_whole_disk() {
        let b = RawBlock {
            object_path: "/b/sdc".into(),
            device: "/dev/sdc".into(),
            id_type: "exfat".into(),
            id_label: "".into(),
            size: 8_000_000_000,
            drive_path: "/drv/usb".into(),
            has_filesystem: true,
            ..Default::default()
        };
        let out = select_drives(&[b], &drives(), &[]);
        assert_eq!(out.len(), 1);
        assert!(out[0].whole_disk);
        assert!(out[0].is_exfat);
    }

    #[test]
    fn vfat_usb_is_included_but_not_exfat() {
        let b = part("/b/sdy1", "/dev/sdy1", "/drv/usb", "vfat", "OLD");
        let out = select_drives(&[b], &drives(), &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].fs_type, "vfat");
        assert!(!out[0].is_exfat);
    }

    #[test]
    fn blank_usb_has_empty_fs_type() {
        let b = RawBlock {
            object_path: "/b/sdd".into(),
            device: "/dev/sdd".into(),
            size: 4_000_000_000,
            drive_path: "/drv/usb".into(),
            ..Default::default()
        };
        let out = select_drives(&[b], &drives(), &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].fs_type, "");
        assert!(!out[0].is_exfat);
    }

    #[test]
    fn zero_size_is_excluded() {
        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.size = 0;
        assert!(select_drives(&[b], &drives(), &[]).is_empty());
    }

    #[test]
    fn block_without_known_drive_is_excluded() {
        let b = part("/b/sdb1", "/dev/sdb1", "/drv/missing", "exfat", "X");
        assert!(select_drives(&[b], &drives(), &[]).is_empty());
    }

    #[test]
    fn trusted_needs_serial_and_drive_size() {
        let blocks = [part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X")];
        assert!(select_drives(&blocks, &drives(), &trust_for("SN123", 32_000_000_000))[0].trusted);
        assert!(!select_drives(&blocks, &drives(), &trust_for("SN123", 32_000_000_001))[0].trusted);
        assert!(!select_drives(&blocks, &drives(), &trust_for("SN999", 32_000_000_000))[0].trusted);
    }

    #[test]
    fn wwn_is_the_identity_when_serial_is_empty() {
        let mut m = HashMap::new();
        m.insert("/drv/usb".to_string(), RawDrive { serial: String::new(), ..usb() });
        let blocks = [part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X")];
        let d = &select_drives(&blocks, &m, &trust_for("0x5000cca", 32_000_000_000))[0];
        assert_eq!(d.serial, "0x5000cca");
        assert!(d.trusted);
    }

    #[test]
    fn placeholder_serial_is_keyed_by_wwn_or_untrustable() {
        let blocks = [part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X")];
        let mut m = HashMap::new();
        // Junk serial WITH a real WWN: keyed by the WWN, like the helper.
        m.insert("/drv/usb".to_string(), RawDrive { serial: "0123456789ABCDEF".into(), ..usb() });
        assert_eq!(select_drives(&blocks, &m, &[])[0].serial, "0x5000cca");
        // Junk serial and no WWN: can't be trusted.
        m.insert("/drv/usb".to_string(), RawDrive { serial: "000000000000".into(), wwn: String::new(), ..usb() });
        let d = &select_drives(&blocks, &m, &trust_for("000000000000", 32_000_000_000))[0];
        assert_eq!((d.serial.as_str(), d.wwn.as_str(), d.trusted), ("", "-", false));
        assert_eq!(require_trusted(d), Err(NO_SERIAL_MESSAGE.to_string()));
    }

    #[test]
    fn identity_tuple_fields() {
        let mut m = HashMap::new();
        m.insert("/drv/usb".to_string(), usb());
        let blocks = [part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X")];
        let d = &select_drives(&blocks, &m, &[])[0];
        // id_model is udisks' Model alone (what the helper derives from ID_MODEL_ENC), not vendor+model.
        assert_eq!((d.id_model.as_str(), d.model.as_str()), ("Ultra", "SanDisk Ultra"));
        assert_eq!(d.wwn, "0x5000cca");
        assert_eq!(d.drive_size, usb().drive_size);
    }

    #[test]
    fn no_serial_or_wwn_means_untrustable() {
        let mut m = HashMap::new();
        m.insert("/drv/usb".to_string(), RawDrive { serial: String::new(), wwn: String::new(), ..usb() });
        let blocks = [part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X")];
        let d = &select_drives(&blocks, &m, &trust_for("", 32_000_000_000))[0];
        assert_eq!(d.serial, "");
        assert!(!d.trusted);
        assert_eq!(require_trusted(d), Err(NO_SERIAL_MESSAGE.to_string()));
    }

    #[test]
    fn require_trusted_gates_untrusted_drives() {
        let mut d = fake_drives().remove(0);
        assert_eq!(require_trusted(&d), Ok(()));
        d.trusted = false;
        assert_eq!(require_trusted(&d), Err(NOT_TRUSTED_MESSAGE.to_string()));
    }

    #[test]
    fn expected_confirm_uses_exfat_label_else_serial() {
        let mut d =
            select_drives(&[part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "PHOTOS")], &drives(), &[]).remove(0);
        assert_eq!(expected_confirm(&d), "PHOTOS");
        d.label.clear();
        assert_eq!(expected_confirm(&d), "SN123");
        // a labelled non-exFAT drive confirms with its serial, as the helper does
        let mut vfat =
            select_drives(&[part("/b/sdy1", "/dev/sdy1", "/drv/usb", "vfat", "OLD")], &drives(), &[]).remove(0);
        assert_eq!(expected_confirm(&vfat), "SN123");
        vfat.is_exfat = true;
        assert_eq!(expected_confirm(&vfat), "OLD");
    }

    #[test]
    fn fake_trust_marks_only_the_given_drive() {
        let old = FAKE_OLD_ID;
        assert!(!fake_drives().iter().find(|d| d.id == old).unwrap().trusted);
        fake_trust(old);
        fake_trust(old);
        assert!(fake_drives().iter().find(|d| d.id == old).unwrap().trusted);
        let usb = fake_drives().into_iter().find(|d| d.id == FAKE_USB_ID).unwrap();
        assert!(usb.trusted);
        assert_eq!(usb.serial, "FAKE0001");
    }

    #[test]
    fn serializes_camel_case() {
        let d = select_drives(&[part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X")], &drives(), &[]).remove(0);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["fsType"], "exfat");
        assert_eq!(v["isExfat"], true);
        assert_eq!(v["wholeDisk"], false);
        assert_eq!(v["serial"], "SN123");
        assert_eq!(v["trusted"], false);
        assert!(v["mountPoint"].is_null());
    }
}
