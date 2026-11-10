//! Entry sets of the driver: name rules, up-cased keys, decoding a live set into its fields and
//! encoding fields back (keeping vendor secondaries of the old set when relocating one).

use crate::exfat::checksum16;
use crate::volume::{le16, le32, le64};

pub(crate) const ATTR_RO: u16 = 0x01;
pub(crate) const ATTR_HIDDEN: u16 = 0x02;
pub(crate) const ATTR_SYSTEM: u16 = 0x04;
pub(crate) const ATTR_DIR: u16 = 0x10;
pub(crate) const ATTR_ARCHIVE: u16 = 0x20;

/// A stored timestamp: DOS-style field, 10 ms increment, UTC offset byte.
pub(crate) type RawTs = (u32, u8, u8);

/// The fields of one file or directory entry set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetFields {
    pub(crate) name: Vec<u16>,
    pub(crate) attrs: u16,
    pub(crate) first: u32,
    pub(crate) nfc: bool,
    pub(crate) len: u64,
    pub(crate) valid: u64,
    pub(crate) ctime: RawTs,
    pub(crate) mtime: RawTs,
    pub(crate) atime: RawTs,
}

/// Why a name cannot be created: EINVAL (a character Windows refuses, a trailing dot or space,
/// `.`/`..`, empty) or ENAMETOOLONG (more than 255 UTF-16 units).
pub(crate) fn check_name(name: &[u16]) -> Result<(), i32> {
    if name.len() > 255 {
        return Err(libc::ENAMETOOLONG);
    }
    if name.is_empty() || name == [0x2E] || name == [0x2E, 0x2E] {
        return Err(libc::EINVAL);
    }
    if name.iter().any(|&c| c < 0x20 || matches!(c, 0x22 | 0x2A | 0x2F | 0x3A | 0x3C | 0x3E | 0x3F | 0x5C | 0x7C)) {
        return Err(libc::EINVAL);
    }
    // Windows strips (and so cannot open) names ending in a dot or a space.
    if matches!(name.last(), Some(0x2E | 0x20)) {
        return Err(libc::EINVAL);
    }
    // An unpaired surrogate cannot be shown or compared by other systems.
    let mut i = 0;
    while i < name.len() {
        match name[i] {
            0xD800..=0xDBFF if i + 1 < name.len() && (0xDC00..=0xDFFF).contains(&name[i + 1]) => i += 2,
            0xD800..=0xDFFF => return Err(libc::EINVAL),
            _ => i += 1,
        }
    }
    Ok(())
}

pub(crate) fn key(name: &[u16], upc: &[u16]) -> Vec<u16> {
    name.iter().map(|&c| upc[c as usize]).collect()
}

pub(crate) fn name_hash(name: &[u16], upc: &[u16]) -> u16 {
    let mut h: u16 = 0;
    for &c in name {
        for b in upc[c as usize].to_le_bytes() {
            h = h.rotate_right(1).wrapping_add(b as u16);
        }
    }
    h
}

/// Bytes of a set with `name_len` name characters and no vendor secondaries.
pub(crate) fn set_bytes(name_len: usize) -> usize {
    32 * (2 + name_len.div_ceil(15))
}

/// The fields of a live, checksum-verified set (as `parse_dir` accepted it).
pub(crate) fn decode(set: &[u8]) -> SetFields {
    let nl = set[35] as usize;
    let name = (0..nl).map(|i| le16(set, 64 + 32 * (i / 15) + 2 + 2 * (i % 15))).collect();
    SetFields {
        name,
        attrs: le16(set, 4),
        first: le32(set, 52),
        nfc: set[33] & 0x02 != 0,
        len: le64(set, 56),
        valid: le64(set, 40),
        ctime: (le32(set, 8), set[20], set[22]),
        mtime: (le32(set, 12), set[21], set[23]),
        atime: (le32(set, 16), 0, set[24]),
    }
}

/// Vendor (benign) secondaries of a set: everything after the name entries.
fn vendor_tail(set: &[u8]) -> &[u8] {
    let nl = set[35] as usize;
    let end = 64 + 32 * nl.div_ceil(15);
    if set.len() > end { &set[end..] } else { &[] }
}

/// Encodes `f` as a complete set (file, stream, names), followed by the vendor secondaries of
/// `old` (a relocated set keeps them), with a valid SetChecksum.
pub(crate) fn encode(f: &SetFields, upc: &[u16], old: Option<&[u8]>) -> Vec<u8> {
    let names = f.name.len().div_ceil(15);
    let tail = old.map(vendor_tail).unwrap_or(&[]);
    let mut e = vec![0u8; 32 * (2 + names) + tail.len()];
    e[0] = 0x85;
    e[1] = (1 + names + tail.len() / 32) as u8;
    e[4..6].copy_from_slice(&f.attrs.to_le_bytes());
    e[8..12].copy_from_slice(&f.ctime.0.to_le_bytes());
    e[12..16].copy_from_slice(&f.mtime.0.to_le_bytes());
    e[16..20].copy_from_slice(&f.atime.0.to_le_bytes());
    e[20] = f.ctime.1;
    e[21] = f.mtime.1;
    e[22] = f.ctime.2;
    e[23] = f.mtime.2;
    e[24] = f.atime.2;
    let s = &mut e[32..64];
    s[0] = 0xC0;
    s[1] = if f.first == 0 {
        0x01
    } else if f.nfc {
        0x03
    } else {
        0x01
    };
    s[3] = f.name.len() as u8;
    s[4..6].copy_from_slice(&name_hash(&f.name, upc).to_le_bytes());
    s[8..16].copy_from_slice(&f.valid.to_le_bytes());
    s[20..24].copy_from_slice(&f.first.to_le_bytes());
    s[24..32].copy_from_slice(&f.len.to_le_bytes());
    for (k, chunk) in f.name.chunks(15).enumerate() {
        let n = &mut e[64 + 32 * k..96 + 32 * k];
        n[0] = 0xC1;
        for (j, &c) in chunk.iter().enumerate() {
            n[2 + 2 * j..4 + 2 * j].copy_from_slice(&c.to_le_bytes());
        }
    }
    let at = 32 * (2 + names);
    e[at..].copy_from_slice(tail);
    let c = checksum16(&e, true);
    e[2..4].copy_from_slice(&c.to_le_bytes());
    e
}

/// The raw timestamp of (secs, nanos) UTC, as flashcp stores it.
pub(crate) fn raw_ts(t: (i64, u32)) -> RawTs {
    let (v, t10) = crate::exfat::ts(t.0, t.1);
    (v, t10, 0x80)
}

/// (secs, nanos) UTC of a raw timestamp (1980-01-01 for an undecodable one).
pub(crate) fn unix_ts(r: RawTs) -> (i64, u32) {
    crate::volume::decode_ts(r.0, r.1, r.2).unwrap_or((315_532_800, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn names_follow_the_windows_rules() {
        for (n, want) in [
            ("ok.txt", Ok(())),
            ("a:b", Err(libc::EINVAL)),
            ("a\\b", Err(libc::EINVAL)),
            ("tab\there", Err(libc::EINVAL)),
            ("trailing.", Err(libc::EINVAL)),
            ("trailing ", Err(libc::EINVAL)),
            (".", Err(libc::EINVAL)),
            ("..", Err(libc::EINVAL)),
            (".hidden", Ok(())),
            ("日本語 😀", Ok(())),
        ] {
            assert_eq!(check_name(&w(n)), want, "{n}");
        }
        assert_eq!(check_name(&vec![0x41; 255]), Ok(()));
        assert_eq!(check_name(&vec![0x41; 256]), Err(libc::ENAMETOOLONG));
        assert_eq!(check_name(&[0x41, 0xD800, 0x41]), Err(libc::EINVAL));
    }

    #[test]
    fn encode_decode_round_trip_keeps_vendor_secondaries() {
        let upc = crate::exfat::upcase_map();
        let f = SetFields {
            name: w("a fairly long file name, over fifteen.txt"),
            attrs: ATTR_ARCHIVE,
            first: 77,
            nfc: true,
            len: 5000,
            valid: 4000,
            ctime: raw_ts((1_700_000_000, 0)),
            mtime: raw_ts((1_700_000_123, 450_000_000)),
            atime: raw_ts((1_700_000_200, 0)),
        };
        let mut e = encode(&f, &upc, None);
        assert_eq!(decode(&e), SetFields { atime: (f.atime.0, 0, f.atime.2), ..f.clone() });
        assert!(crate::volume::checksum_ok_set(&e));
        // a vendor secondary survives a rename to a shorter name
        let mut vendor = [0u8; 32];
        vendor[0] = 0xE0;
        e[1] += 1;
        e.extend_from_slice(&vendor);
        let g = SetFields { name: w("short"), ..f.clone() };
        let e2 = encode(&g, &upc, Some(&e));
        assert_eq!(e2.len(), 3 * 32 + 32);
        assert_eq!(e2[1], 3);
        assert_eq!(e2[96], 0xE0);
        assert!(crate::volume::checksum_ok_set(&e2));
        assert_eq!(decode(&e2).name, w("short"));
    }
}
