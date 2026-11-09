//! Pure selection of USB drives from udisks objects. No D-Bus here, so it is unit-tested.

use serde::Serialize;
use std::collections::HashMap;

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

/// A udisks Drive object, reduced to the fields the filter needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawDrive {
    pub connection_bus: String,
    pub removable: bool,
    pub media_removable: bool,
    pub vendor: String,
    pub model: String,
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
}

/// USB, removable, not hinted as system/ignored, non-empty blocks. A whole disk that has a
/// partition table is skipped (its partitions are listed); an unpartitioned disk is listed
/// as a whole disk.
pub fn select_drives(blocks: &[RawBlock], drives: &HashMap<String, RawDrive>) -> Vec<Drive> {
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
            })
        })
        .collect();
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

/// The text the user must type to confirm an erase: the label, or the device basename
/// (e.g. "sdb1") when the label is empty.
pub fn expected_confirm(drive: &Drive) -> String {
    if drive.label.is_empty() {
        drive.device.rsplit('/').next().unwrap_or_default().to_string()
    } else {
        drive.label.clone()
    }
}

/// Two fake drives for dev runs (FLASHCP_GUI_FAKE_DRIVES=1).
pub fn fake_drives() -> Vec<Drive> {
    vec![
        Drive {
            id: "/org/freedesktop/UDisks2/block_devices/sdz1".into(),
            device: "/dev/sdz1".into(),
            label: "FAKEUSB".into(),
            fs_type: "exfat".into(),
            is_exfat: true,
            size: 32_010_928_128,
            mount_point: Some("/tmp/flashcp-fake-mount".into()),
            model: "Fake USB".into(),
            whole_disk: false,
        },
        Drive {
            id: "/org/freedesktop/UDisks2/block_devices/sdy1".into(),
            device: "/dev/sdy1".into(),
            label: "OLDSTICK".into(),
            fs_type: "vfat".into(),
            is_exfat: false,
            size: 7_980_000_000,
            mount_point: None,
            model: "Fake USB".into(),
            whole_disk: false,
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
        }
    }

    fn sata() -> RawDrive {
        RawDrive {
            connection_bus: "sata".into(),
            removable: false,
            media_removable: false,
            vendor: "ATA".into(),
            model: "SSD".into(),
        }
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

    #[test]
    fn usb_exfat_partition_is_included() {
        let blocks = [part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "PHOTOS")];
        let out = select_drives(&blocks, &drives());
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
    }

    #[test]
    fn mount_point_is_first_mount() {
        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.mount_points = vec!["/media/u/X".into(), "/other".into()];
        let out = select_drives(&[b], &drives());
        assert_eq!(out[0].mount_point.as_deref(), Some("/media/u/X"));
    }

    #[test]
    fn internal_disks_are_excluded() {
        let mut b = part("/b/sda1", "/dev/sda1", "/drv/sata", "exfat", "SYS");
        b.is_partition = true;
        assert!(select_drives(&[b], &drives()).is_empty());

        // a SATA/NVMe disk that is not on the usb bus, even if flagged removable
        let mut internal = sata();
        internal.removable = true;
        let mut m = HashMap::new();
        m.insert("/drv/nvme".to_string(), internal);
        let b = part("/b/nvme0n1p1", "/dev/nvme0n1p1", "/drv/nvme", "vfat", "");
        assert!(select_drives(&[b], &m).is_empty());

        // an empty connection bus (e.g. a virtual or unknown drive) is excluded too
        let mut m = HashMap::new();
        m.insert(
            "/drv/x".to_string(),
            RawDrive { connection_bus: String::new(), removable: true, ..Default::default() },
        );
        let b = part("/b/x1", "/dev/x1", "/drv/x", "exfat", "X");
        assert!(select_drives(&[b], &m).is_empty());
    }

    #[test]
    fn non_removable_usb_is_excluded() {
        let mut m = HashMap::new();
        m.insert("/drv/usb".to_string(), RawDrive { connection_bus: "usb".into(), ..Default::default() });
        let b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        assert!(select_drives(&[b], &m).is_empty());
    }

    #[test]
    fn hint_system_and_ignore_are_excluded() {
        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.hint_system = true;
        assert!(select_drives(&[b], &drives()).is_empty());

        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.hint_ignore = true;
        assert!(select_drives(&[b], &drives()).is_empty());
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
        let out = select_drives(&[whole, p1], &drives());
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
        let out = select_drives(&[b], &drives());
        assert_eq!(out.len(), 1);
        assert!(out[0].whole_disk);
        assert!(out[0].is_exfat);
    }

    #[test]
    fn vfat_usb_is_included_but_not_exfat() {
        let b = part("/b/sdy1", "/dev/sdy1", "/drv/usb", "vfat", "OLD");
        let out = select_drives(&[b], &drives());
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
        let out = select_drives(&[b], &drives());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].fs_type, "");
        assert!(!out[0].is_exfat);
    }

    #[test]
    fn zero_size_is_excluded() {
        let mut b = part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X");
        b.size = 0;
        assert!(select_drives(&[b], &drives()).is_empty());
    }

    #[test]
    fn block_without_known_drive_is_excluded() {
        let b = part("/b/sdb1", "/dev/sdb1", "/drv/missing", "exfat", "X");
        assert!(select_drives(&[b], &drives()).is_empty());
    }

    #[test]
    fn expected_confirm_uses_label_or_basename() {
        let mut d = select_drives(&[part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "PHOTOS")], &drives()).remove(0);
        assert_eq!(expected_confirm(&d), "PHOTOS");
        d.label.clear();
        assert_eq!(expected_confirm(&d), "sdb1");
    }

    #[test]
    fn serializes_camel_case() {
        let d = select_drives(&[part("/b/sdb1", "/dev/sdb1", "/drv/usb", "exfat", "X")], &drives()).remove(0);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["fsType"], "exfat");
        assert_eq!(v["isExfat"], true);
        assert_eq!(v["wholeDisk"], false);
        assert!(v["mountPoint"].is_null());
    }
}
