//! Strict argv parsing. The helper accepts exactly three command shapes and builds the flashcp
//! argv itself from the validated values, so nothing the caller sends is forwarded verbatim.

use std::ffi::OsString;

/// Longest source path we accept (PATH_MAX).
const MAX_SRC: usize = 4096;
/// Longest destination directory inside the volume.
const MAX_TO: usize = 1024;
/// exFAT volume labels hold at most 15 UTF-16 units; we count chars, which is never more lenient
/// for the BMP and keeps the rule simple.
const MAX_LABEL: usize = 15;

/// A validated device name: `/dev/sd` + 1-3 lowercase letters + optional 1-3 digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevName {
    /// Node name without `/dev/`, e.g. `sdb1`.
    pub name: String,
    /// Whole-disk name, e.g. `sdb` (equal to `name` for a whole disk).
    pub disk: String,
}

impl DevName {
    pub fn is_partition(&self) -> bool {
        self.name != self.disk
    }

    /// The path flashcp is told to open. Always the canonical `/dev/<name>` spelling.
    pub fn path(&self) -> String {
        format!("/dev/{}", self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Add { src: String, dev: DevName, to: Option<String>, skip_existing: bool },
    Build { src: String, dev: DevName, label: Option<String> },
    Info { dev: DevName },
}

impl Request {
    pub fn dev(&self) -> &DevName {
        match self {
            Request::Add { dev, .. } | Request::Build { dev, .. } | Request::Info { dev } => dev,
        }
    }

    /// The exact argv (without argv[0]) handed to flashcp. `--json` is always appended.
    pub fn flashcp_args(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        match self {
            Request::Add { src, dev, to, skip_existing } => {
                v.extend(["add".into(), src.clone(), dev.path()]);
                if let Some(to) = to {
                    v.extend(["--to".into(), to.clone()]);
                }
                if *skip_existing {
                    v.push("--skip-existing".into());
                }
            }
            Request::Build { src, dev, label } => {
                v.extend(["build".into(), src.clone(), dev.path(), "--yes-erase".into()]);
                if let Some(l) = label {
                    v.extend(["--label".into(), l.clone()]);
                }
            }
            Request::Info { dev } => v.extend(["info".into(), dev.path()]),
        }
        v.push("--json".into());
        v
    }
}

/// Parse the helper's argv (without argv[0]).
pub fn parse(args: &[OsString]) -> Result<Request, String> {
    // Non-UTF-8 is rejected outright: the GUI only ever sends UTF-8 and it keeps every later
    // check a plain string check.
    let args: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().ok_or_else(|| "arguments must be UTF-8".to_string()))
        .collect::<Result<_, _>>()?;
    let (op, rest) = args.split_first().ok_or("missing operation")?;
    match *op {
        "add" => {
            let [src, dev, flags @ ..] = rest else {
                return Err("usage: add <src> <dev> [--to <dir>] [--skip-existing]".into());
            };
            let (src, dev) = (check_src(src)?, parse_dev(dev)?);
            let (mut to, mut skip_existing) = (None, false);
            let mut it = flags.iter();
            while let Some(f) = it.next() {
                match *f {
                    "--to" if to.is_none() => to = Some(check_to(it.next().ok_or("--to needs a value")?)?),
                    "--skip-existing" if !skip_existing => skip_existing = true,
                    "--to" | "--skip-existing" => return Err(format!("duplicate flag {f}")),
                    _ => return Err("unexpected argument for add".into()),
                }
            }
            Ok(Request::Add { src, dev, to, skip_existing })
        }
        "build" => {
            let [src, dev, flags @ ..] = rest else {
                return Err("usage: build <src> <dev> --yes-erase [--label <L>]".into());
            };
            let (src, dev) = (check_src(src)?, parse_dev(dev)?);
            let (mut yes, mut label) = (false, None);
            let mut it = flags.iter();
            while let Some(f) = it.next() {
                match *f {
                    "--yes-erase" if !yes => yes = true,
                    "--label" if label.is_none() => {
                        label = Some(check_label(it.next().ok_or("--label needs a value")?)?)
                    }
                    "--yes-erase" | "--label" => return Err(format!("duplicate flag {f}")),
                    _ => return Err("unexpected argument for build".into()),
                }
            }
            // Erasing is never implied: the caller must say so explicitly.
            if !yes {
                return Err("build requires --yes-erase".into());
            }
            Ok(Request::Build { src, dev, label })
        }
        "info" => match rest {
            [dev] => Ok(Request::Info { dev: parse_dev(dev)? }),
            _ => Err("usage: info <dev>".into()),
        },
        _ => Err("unknown operation".into()),
    }
}

/// `/dev/sd` + 1-3 lowercase letters + 0-3 digits, nothing else. No normalisation is done:
/// `/dev/../dev/sda`, trailing spaces, symlink paths like `/dev/disk/by-id/...` all fail here,
/// so the only thing that reaches device validation is a plain sd node name.
pub fn parse_dev(s: &str) -> Result<DevName, String> {
    let bad = || "device must look like /dev/sdX or /dev/sdXN".to_string();
    let rest = s.strip_prefix("/dev/sd").ok_or_else(bad)?;
    let letters = rest.bytes().take_while(u8::is_ascii_lowercase).count();
    let digits = rest[letters..].bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=3).contains(&letters) || digits > 3 || letters + digits != rest.len() {
        return Err(bad());
    }
    Ok(DevName { name: format!("sd{rest}"), disk: format!("sd{}", &rest[..letters]) })
}

/// The source is read by flashcp running as the invoking user, so the helper only needs to make
/// sure it is an unambiguous path argument (absolute, so it can never be parsed as a flag).
fn check_src(s: &str) -> Result<String, String> {
    if !s.starts_with('/') {
        return Err("source must be an absolute path".into());
    }
    if s.contains('\0') || s.len() > MAX_SRC {
        return Err("source path is invalid or too long".into());
    }
    Ok(s.to_string())
}

/// Destination directory inside the exFAT volume. No `..` so flashcp never has to resolve
/// upward, and no control characters (they cannot be represented sanely in exFAT names).
fn check_to(s: &str) -> Result<String, String> {
    if !s.starts_with('/') {
        return Err("--to must start with /".into());
    }
    if s.len() > MAX_TO || s.chars().any(char::is_control) || s.split('/').any(|c| c == "..") {
        return Err("--to is invalid".into());
    }
    Ok(s.to_string())
}

/// Volume label. A leading `-` is refused so flashcp's argument parser can never read it as
/// a flag.
fn check_label(s: &str) -> Result<String, String> {
    let n = s.chars().count();
    if n == 0 || n > MAX_LABEL || s.chars().any(|c| c.is_control() || c == '/') || s.starts_with('-') {
        return Err("--label must be 1-15 characters, no control characters, '/' or leading '-'".into());
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Request, String> {
        parse(&a.iter().map(OsString::from).collect::<Vec<_>>())
    }

    fn argv(a: &[&str]) -> Vec<String> {
        p(a).unwrap().flashcp_args()
    }

    #[test]
    fn valid_shapes_build_exact_argv() {
        assert_eq!(
            argv(&["add", "/home/u/x", "/dev/sdb1", "--to", "/Photos", "--skip-existing"]),
            ["add", "/home/u/x", "/dev/sdb1", "--to", "/Photos", "--skip-existing", "--json"]
        );
        // Flag order does not matter; output order is fixed.
        assert_eq!(
            argv(&["add", "/home/u/x", "/dev/sdb1", "--skip-existing", "--to", "/Photos"]),
            ["add", "/home/u/x", "/dev/sdb1", "--to", "/Photos", "--skip-existing", "--json"]
        );
        assert_eq!(argv(&["add", "/x", "/dev/sdc"]), ["add", "/x", "/dev/sdc", "--json"]);
        assert_eq!(argv(&["add", "/x", "/dev/sdc", "--to", "/"]), ["add", "/x", "/dev/sdc", "--to", "/", "--json"]);
        assert_eq!(
            argv(&["build", "/home/u/d", "/dev/sdb", "--yes-erase", "--label", "MY STICK"]),
            ["build", "/home/u/d", "/dev/sdb", "--yes-erase", "--label", "MY STICK", "--json"]
        );
        assert_eq!(
            argv(&["build", "/d", "/dev/sdaa12", "--label", "X", "--yes-erase"]),
            ["build", "/d", "/dev/sdaa12", "--yes-erase", "--label", "X", "--json"]
        );
        assert_eq!(argv(&["info", "/dev/sdz9"]), ["info", "/dev/sdz9", "--json"]);
        assert_eq!(argv(&["add", "/a b/ünï", "/dev/sdb1", "--to", "/ä/b c"])[1], "/a b/ünï");
    }

    #[test]
    fn dev_name_split() {
        assert_eq!(parse_dev("/dev/sdb1").unwrap(), DevName { name: "sdb1".into(), disk: "sdb".into() });
        assert!(!parse_dev("/dev/sdabc").unwrap().is_partition());
        assert!(parse_dev("/dev/sdabc123").unwrap().is_partition());
    }

    #[test]
    fn device_path_tricks_rejected() {
        for d in [
            "/dev/sdb1/../sda",
            "/dev/../dev/sda",
            "dev/sdb",
            "/dev/sdb1 ",
            " /dev/sdb1",
            "/dev/sdB1",
            "/dev/disk/by-id/usb-x",
            "/dev/sd",
            "/dev/sd1",
            "/dev/sdabcd",
            "/dev/sda1234",
            "/dev/sda1b",
            "/dev/nvme0n1p1",
            "/dev/mmcblk0",
            "/dev//sda",
            "/dev/sda\0",
            "/dev/sda\n",
            "",
        ] {
            assert!(p(&["info", d]).is_err(), "{d:?} accepted");
            assert!(p(&["add", "/x", d]).is_err(), "{d:?} accepted");
        }
    }

    #[test]
    fn bad_args_rejected() {
        let cases: &[&[&str]] = &[
            &[],
            &["frobnicate", "/dev/sdb"],
            &["ADD", "/x", "/dev/sdb1"],
            &["add", "/x"],
            &["add", "x", "/dev/sdb1"],
            &["add", "-rf", "/dev/sdb1"],
            &["add", "./x", "/dev/sdb1"],
            &["add", "/x\0y", "/dev/sdb1"],
            &["add", "/x", "/dev/sdb1", "--to", "../etc"],
            &["add", "/x", "/dev/sdb1", "--to", "/a/../../b"],
            &["add", "/x", "/dev/sdb1", "--to", "/a/.."],
            &["add", "/x", "/dev/sdb1", "--to", "a"],
            &["add", "/x", "/dev/sdb1", "--to", "/a\nb"],
            &["add", "/x", "/dev/sdb1", "--to", "/a\0b"],
            &["add", "/x", "/dev/sdb1", "--to"],
            &["add", "/x", "/dev/sdb1", "--to", "/a", "--to", "/b"],
            &["add", "/x", "/dev/sdb1", "--skip-existing", "--skip-existing"],
            &["add", "/x", "/dev/sdb1", "--yes-erase"],
            &["add", "/x", "/dev/sdb1", "/extra"],
            &["add", "/x", "/dev/sdb1", "--to=/a"],
            &["build", "/x", "/dev/sdb"],
            &["build", "/x", "/dev/sdb", "--label", "L"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--yes-erase"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--label", "SIXTEEN_CHARS_XX"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--label", ""],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--label", "a/b"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--label", "a\tb"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--label", "-x"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--label"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--label", "A", "--label", "B"],
            &["build", "/x", "/dev/sdb", "--yes-erase", "--skip-existing"],
            &["info"],
            &["info", "/dev/sdb", "--json"],
        ];
        for c in cases {
            assert!(p(c).is_err(), "{c:?} accepted");
        }
        let long = format!("/{}", "a".repeat(MAX_SRC));
        assert!(p(&["add", &long, "/dev/sdb1"]).is_err());
        let long_to = format!("/{}", "a".repeat(MAX_TO));
        assert!(p(&["add", "/x", "/dev/sdb1", "--to", &long_to]).is_err());
        // 15 chars (multi-byte) is fine.
        assert!(p(&["build", "/x", "/dev/sdb", "--yes-erase", "--label", "ééééééééééééééé"]).is_ok());
    }

    #[test]
    fn non_utf8_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let a = vec![OsString::from("add"), OsString::from_vec(b"/x\xff".to_vec()), OsString::from("/dev/sdb1")];
        assert!(parse(&a).is_err());
    }
}
