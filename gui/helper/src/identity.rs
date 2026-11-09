//! The stable identity of a whole disk, read from its udev database entry and sysfs size.
//!
//! `key` is what the GUI passes as `--expect` (udisks' `Drive.Serial`, derived with the same
//! priority, checked against a real UAS SSD where both exist and udisks shows the short one):
//! `ID_SERIAL_SHORT`, else `ID_SCSI_SERIAL`. If that serial is a placeholder (see
//! [`crate::weak`]) the WWN is the key instead; with neither the drive cannot be trusted.
//! The model is derived the way udisks shows it (decoded `ID_MODEL_ENC`, trimmed), so the
//! tuple the GUI displays can be compared field by field.

use std::fs;

use crate::args::MAX_KEY;
use crate::device::{udev_props, Roots, ValidDevice};

/// Longest wwn/model value we store.
const MAX_FIELD: usize = 256;

/// One disk's identity. Also the shape of a trust-file entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub key: String,
    /// `-` when the disk reports none.
    pub wwn: String,
    /// `-` when the disk reports none.
    pub model: String,
    /// Bytes.
    pub size: u64,
}

/// A value that can go into a tab-separated trust line and be shown to the user: no TAB,
/// newline or any other control character.
pub fn clean(v: &str) -> bool {
    !v.chars().any(char::is_control)
}

fn first_non_empty<'a>(props: &'a [(String, String)], keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| props.iter().find(|(pk, v)| pk == k && !v.is_empty()).map(|(_, v)| v.as_str()))
}

/// Shown (via the GUI) when a drive has only a placeholder serial and no usable WWN.
pub const UNTRUSTABLE: &str = "this drive reports a placeholder serial number and no WWN, so flashcp can't \
     remember it safely. Copy to it with the command line instead: sudo flashcp add <folder> /dev/sdXN";

/// udisks' `Drive.Model`: `ID_MODEL_ENC` with its `\xHH` escapes decoded and whitespace trimmed,
/// falling back to `ID_MODEL` with `_` turned back into spaces. `-` when there is none.
fn udisks_model(props: &[(String, String)]) -> String {
    let decoded = first_non_empty(props, &["ID_MODEL_ENC"]).map(|enc| {
        let b = enc.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
                let hex = |c: u8| (c as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hex(b[i + 2]), hex(b[i + 3])) {
                    out.push((hi * 16 + lo) as u8);
                    i += 4;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).trim().to_string()
    });
    let m = decoded
        .filter(|m| !m.is_empty())
        .or_else(|| first_non_empty(props, &["ID_MODEL"]).map(|m| m.replace('_', " ").trim().to_string()))
        .unwrap_or_default();
    if m.is_empty() {
        "-".into()
    } else {
        m
    }
}

/// Build the identity from udev properties (`E:` lines of the disk's db entry) and the disk's
/// sysfs `size` (512-byte sectors).
pub fn identity_from(props: &[(String, String)], sectors: u64) -> Result<Identity, String> {
    let serial = first_non_empty(props, &["ID_SERIAL_SHORT", "ID_SCSI_SERIAL"]);
    let wwn = first_non_empty(props, &["ID_WWN_WITH_EXTENSION", "ID_WWN"]).unwrap_or("-");
    let model = udisks_model(props);
    let model = model.as_str();
    for (what, v) in [("serial", serial.unwrap_or("")), ("WWN", wwn), ("model", model)] {
        if !clean(v) {
            return Err(format!("drive {what} contains control characters; refusing to trust it"));
        }
        if v.chars().count() > MAX_FIELD {
            return Err(format!("drive {what} is too long"));
        }
    }
    let key = crate::weak::identity_key(serial.unwrap_or(""), wwn).ok_or(UNTRUSTABLE)?;
    if key.chars().count() > MAX_KEY {
        return Err("drive serial number is too long".into());
    }
    let size = sectors.checked_mul(512).filter(|&s| s > 0).ok_or("drive size is invalid")?;
    Ok(Identity { key: key.into(), wwn: wwn.into(), model: model.into(), size })
}

/// Identity of the whole disk that `dev` (a disk or one of its partitions) lives on.
pub fn read_identity(roots: &Roots, dev: &ValidDevice) -> Result<Identity, String> {
    let props = udev_props(roots, &dev.disk_majmin)?;
    let p = dev.disk_dir.join("size");
    let sectors: u64 = fs::read_to_string(&p)
        .map_err(|e| format!("cannot read {}: {e}", p.display()))?
        .trim()
        .parse()
        .map_err(|_| "unreadable disk size".to_string())?;
    identity_from(&props, sectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn props(kv: &[(&str, &str)]) -> Vec<(String, String)> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const FULL: &[(&str, &str)] = &[
        ("ID_SERIAL", "SanDisk_Ultra_4C530001"),
        ("ID_SERIAL_SHORT", "4C530001"),
        ("ID_SCSI_SERIAL", "AA00000000001234"),
        ("ID_WWN", "0x5000000000000001"),
        ("ID_WWN_WITH_EXTENSION", "0x5000000000000001abcd"),
        ("ID_MODEL", "Ultra"),
    ];

    #[test]
    fn short_serial_has_priority() {
        let id = identity_from(&props(FULL), 1000).unwrap();
        assert_eq!(
            id,
            Identity {
                key: "4C530001".into(),
                wwn: "0x5000000000000001abcd".into(),
                model: "Ultra".into(),
                size: 512_000
            }
        );
    }

    #[test]
    fn scsi_serial_when_no_short_serial() {
        let p: Vec<_> = FULL.iter().copied().filter(|(k, _)| *k != "ID_SERIAL_SHORT").collect();
        assert_eq!(identity_from(&props(&p), 1).unwrap().key, "AA00000000001234");
        // An empty ID_SERIAL_SHORT counts as missing.
        let mut p = props(&p);
        p.insert(0, ("ID_SERIAL_SHORT".into(), String::new()));
        assert_eq!(identity_from(&p, 1).unwrap().key, "AA00000000001234");
    }

    #[test]
    fn wwn_fallback_and_defaults() {
        let id = identity_from(&props(&[("ID_WWN", "0x50014ee2b5a1c3d7")]), 2).unwrap();
        assert_eq!(
            id,
            Identity {
                key: "0x50014ee2b5a1c3d7".into(),
                wwn: "0x50014ee2b5a1c3d7".into(),
                model: "-".into(),
                size: 1024
            }
        );
        let id = identity_from(
            &props(&[("ID_WWN_WITH_EXTENSION", "0x50014ee2b5a1c3d8x"), ("ID_WWN", "0x50014ee2b5a1c3d8")]),
            2,
        )
        .unwrap();
        assert_eq!((id.key.as_str(), id.wwn.as_str()), ("0x50014ee2b5a1c3d8x", "0x50014ee2b5a1c3d8x"));
        let id = identity_from(&props(&[("ID_SERIAL_SHORT", "SER1")]), 2).unwrap();
        assert_eq!((id.wwn.as_str(), id.model.as_str()), ("-", "-"));
    }

    #[test]
    fn placeholder_serial_without_wwn_refuses_persistent_trust() {
        for serial in ["0123456789ABCDEF", "000000000000", "1234567890", "AAAAAAAA", "S1"] {
            let p = props(&[("ID_SERIAL_SHORT", serial), ("ID_MODEL", "Cheap")]);
            assert_eq!(identity_from(&p, 10).unwrap_err(), UNTRUSTABLE, "{serial}");
            // A junk WWN doesn't rescue it either.
            let p = props(&[("ID_SERIAL_SHORT", serial), ("ID_WWN", "0x0000000000000000")]);
            assert_eq!(identity_from(&p, 10).unwrap_err(), UNTRUSTABLE, "{serial}");
        }
    }

    #[test]
    fn placeholder_serial_with_wwn_is_keyed_by_wwn() {
        let p = props(&[("ID_SERIAL_SHORT", "0123456789ABCDEF"), ("ID_WWN", "0x500014a000000001")]);
        let id = identity_from(&p, 10).unwrap();
        assert_eq!((id.key.as_str(), id.wwn.as_str()), ("0x500014a000000001", "0x500014a000000001"));
    }

    #[test]
    fn model_matches_udisks() {
        // Real UAS SSD: udisks shows "DATAMAG 20Gbps" (decoded ID_MODEL_ENC, trimmed).
        let p = props(&[
            ("ID_SERIAL_SHORT", "500014a000000001"),
            ("ID_MODEL", "DATAMAG_20Gbps"),
            ("ID_MODEL_ENC", "DATAMAG\\x2020Gbps\\x20\\x20"),
        ]);
        assert_eq!(identity_from(&p, 1).unwrap().model, "DATAMAG 20Gbps");
        // Without ID_MODEL_ENC, underscores in ID_MODEL become spaces.
        let p = props(&[("ID_SERIAL_SHORT", "500014a000000001"), ("ID_MODEL", "Ultra_Fit_")]);
        assert_eq!(identity_from(&p, 1).unwrap().model, "Ultra Fit");
        // A bad escape is kept literally; non-ASCII after a backslash doesn't panic.
        let p = props(&[("ID_SERIAL_SHORT", "500014a000000001"), ("ID_MODEL_ENC", "A\\xZZ\\xé")]);
        assert_eq!(identity_from(&p, 1).unwrap().model, "A\\xZZ\\xé");
    }

    #[test]
    fn no_serial_no_wwn_is_untrustable() {
        for p in [props(&[]), props(&[("ID_MODEL", "X"), ("ID_SERIAL", "X_123")]), props(&[("ID_SERIAL_SHORT", "")])] {
            assert_eq!(identity_from(&p, 10).unwrap_err(), UNTRUSTABLE);
        }
    }

    #[test]
    fn control_chars_refused() {
        for (k, v) in [
            ("ID_SERIAL_SHORT", "A\tB"),
            ("ID_SERIAL_SHORT", "A\nB"),
            ("ID_MODEL", "M\u{1b}[2J"),
            ("ID_WWN", "0x1\r"),
            ("ID_MODEL", "M\u{7f}"),
        ] {
            let mut p = props(&[("ID_SERIAL_SHORT", "OK1")]);
            p.retain(|(pk, _)| pk != k);
            p.push((k.into(), v.into()));
            assert!(identity_from(&p, 10).unwrap_err().contains("control characters"), "{k}={v:?}");
        }
    }

    #[test]
    fn limits() {
        assert!(identity_from(&props(&[("ID_SERIAL_SHORT", "SER1")]), 0).is_err());
        assert!(identity_from(&props(&[("ID_SERIAL_SHORT", "SER1")]), u64::MAX).is_err());
        let long = "s".repeat(MAX_KEY + 1);
        assert!(identity_from(&props(&[("ID_SERIAL_SHORT", &long)]), 1).is_err());
        let long = "m".repeat(MAX_FIELD + 1);
        assert!(identity_from(&props(&[("ID_SERIAL_SHORT", "SER1"), ("ID_MODEL", &long)]), 1).is_err());
        // Spaces are fine (scsi serials often carry them); they are kept verbatim.
        assert_eq!(identity_from(&props(&[("ID_SCSI_SERIAL", " AB 12 ")]), 1).unwrap().key, " AB 12 ");
    }

    #[test]
    fn read_from_udev_db_and_sysfs() {
        let t = tempfile::tempdir().unwrap();
        let mut roots = Roots::system();
        roots.udev_data = t.path().join("udev");
        fs::create_dir(&roots.udev_data).unwrap();
        let disk_dir: PathBuf = t.path().join("sdb");
        fs::create_dir(&disk_dir).unwrap();
        fs::write(disk_dir.join("size"), "62521344\n").unwrap();
        fs::write(
            roots.udev_data.join("b8:16"),
            "S:disk/by-id/usb-x\nE:ID_MODEL=Cruzer\nE:ID_SERIAL_SHORT=4C53=0001\nE:ID_BUS=usb\nG:systemd\n",
        )
        .unwrap();
        // The partition's own db entry is NOT consulted.
        fs::write(roots.udev_data.join("b8:17"), "E:ID_SERIAL_SHORT=OTHER\n").unwrap();
        let dev = ValidDevice {
            name: crate::args::parse_dev("/dev/sdb1").unwrap(),
            major: 8,
            minor: 17,
            disk_majmin: "8:16".into(),
            disk_dir: disk_dir.clone(),
        };
        let id = read_identity(&roots, &dev).unwrap();
        assert_eq!(
            id,
            Identity { key: "4C53=0001".into(), wwn: "-".into(), model: "Cruzer".into(), size: 32010928128 }
        );
        fs::remove_file(roots.udev_data.join("b8:16")).unwrap();
        assert!(read_identity(&roots, &dev).is_err());
    }
}
