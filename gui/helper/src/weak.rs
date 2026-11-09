//! Placeholder ("junk") drive identities, in ONE place.
//!
//! Many cheap USB bridges and sticks report a serial number that is the same on every unit
//! (all zeros, `0123456789ABCDEF`, ...). Trusting such a serial would trust every drive of
//! that make. This file has no dependencies so the GUI compiles the very same code
//! (`#[path = "../../helper/src/weak.rs"]`) and both sides agree on which drives can be
//! remembered.

/// Serials seen as factory placeholders, compared case-insensitively after trimming.
const JUNK_SERIALS: &[&str] = &[
    "0123456789abcdef",
    "123456789abcdef0",
    "0123456789",
    "1234567890",
    "123456789",
    "12345678",
    "0000000000000001",
    "00000000000000000001",
    "deadbeef",
    "none",
    "null",
    "n/a",
    "na",
    "default",
    "default string",
    "serial",
    "serialnumber",
    "to be filled by o.e.m.",
    "0x0",
];

/// True when `serial` can't tell one physical drive from another of the same make.
pub fn is_weak_serial(serial: &str) -> bool {
    let s = serial.trim();
    let lower = s.to_ascii_lowercase();
    let mut chars = lower.chars().filter(|c| !matches!(c, ' ' | '-' | '_' | '.' | ':'));
    let first = chars.clone().next();
    s.chars().count() < 4
        // the same character repeated (000000000000, FFFFFFFF, "    ", ...)
        || first.is_none_or(|f| chars.all(|c| c == f))
        || JUNK_SERIALS.contains(&lower.as_str())
}

/// A real WWN (NAA type + 24-bit vendor OUI + vendor-assigned id) has plenty of different
/// digits; placeholders like `0x0000000000000001` don't.
const MIN_DISTINCT_NONZERO_WWN_DIGITS: usize = 4;

/// True when a WWN is missing or a placeholder: all-zero / one repeated digit after `0x`, or
/// fewer than [`MIN_DISTINCT_NONZERO_WWN_DIGITS`] distinct nonzero hex digits.
pub fn is_weak_wwn(wwn: &str) -> bool {
    let w = wwn.trim();
    if w.is_empty() || w == "-" {
        return true;
    }
    let hex = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")).unwrap_or(w);
    let mut distinct: Vec<char> =
        hex.chars().filter(|c| c.is_ascii_hexdigit() && *c != '0').map(|c| c.to_ascii_lowercase()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    is_weak_serial(hex) || distinct.len() < MIN_DISTINCT_NONZERO_WWN_DIGITS
}

/// The key a drive is remembered by: its serial if that is real, else its WWN if that is real,
/// else `None` (the drive can't be trusted persistently).
pub fn identity_key<'a>(serial: &'a str, wwn: &'a str) -> Option<&'a str> {
    if !is_weak_serial(serial) {
        Some(serial)
    } else if !is_weak_wwn(wwn) {
        Some(wwn)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_serials_are_weak() {
        for s in [
            "",
            "   ",
            "abc",
            "000000000000",
            "0000 0000",
            "FFFFFFFFFFFF",
            "aaaa",
            "0123456789ABCDEF",
            "0123456789abcdef",
            " 1234567890 ",
            "123456789",
            "12345678",
            "DEADBEEF",
            "To Be Filled By O.E.M.",
            "None",
            "0-0-0-0",
        ] {
            assert!(is_weak_serial(s), "{s:?} should be weak");
        }
    }

    #[test]
    fn real_serials_are_not_weak() {
        for s in ["500014a000000001", "4C530001234567", "AA00000000001234", "S3Z9NB0K123456X", "1001"] {
            assert!(!is_weak_serial(s), "{s:?} should be accepted");
        }
    }

    #[test]
    fn wwn_rules() {
        assert!(is_weak_wwn(""));
        assert!(is_weak_wwn("-"));
        assert!(is_weak_wwn("0x0000000000000000"));
        assert!(is_weak_wwn("0xffffffffffffffff"));
        assert!(!is_weak_wwn("0x500014a000000001"));
        assert!(!is_weak_wwn("0x50014ee2b5a1c3d7"));
        // Fewer than 4 distinct nonzero hex digits: placeholder-ish.
        for w in ["0x0000000000000001", "0x1000000000000001", "0x5000000000000012", "0x0101010101010101", "0x5000CCA0"]
        {
            assert!(is_weak_wwn(w), "{w} should be weak");
        }
        // Exactly 4 distinct nonzero digits is enough; case doesn't matter.
        assert!(!is_weak_wwn("0x5000000000001234"));
        assert!(!is_weak_wwn("0X5000A0B0C0000000"));
    }

    #[test]
    fn key_prefers_real_serial_then_wwn() {
        assert_eq!(identity_key("500014a000000001", "0x5001"), Some("500014a000000001"));
        // Placeholder serial WITH a real WWN: keyed by the WWN.
        assert_eq!(identity_key("0123456789ABCDEF", "0x500014a000000001"), Some("0x500014a000000001"));
        assert_eq!(identity_key("", "0x500014a000000001"), Some("0x500014a000000001"));
        // Placeholder serial and no (or a junk) WWN: not trustable.
        assert_eq!(identity_key("000000000000", "-"), None);
        assert_eq!(identity_key("0123456789ABCDEF", "0x0000000000000000"), None);
        assert_eq!(identity_key("000000000000", "0x0000000000000001"), None);
        assert_eq!(identity_key("", ""), None);
    }
}
