//! Strict argv parsing for both programs. The helper accepts exactly three command shapes and
//! builds every flashcp argv itself from the validated values, so nothing the caller sends is
//! forwarded verbatim.

use std::ffi::OsString;

/// Longest source path we accept (PATH_MAX).
const MAX_SRC: usize = 4096;
/// Longest destination directory inside the volume.
const MAX_TO: usize = 1024;
/// exFAT volume labels hold at most 15 UTF-16 units; we count chars, which is never more lenient
/// for the BMP and keeps the rule simple.
const MAX_LABEL: usize = 15;
/// Longest `--expect` / `--confirm` value, in chars.
pub const MAX_KEY: usize = 128;
/// Most `--item`s in one `add` job.
pub const MAX_ITEMS: usize = 256;
/// The fd number flashcp is told to use (`--target-fd`).
pub const TARGET_FD: i32 = 3;

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

    /// The canonical `/dev/<name>` spelling.
    pub fn path(&self) -> String {
        format!("/dev/{}", self.name)
    }
}

/// One `--item <src> <to>` of an `add` job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub src: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Add { dev: DevName, expect: String, skip_existing: bool, items: Vec<Item> },
    Build { dev: DevName, expect: String, confirm: String, label: Option<String>, src: String },
    Info { dev: DevName, expect: String },
}

/// `flashcp-trust <dev> --expect <key>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRequest {
    pub dev: DevName,
    /// The full identity the user was shown and confirmed. Every field must match the drive
    /// the helper actually opens, or nothing is written.
    pub shown: crate::identity::Identity,
}

fn tail() -> [String; 3] {
    ["--target-fd".into(), TARGET_FD.to_string(), "--json".into()]
}

/// argv (without argv[0]) of `flashcp info --target-fd 3 --json`.
pub fn info_args() -> Vec<String> {
    let mut v = vec!["info".to_string()];
    v.extend(tail());
    v
}

impl Request {
    pub fn dev(&self) -> &DevName {
        match self {
            Request::Add { dev, .. } | Request::Build { dev, .. } | Request::Info { dev, .. } => dev,
        }
    }

    pub fn expect(&self) -> &str {
        match self {
            Request::Add { expect, .. } | Request::Build { expect, .. } | Request::Info { expect, .. } => expect,
        }
    }

    /// `info` only reads; everything else needs a writable, exclusive open.
    pub fn writes(&self) -> bool {
        !matches!(self, Request::Info { .. })
    }

    /// The exact argvs (without argv[0]) of the flashcp runs, in order: one per item for `add`,
    /// one for `build`/`info`.
    pub fn flashcp_runs(&self) -> Vec<Vec<String>> {
        match self {
            Request::Add { skip_existing, items, .. } => items
                .iter()
                .map(|it| {
                    let mut v = vec!["add".to_string(), it.src.clone()];
                    v.extend(["--target-fd".into(), TARGET_FD.to_string(), "--to".into(), it.to.clone()]);
                    if *skip_existing {
                        v.push("--skip-existing".into());
                    }
                    v.push("--json".into());
                    v
                })
                .collect(),
            Request::Build { label, src, .. } => {
                let mut v = vec!["build".to_string(), src.clone()];
                v.extend(["--target-fd".into(), TARGET_FD.to_string(), "--yes-erase".into()]);
                if let Some(l) = label {
                    v.extend(["--label".into(), l.clone()]);
                }
                v.push("--json".into());
                vec![v]
            }
            Request::Info { .. } => vec![info_args()],
        }
    }
}

/// Non-UTF-8 is rejected outright: the GUI only ever sends UTF-8 and it keeps every later check
/// a plain string check.
fn utf8(args: &[OsString]) -> Result<Vec<&str>, String> {
    args.iter().map(|a| a.to_str().ok_or_else(|| "arguments must be UTF-8".to_string())).collect()
}

/// Set a once-only flag value, refusing duplicates.
fn once<T>(slot: &mut Option<T>, flag: &str, v: Result<T, String>) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("duplicate flag {flag}"));
    }
    *slot = Some(v?);
    Ok(())
}

/// The value after flag `f`.
fn val<'a>(it: &mut impl Iterator<Item = &'a str>, f: &str) -> Result<&'a str, String> {
    it.next().ok_or_else(|| format!("{f} needs a value"))
}

/// Parse the helper's argv (without argv[0]).
pub fn parse(args: &[OsString]) -> Result<Request, String> {
    let args = utf8(args)?;
    let (op, rest) = args.split_first().ok_or("missing operation")?;
    let Some((dev, flags)) = rest.split_first() else {
        return Err(format!("usage: {op} <dev> --expect <key> ..."));
    };
    let dev = parse_dev(dev)?;
    let mut it = flags.iter().copied();
    let mut expect = None;
    match *op {
        "add" => {
            let (mut skip, mut items) = (false, Vec::new());
            while let Some(f) = it.next() {
                match f {
                    "--expect" => once(&mut expect, f, val(&mut it, f).and_then(check_key))?,
                    "--skip-existing" if !skip => skip = true,
                    "--skip-existing" => return Err(format!("duplicate flag {f}")),
                    "--item" => {
                        let src = check_src(val(&mut it, f)?)?;
                        let to = check_to(val(&mut it, f)?)?;
                        if items.len() == MAX_ITEMS {
                            return Err(format!("at most {MAX_ITEMS} --item values"));
                        }
                        items.push(Item { src, to });
                    }
                    _ => return Err("unexpected argument for add".into()),
                }
            }
            if items.is_empty() {
                return Err("add needs at least one --item <src> <to>".into());
            }
            let expect = expect.ok_or("add requires --expect")?;
            Ok(Request::Add { dev, expect, skip_existing: skip, items })
        }
        "build" => {
            let (mut confirm, mut label, mut src) = (None, None, None);
            while let Some(f) = it.next() {
                match f {
                    "--expect" => once(&mut expect, f, val(&mut it, f).and_then(check_key))?,
                    "--confirm" => once(&mut confirm, f, val(&mut it, f).and_then(check_key))?,
                    "--label" => once(&mut label, f, val(&mut it, f).and_then(check_label))?,
                    s if s.starts_with('/') && src.is_none() => src = Some(check_src(s)?),
                    _ => return Err("unexpected argument for build".into()),
                }
            }
            let expect = expect.ok_or("build requires --expect")?;
            // Erasing is never implied: the caller must echo the drive's label (or serial).
            let confirm = confirm.ok_or("build requires --confirm")?;
            let src = src.ok_or("build requires a source directory")?;
            Ok(Request::Build { dev, expect, confirm, label, src })
        }
        "info" => {
            while let Some(f) = it.next() {
                match f {
                    "--expect" => once(&mut expect, f, val(&mut it, f).and_then(check_key))?,
                    _ => return Err("unexpected argument for info".into()),
                }
            }
            Ok(Request::Info { dev, expect: expect.ok_or("info requires --expect")? })
        }
        _ => Err("unknown operation".into()),
    }
}

/// Parse flashcp-trust's argv (without argv[0]): exactly `<dev> --expect <key>`.
pub fn parse_trust(args: &[OsString]) -> Result<TrustRequest, String> {
    match utf8(args)?.as_slice() {
        [dev, "--expect", key, "--wwn", wwn, "--model", model, "--size", size] => {
            let field = |v: &str| {
                if v.is_empty() || v.chars().count() > 256 || v.chars().any(char::is_control) {
                    Err("--wwn/--model must be 1-256 characters without control characters (\"-\" for none)"
                        .to_string())
                } else {
                    Ok(v.to_string())
                }
            };
            let size: u64 = size
                .parse()
                .ok()
                .filter(|&n: &u64| n > 0 && size.bytes().all(|b| b.is_ascii_digit()))
                .ok_or("--size must be a positive number of bytes")?;
            Ok(TrustRequest {
                dev: parse_dev(dev)?,
                shown: crate::identity::Identity { key: check_key(key)?, wwn: field(wwn)?, model: field(model)?, size },
            })
        }
        _ => Err("usage: flashcp-trust <dev> --expect <key> --wwn <wwn|-> --model <model|-> --size <bytes>".into()),
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

/// `--expect` / `--confirm`: 1-128 chars, no control characters. These are only ever compared
/// against values the helper reads itself, never forwarded.
pub fn check_key(s: &str) -> Result<String, String> {
    let n = s.chars().count();
    if n == 0 || n > MAX_KEY || s.chars().any(char::is_control) {
        return Err(format!("--expect/--confirm must be 1-{MAX_KEY} characters without control characters"));
    }
    Ok(s.to_string())
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
        return Err("destination must start with /".into());
    }
    if s.len() > MAX_TO || s.chars().any(char::is_control) || s.split('/').any(|c| c == "..") {
        return Err("destination is invalid".into());
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

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    fn p(a: &[&str]) -> Result<Request, String> {
        parse(&os(a))
    }

    fn runs(a: &[&str]) -> Vec<Vec<String>> {
        p(a).unwrap().flashcp_runs()
    }

    fn add(dev: &str, extra: &[&str]) -> Vec<String> {
        [&["add", dev, "--expect", "SER1"], extra].concat().iter().map(|s| s.to_string()).collect()
    }

    fn pa(dev: &str, extra: &[&str]) -> Result<Request, String> {
        parse(&add(dev, extra).iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn add_builds_one_run_per_item() {
        let r = p(&["add", "/dev/sdb1", "--expect", "SER1", "--item", "/home/u/a", "/Photos", "--item", "/b c", "/"])
            .unwrap();
        assert_eq!(r.expect(), "SER1");
        assert!(r.writes());
        assert_eq!(
            r.flashcp_runs(),
            [
                vec!["add", "/home/u/a", "--target-fd", "3", "--to", "/Photos", "--json"],
                vec!["add", "/b c", "--target-fd", "3", "--to", "/", "--json"],
            ]
        );
        // Flag order does not matter; --skip-existing applies to every run.
        let r = runs(&["add", "/dev/sdc", "--item", "/x", "/ä/b c", "--skip-existing", "--expect", "K"]);
        assert_eq!(r, [vec!["add", "/x", "--target-fd", "3", "--to", "/ä/b c", "--skip-existing", "--json"]]);
    }

    #[test]
    fn add_item_count_limits() {
        assert!(pa("/dev/sdb1", &[]).unwrap_err().contains("at least one"));
        let mut many: Vec<String> = Vec::new();
        for i in 0..MAX_ITEMS {
            many.extend(["--item".into(), format!("/src{i}"), format!("/d{i}")]);
        }
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(pa("/dev/sdb1", &refs).unwrap().flashcp_runs().len(), 256);
        let mut more = refs.clone();
        more.extend(["--item", "/one", "/more"]);
        assert!(pa("/dev/sdb1", &more).unwrap_err().contains("at most 256"));
    }

    #[test]
    fn build_and_info_shapes() {
        assert_eq!(
            runs(&["build", "/dev/sdb", "--expect", "S", "--confirm", "MY STICK", "--label", "MY STICK", "/home/u/d"]),
            [vec!["build", "/home/u/d", "--target-fd", "3", "--yes-erase", "--label", "MY STICK", "--json"]]
        );
        let r = p(&["build", "/dev/sdaa12", "/d", "--confirm", "x", "--expect", "S"]).unwrap();
        assert_eq!(
            r,
            Request::Build {
                dev: parse_dev("/dev/sdaa12").unwrap(),
                expect: "S".into(),
                confirm: "x".into(),
                label: None,
                src: "/d".into()
            }
        );
        assert_eq!(r.flashcp_runs(), [vec!["build", "/d", "--target-fd", "3", "--yes-erase", "--json"]]);
        let r = p(&["info", "/dev/sdz9", "--expect", "S"]).unwrap();
        assert!(!r.writes());
        assert_eq!(r.flashcp_runs(), [vec!["info", "--target-fd", "3", "--json"]]);
        assert_eq!(info_args(), ["info", "--target-fd", "3", "--json"]);
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
            assert!(p(&["info", d, "--expect", "S"]).is_err(), "{d:?} accepted");
            assert!(pa(d, &["--item", "/x", "/"]).is_err(), "{d:?} accepted");
            assert!(parse_trust(&os(&[d, "--expect", "S"])).is_err(), "{d:?} accepted");
        }
    }

    #[test]
    fn bad_args_rejected() {
        let cases: &[&[&str]] = &[
            &[],
            &["frobnicate", "/dev/sdb"],
            &["ADD", "/dev/sdb1", "--expect", "S", "--item", "/x", "/"],
            &["add"],
            &["add", "/dev/sdb1", "--item", "/x", "/"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "x", "/"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "-rf", "/"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "./x", "/"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x\0y", "/"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "../etc"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/a/../../b"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/a/.."],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "a"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/a\nb"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/a\0b"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/", "--skip-existing", "--skip-existing"],
            &["add", "/dev/sdb1", "--expect", "S", "--expect", "S", "--item", "/x", "/"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/", "--yes-erase"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/", "/extra"],
            &["add", "/dev/sdb1", "--expect=S", "--item", "/x", "/"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/", "--to", "/a"],
            &["add", "/dev/sdb1", "--expect", "S", "--item", "/x", "/", "--target-fd", "5"],
            &["add", "/dev/sdb1", "--expect", "", "--item", "/x", "/"],
            &["add", "/dev/sdb1", "--expect", "a\tb", "--item", "/x", "/"],
            &["add", "/dev/sdb1", "--expect"],
            &["build", "/dev/sdb", "--expect", "S", "/x"],
            &["build", "/dev/sdb", "--confirm", "C", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "/x", "/y"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--confirm", "C", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "a\nb", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--yes-erase", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--label", "SIXTEEN_CHARS_XX", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--label", "", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--label", "a/b", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--label", "a\tb", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--label", "-x", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "/x", "--label"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--label", "A", "--label", "B", "/x"],
            &["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--skip-existing", "/x"],
            &["info"],
            &["info", "/dev/sdb"],
            &["info", "/dev/sdb", "--expect"],
            &["info", "/dev/sdb", "--expect", "S", "--json"],
            &["info", "/dev/sdb", "--expect", "S", "--expect", "S"],
        ];
        for c in cases {
            assert!(p(c).is_err(), "{c:?} accepted");
        }
        let long = format!("/{}", "a".repeat(MAX_SRC));
        assert!(pa("/dev/sdb1", &["--item", &long, "/"]).is_err());
        let long_to = format!("/{}", "a".repeat(MAX_TO));
        assert!(pa("/dev/sdb1", &["--item", "/x", &long_to]).is_err());
        // 15 chars (multi-byte) is fine.
        assert!(
            p(&["build", "/dev/sdb", "--expect", "S", "--confirm", "C", "--label", "ééééééééééééééé", "/x"]).is_ok()
        );
    }

    #[test]
    fn missing_expect_reported() {
        assert!(pa("/dev/sdb", &["--item", "/x", "/"]).is_ok());
        let e = p(&["add", "/dev/sdb", "--item", "/x", "/"]).unwrap_err();
        assert!(e.contains("--expect"), "{e}");
        let e = p(&["build", "/dev/sdb", "--expect", "S", "/x"]).unwrap_err();
        assert!(e.contains("--confirm"), "{e}");
        let e = p(&["info", "/dev/sdb"]).unwrap_err();
        assert!(e.contains("--expect"), "{e}");
    }

    #[test]
    fn key_limits() {
        let k128 = "é".repeat(MAX_KEY);
        assert_eq!(check_key(&k128).unwrap(), k128);
        assert!(check_key(&"a".repeat(MAX_KEY + 1)).is_err());
        for bad in ["", "a\u{7f}", "a\rb", "\u{1b}[31m", "a\u{85}b"] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
        // Spaces and a leading dash are fine (serials can contain both; values are never forwarded).
        assert!(check_key("-AB 12").is_ok());
    }

    fn tr(dev: &str, key: &str, wwn: &str, model: &str, size: &str) -> Vec<OsString> {
        os(&[dev, "--expect", key, "--wwn", wwn, "--model", model, "--size", size])
    }

    #[test]
    fn trust_args() {
        let r = parse_trust(&tr("/dev/sdb", "4C53000", "-", "DATAMAG 20Gbps", "1000204886016")).unwrap();
        let shown = crate::identity::Identity {
            key: "4C53000".into(),
            wwn: "-".into(),
            model: "DATAMAG 20Gbps".into(),
            size: 1_000_204_886_016,
        };
        assert_eq!(r, TrustRequest { dev: parse_dev("/dev/sdb").unwrap(), shown });
        assert!(parse_trust(&tr("/dev/sdb1", "K", "0x5001", "-", "1")).is_ok());
        let ok = ["/dev/sdb", "--expect", "K", "--wwn", "-", "--model", "M", "--size", "512"];
        let cases: Vec<Vec<&str>> = vec![
            vec![],
            vec!["/dev/sdb"],
            vec!["/dev/sdb", "--expect", "K"],
            ok[..ok.len() - 1].to_vec(),
            [&ok[..], &["extra"]].concat(),
            vec!["/dev/sdb", "--expect", "", "--wwn", "-", "--model", "M", "--size", "512"],
            vec!["/dev/sdb", "--expect", "a\nb", "--wwn", "-", "--model", "M", "--size", "512"],
            vec!["/dev/sdb", "--expect", "K", "--wwn", "", "--model", "M", "--size", "512"],
            vec!["/dev/sdb", "--expect", "K", "--wwn", "-", "--model", "M\tX", "--size", "512"],
            vec!["/dev/sdb", "--expect", "K", "--wwn", "-", "--model", "M", "--size", "0"],
            vec!["/dev/sdb", "--expect", "K", "--wwn", "-", "--model", "M", "--size", "+512"],
            vec!["/dev/sdb", "--expect", "K", "--wwn", "-", "--model", "M", "--size", "1e9"],
            vec!["/dev/sdb", "--model", "M", "--expect", "K", "--wwn", "-", "--size", "512"],
            vec!["--expect", "K", "/dev/sdb", "--wwn", "-", "--model", "M", "--size", "512"],
            vec!["/dev/sdb", "--expect=K", "--wwn", "-", "--model", "M", "--size", "512"],
        ];
        for c in &cases {
            assert!(parse_trust(&os(c)).is_err(), "{c:?} accepted");
        }
        assert!(parse_trust(&tr("/dev/sdb", &"k".repeat(129), "-", "M", "1")).is_err());
    }

    #[test]
    fn non_utf8_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let mut a = os(&["add", "/dev/sdb1", "--expect", "S", "--item"]);
        a.push(OsString::from_vec(b"/x\xff".to_vec()));
        a.push("/".into());
        assert!(parse(&a).is_err());
        let a = vec![OsString::from("/dev/sdb"), "--expect".into(), OsString::from_vec(b"K\xff".to_vec())];
        assert!(parse_trust(&a).is_err());
    }
}
