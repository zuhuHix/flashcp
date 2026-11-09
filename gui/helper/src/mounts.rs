//! Parsers for /proc/self/mountinfo and /proc/swaps.

/// One mountinfo line, reduced to what the device checks need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `MAJ:MIN` of the mounted device (field 3).
    pub dev: String,
    pub mount_point: String,
    /// Mount source after the ` - ` separator (e.g. `/dev/sdb1`). Checked as well because some
    /// filesystems (btrfs) report an anonymous `0:N` device in field 3.
    pub source: String,
}

/// Undo the kernel's octal escaping of space, tab, newline and backslash (`\040` etc.).
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1] <= b'3'
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut halves = line.splitn(2, " - ");
            let pre: Vec<&str> = halves.next()?.split(' ').collect();
            let post: Vec<&str> = halves.next().unwrap_or("").split(' ').collect();
            Some(Mount {
                dev: pre.get(2)?.to_string(),
                mount_point: unescape(pre.get(4)?),
                source: post.get(1).map(|s| unescape(s)).unwrap_or_default(),
            })
        })
        .collect()
}

/// Device paths listed in /proc/swaps (first column, header skipped).
pub fn parse_swaps(text: &str) -> Vec<String> {
    text.lines().skip(1).filter_map(|l| l.split_whitespace().next()).map(unescape).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo() {
        let t = "22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
                 90 22 8:17 / /media/u/MY\\040STICK rw,nosuid shared:50 - exfat /dev/sdb1 rw\n\
                 91 22 0:45 / /mnt/b rw - btrfs /dev/sdc1 rw\n\
                 garbage\n";
        let m = parse_mountinfo(t);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0], Mount { dev: "259:2".into(), mount_point: "/".into(), source: "/dev/nvme0n1p2".into() });
        assert_eq!(m[1].mount_point, "/media/u/MY STICK");
        assert_eq!(m[2].source, "/dev/sdc1");
    }

    #[test]
    fn swaps() {
        let t = "Filename\tType\tSize\tUsed\tPriority\n/dev/sdb2 partition 1000 0 -2\n/swap\\040file file 1 0 -3\n";
        assert_eq!(parse_swaps(t), ["/dev/sdb2", "/swap file"]);
    }

    #[test]
    fn unescape_edge() {
        assert_eq!(unescape("a\\134b"), "a\\b");
        assert_eq!(unescape("trail\\04"), "trail\\04");
        assert_eq!(unescape("\\0401"), " 1");
    }
}
