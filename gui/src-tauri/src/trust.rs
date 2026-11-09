//! The trust store written by flashcp-trust: /etc/flashcp/trusted-drives. The GUI only reads it,
//! as a hint for the UI. The helper checks the same file again before any write.

use std::io::ErrorKind;
use std::path::Path;

pub const TRUST_FILE: &str = "/etc/flashcp/trusted-drives";

/// One trusted drive. The wwn and model columns are informational and not compared here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustEntry {
    pub key: String,
    pub size: u64,
}

/// Parses the file. Each line is tab-separated: `v1<TAB><key><TAB><wwn|-><TAB><model|-><TAB><size>`.
/// Blank lines and `#` comments are skipped. Malformed lines are skipped and logged.
pub fn parse(text: &str) -> Vec<TrustEntry> {
    let mut out = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_line(line) {
            Some(entry) => out.push(entry),
            None => eprintln!("flashcp-gui: ignoring malformed trust entry on line {}", number + 1),
        }
    }
    out
}

fn parse_line(line: &str) -> Option<TrustEntry> {
    let fields: Vec<&str> = line.split('\t').collect();
    let &[version, key, wwn, model, size] = fields.as_slice() else {
        return None;
    };
    if version != "v1" || key.is_empty() || wwn.is_empty() || model.is_empty() {
        return None;
    }
    let size = size.parse::<u64>().ok()?;
    Some(TrustEntry { key: key.to_string(), size })
}

/// True when some entry has this identity key and this exact drive size. An empty key never matches.
pub fn is_trusted(entries: &[TrustEntry], key: &str, size: u64) -> bool {
    !key.is_empty() && entries.iter().any(|e| e.key == key && e.size == size)
}

/// Reads the trust file. A missing file means no trusted drives; other read errors are logged.
pub fn load(path: &Path) -> Vec<TrustEntry> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            eprintln!("flashcp-gui: cannot read {}: {e}", path.display());
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v1_lines() {
        let text = "# trusted by flashcp-trust\n\nv1\tSN123\t0x5000cca\tSanDisk Ultra\t32000000000\n";
        assert_eq!(parse(text), vec![TrustEntry { key: "SN123".into(), size: 32_000_000_000 }]);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let text = [
            "v2\tSN1\t-\tM\t100",    // unknown version
            "v1\tSN2\t-\tM",         // too few columns
            "v1\tSN3\t-\tM\t10x",    // size not a number
            "v1\t\t-\tM\t100",       // empty key
            "v1\tSN4\t\tM\t100",     // empty wwn column
            "v1\tSN5\t-\tM\t100\t9", // too many columns
            "v1\tSN6\t-\tM\t100",    // the only good line
        ]
        .join("\n");
        assert_eq!(parse(&text), vec![TrustEntry { key: "SN6".into(), size: 100 }]);
    }

    #[test]
    fn trusted_needs_key_and_size() {
        let entries = vec![TrustEntry { key: "SN1".into(), size: 100 }];
        assert!(is_trusted(&entries, "SN1", 100));
        assert!(!is_trusted(&entries, "SN1", 101));
        assert!(!is_trusted(&entries, "SN2", 100));
        assert!(!is_trusted(&entries, "", 100));
        assert!(!is_trusted(&[TrustEntry { key: String::new(), size: 0 }], "", 0));
    }

    #[test]
    fn missing_file_means_nothing_trusted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn load_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted-drives");
        std::fs::write(&path, "v1\tSN9\t-\tStick\t7\n").unwrap();
        assert_eq!(load(&path), vec![TrustEntry { key: "SN9".into(), size: 7 }]);
    }
}
