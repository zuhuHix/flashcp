//! Strict argv parsing for both programs. The helper accepts exactly five command shapes and
//! builds every flashcp argv itself from the validated values, so nothing the caller sends is
//! forwarded verbatim.

use std::ffi::OsString;

/// Longest source path we accept (PATH_MAX).
const MAX_SRC: usize = 4096;
/// Longest destination directory inside the volume (`add --item .. <to>`), and longest
/// `pull --from` folder inside the volume.
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
/// Where `optimize` keeps its temporary copy, relative to the invoking user's passwd home.
pub const OPTIMIZE_TEMP: &str = ".cache/flashcp/optimize";
/// Longest home directory we build the optimize temp path from (leaves room under PATH_MAX).
const MAX_HOME: usize = 3072;

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

/// What `pull` does with files that already exist in the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// Neither flag: flashcp refuses to start if anything would collide.
    Fail,
    Skip,
    Overwrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Add {
        dev: DevName,
        expect: String,
        skip_existing: bool,
        items: Vec<Item>,
    },
    Build {
        dev: DevName,
        expect: String,
        confirm: String,
        label: Option<String>,
        src: String,
    },
    Info {
        dev: DevName,
        expect: String,
    },
    /// Copy files OFF the volume into `dest`, a local directory. `dest` is never touched by the
    /// helper (it runs as root); flashcp opens or creates it after the privilege drop.
    Pull {
        dev: DevName,
        expect: String,
        from: Option<String>,
        existing: Existing,
        dest: String,
    },
    /// Rebuild the volume in the flashcp layout without losing data (copy off, verify, erase,
    /// rebuild, verify, delete the copy). No caller-chosen path: the temporary copy always goes
    /// to [`optimize_temp`] of the invoking user's home, which the helper never touches itself.
    /// Re-running the same request after an interruption resumes (flashcp detects the job).
    Optimize {
        dev: DevName,
        expect: String,
        dry_run: bool,
    },
}

/// How the helper opens the device for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `O_RDONLY`: `info` only reads the boot region, briefly.
    Read,
    /// `O_RDONLY|O_EXCL`: `pull` reads the whole volume and must see it stay unmounted and
    /// unclaimed for the whole run (see SECURITY-NOTES.md).
    ReadExclusive,
    /// `O_RDWR|O_EXCL`: `add`, `build` and `optimize` (its dry run too: flashcp requires a
    /// read-write fd for optimize, and the plan is only meaningful on an unclaimed volume).
    WriteExclusive,
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
            Request::Add { dev, .. }
            | Request::Build { dev, .. }
            | Request::Info { dev, .. }
            | Request::Pull { dev, .. }
            | Request::Optimize { dev, .. } => dev,
        }
    }

    pub fn expect(&self) -> &str {
        match self {
            Request::Add { expect, .. }
            | Request::Build { expect, .. }
            | Request::Info { expect, .. }
            | Request::Pull { expect, .. }
            | Request::Optimize { expect, .. } => expect,
        }
    }

    /// `info` reads briefly, `pull` reads but holds the device exclusively, `add`, `build` and
    /// `optimize` write and hold it exclusively.
    pub fn access(&self) -> Access {
        match self {
            Request::Info { .. } => Access::Read,
            Request::Pull { .. } => Access::ReadExclusive,
            Request::Add { .. } | Request::Build { .. } | Request::Optimize { .. } => Access::WriteExclusive,
        }
    }

    /// The exact argvs (without argv[0]) of the flashcp runs, in order: one per item for `add`,
    /// one for `build`/`info`/`pull`/`optimize`. `home` is the invoking user's home directory
    /// from the passwd database (never from argv or the environment); only `optimize` uses it,
    /// and it fails only when that home can't hold the temporary copy (see [`optimize_temp`]).
    pub fn flashcp_runs(&self, home: &str) -> Result<Vec<Vec<String>>, String> {
        Ok(match self {
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
            Request::Pull { from, existing, dest, .. } => {
                let mut v = vec!["pull".to_string(), "--target-fd".into(), TARGET_FD.to_string(), dest.clone()];
                if let Some(f) = from {
                    v.extend(["--from".into(), f.clone()]);
                }
                match existing {
                    Existing::Fail => {}
                    Existing::Skip => v.push("--skip-existing".into()),
                    Existing::Overwrite => v.push("--overwrite".into()),
                }
                v.push("--json".into());
                vec![v]
            }
            Request::Optimize { dry_run, .. } => {
                let mut v = vec!["optimize".to_string(), "--target-fd".into(), TARGET_FD.to_string()];
                v.extend(["--temp".into(), optimize_temp(home)?, "--yes".into()]);
                if *dry_run {
                    v.push("--dry-run".into());
                }
                v.push("--json".into());
                vec![v]
            }
        })
    }
}

/// The directory `optimize` keeps its temporary copy in: `<home>/.cache/flashcp/optimize`.
///
/// Built from the passwd home only, as a string: the helper (root) never stats, creates or
/// resolves it. flashcp creates it after the privilege drop, so the kernel applies the user's
/// own rights; it refuses it if it is on RAM (no `--temp-volatile-…` flag is ever passed) or on
/// the drive being optimized. Being fixed, every run (and every resume) of every drive uses the
/// same place, so an interrupted job is always found again. Refused: a relative or root (`/`)
/// home, `.`/`..` components, control characters, or more than 3072 bytes.
pub fn optimize_temp(home: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("cannot use your home directory {home:?} for optimize's temporary copy: {why}"));
    let trimmed = home.trim_end_matches('/');
    if !home.starts_with('/') {
        return bad("it is not an absolute path");
    }
    if trimmed.is_empty() {
        return bad("it is the root directory");
    }
    if home.len() > MAX_HOME || home.chars().any(char::is_control) {
        return bad("it is too long or contains control characters");
    }
    if trimmed.split('/').skip(1).any(|c| c.is_empty() || c == "." || c == "..") {
        return bad("it is not in canonical form");
    }
    Ok(format!("{trimmed}/{OPTIMIZE_TEMP}"))
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
        "pull" => {
            let (mut from, mut existing, mut dest) = (None, None, None);
            while let Some(f) = it.next() {
                match f {
                    "--expect" => once(&mut expect, f, val(&mut it, f).and_then(check_key))?,
                    "--from" => once(&mut from, f, val(&mut it, f).and_then(check_from))?,
                    "--skip-existing" | "--overwrite" if existing.is_some() => {
                        return Err("give at most one of --skip-existing and --overwrite".into())
                    }
                    "--skip-existing" => existing = Some(Existing::Skip),
                    "--overwrite" => existing = Some(Existing::Overwrite),
                    s if s.starts_with('/') && dest.is_none() => dest = Some(check_dest(s)?),
                    _ => return Err("unexpected argument for pull".into()),
                }
            }
            let expect = expect.ok_or("pull requires --expect")?;
            let dest = dest.ok_or("pull requires a destination directory")?;
            Ok(Request::Pull { dev, expect, from, existing: existing.unwrap_or(Existing::Fail), dest })
        }
        "optimize" => {
            let mut dry_run = false;
            while let Some(f) = it.next() {
                match f {
                    "--expect" => once(&mut expect, f, val(&mut it, f).and_then(check_key))?,
                    "--dry-run" if !dry_run => dry_run = true,
                    "--dry-run" => return Err(format!("duplicate flag {f}")),
                    _ => return Err("unexpected argument for optimize".into()),
                }
            }
            Ok(Request::Optimize { dev, expect: expect.ok_or("optimize requires --expect")?, dry_run })
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

/// `pull`'s local destination directory. The helper runs as root, so it deliberately does NOT
/// look at this path (no stat, no ownership or permission check, no canonicalisation): any such
/// check would be a root-side check of a user-controlled path, racy and confusable through
/// symlinks. It only makes sure the value is an unambiguous path argument (absolute, so it can
/// never parse as a flag, and resolves the same with flashcp's cwd `/`). flashcp opens or creates
/// it after the privilege drop, so the kernel applies the invoking user's own rights.
fn check_dest(s: &str) -> Result<String, String> {
    if !s.starts_with('/') {
        return Err("destination must be an absolute path".into());
    }
    if s.contains('\0') || s.len() > MAX_SRC {
        return Err("destination path is invalid or too long".into());
    }
    Ok(s.to_string())
}

/// `pull --from`: a folder inside the volume, relative to its root, in canonical form:
/// non-empty components separated by single `/`, none `.` or `..`, no leading `/` or `-` (so it
/// can never be read as a flag), no control characters, at most 1024 bytes.
pub fn check_from(s: &str) -> Result<String, String> {
    let bad = || "--from must be a relative folder on the drive, like \"Photos/2026\"".to_string();
    if s.is_empty() || s.len() > MAX_TO || s.starts_with('-') || s.chars().any(char::is_control) {
        return Err(bad());
    }
    if s.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(bad());
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

    const HOME: &str = "/home/u";

    fn runs(a: &[&str]) -> Vec<Vec<String>> {
        p(a).unwrap().flashcp_runs(HOME).unwrap()
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
        assert_eq!(r.access(), Access::WriteExclusive);
        assert_eq!(
            r.flashcp_runs(HOME).unwrap(),
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
        assert_eq!(pa("/dev/sdb1", &refs).unwrap().flashcp_runs(HOME).unwrap().len(), 256);
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
        assert_eq!(r.flashcp_runs(HOME).unwrap(), [vec!["build", "/d", "--target-fd", "3", "--yes-erase", "--json"]]);
        let r = p(&["info", "/dev/sdz9", "--expect", "S"]).unwrap();
        assert_eq!(r.access(), Access::Read);
        assert_eq!(r.flashcp_runs(HOME).unwrap(), [vec!["info", "--target-fd", "3", "--json"]]);
        assert_eq!(info_args(), ["info", "--target-fd", "3", "--json"]);
    }

    #[test]
    fn pull_shapes() {
        let r = p(&["pull", "/dev/sdb1", "--expect", "S", "/home/u/Downloads/STICK-2026-11-09"]).unwrap();
        assert_eq!(r.access(), Access::ReadExclusive);
        assert_eq!(
            r,
            Request::Pull {
                dev: parse_dev("/dev/sdb1").unwrap(),
                expect: "S".into(),
                from: None,
                existing: Existing::Fail,
                dest: "/home/u/Downloads/STICK-2026-11-09".into()
            }
        );
        assert_eq!(
            r.flashcp_runs(HOME).unwrap(),
            [vec!["pull", "--target-fd", "3", "/home/u/Downloads/STICK-2026-11-09", "--json"]]
        );
        // Flag order does not matter; the flashcp argv is always built in one fixed order.
        assert_eq!(
            runs(&["pull", "/dev/sdc", "/d e", "--skip-existing", "--from", "DCIM/100 CANON", "--expect", "K"]),
            [vec!["pull", "--target-fd", "3", "/d e", "--from", "DCIM/100 CANON", "--skip-existing", "--json"]]
        );
        assert_eq!(
            runs(&["pull", "/dev/sdc", "--expect", "K", "--overwrite", "/d", "--from", "ä"]),
            [vec!["pull", "--target-fd", "3", "/d", "--from", "ä", "--overwrite", "--json"]]
        );
        // The destination is passed through as given: the helper never resolves it as root.
        assert_eq!(runs(&["pull", "/dev/sdc", "--expect", "K", "/a/../b"])[0][3], "/a/../b");
        let long = format!("/{}", "a".repeat(MAX_SRC - 1));
        assert!(p(&["pull", "/dev/sdc", "--expect", "K", &long]).is_ok());
        let f = "a".repeat(MAX_TO);
        assert!(p(&["pull", "/dev/sdc", "--expect", "K", "--from", &f, "/d"]).is_ok());
    }

    #[test]
    fn pull_bad_args_rejected() {
        let cases: &[&[&str]] = &[
            &["pull", "/dev/sdb1"],
            &["pull", "/dev/sdb1", "/d"],
            &["pull", "/dev/sdb1", "--expect", "S"],
            &["pull", "/dev/sdb1", "--expect", "S", "d"],
            &["pull", "/dev/sdb1", "--expect", "S", "./d"],
            &["pull", "/dev/sdb1", "--expect", "S", "-d"],
            &["pull", "/dev/sdb1", "--expect", "S", "~/d"],
            &["pull", "/dev/sdb1", "--expect", "S", ""],
            &["pull", "/dev/sdb1", "--expect", "S", "/d\0x"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "/e"],
            &["pull", "/dev/sdb1", "--expect", "S", "--expect", "S", "/d"],
            &["pull", "/dev/sdb1", "--expect=S", "/d"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--skip-existing", "--overwrite"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--overwrite", "--skip-existing"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--skip-existing", "--skip-existing"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--overwrite", "--overwrite"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--from"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--from", "A", "--from", "B"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--from=A"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--target-fd", "5"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--json"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--chunk-kb", "1"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--item", "/x", "/"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--to", "/"],
            &["pull", "/dev/sdb1", "--expect", "S", "/d", "--yes-erase"],
            &["PULL", "/dev/sdb1", "--expect", "S", "/d"],
            &["pull", "/dev/nvme0n1", "--expect", "S", "/d"],
        ];
        for c in cases {
            assert!(p(c).is_err(), "{c:?} accepted");
        }
        let long = format!("/{}", "a".repeat(MAX_SRC));
        assert!(p(&["pull", "/dev/sdc", "--expect", "K", &long]).is_err());
        let e = p(&["pull", "/dev/sdb1", "--expect", "S"]).unwrap_err();
        assert!(e.contains("destination"), "{e}");
        let e = p(&["pull", "/dev/sdb1", "/d"]).unwrap_err();
        assert!(e.contains("--expect"), "{e}");
    }

    #[test]
    fn pull_from_must_be_relative_and_canonical() {
        for ok in ["DCIM", "DCIM/100CANON", "a b/c", "ä/ö", "a-b", "a/-b", ".hidden", "a/..b", "..."] {
            assert_eq!(check_from(ok).unwrap(), ok, "{ok:?}");
        }
        let long = "a".repeat(MAX_TO + 1);
        for bad in [
            "",
            "/",
            "/DCIM",
            "DCIM/",
            "a//b",
            ".",
            "./a",
            "a/.",
            "..",
            "../a",
            "a/..",
            "a/../b",
            "-a",
            "--x",
            "a\nb",
            "a\tb",
            "a\0b",
            "a\u{7f}",
            "\u{1b}[31m",
            &long,
        ] {
            assert!(check_from(bad).is_err(), "{bad:?} accepted");
            assert!(p(&["pull", "/dev/sdb", "--expect", "S", "--from", bad, "/d"]).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn optimize_shapes() {
        let r = p(&["optimize", "/dev/sdb1", "--expect", "S"]).unwrap();
        assert_eq!(r, Request::Optimize { dev: parse_dev("/dev/sdb1").unwrap(), expect: "S".into(), dry_run: false });
        // Writes: O_RDWR|O_EXCL, like add and build, for the dry run too.
        assert_eq!(r.access(), Access::WriteExclusive);
        assert_eq!(
            r.flashcp_runs(HOME).unwrap(),
            [vec!["optimize", "--target-fd", "3", "--temp", "/home/u/.cache/flashcp/optimize", "--yes", "--json"]]
        );
        let r = p(&["optimize", "/dev/sdc", "--dry-run", "--expect", "K"]).unwrap();
        assert_eq!(r.access(), Access::WriteExclusive);
        assert_eq!(
            r.flashcp_runs(HOME).unwrap(),
            [vec![
                "optimize",
                "--target-fd",
                "3",
                "--temp",
                "/home/u/.cache/flashcp/optimize",
                "--yes",
                "--dry-run",
                "--json"
            ]]
        );
        // Resuming is the same request again: the argv (and so the temp dir) never changes
        // between runs, so flashcp always finds the interrupted job.
        let a = ["optimize", "/dev/sdb1", "--expect", "S"];
        assert_eq!(runs(&a), runs(&a));
        // The temp dir comes from the home handed in (the passwd entry), nothing else.
        assert_eq!(
            p(&a).unwrap().flashcp_runs("/srv/home/ä b/").unwrap()[0][4],
            "/srv/home/ä b/.cache/flashcp/optimize"
        );
    }

    #[test]
    fn optimize_bad_args_rejected() {
        let cases: &[&[&str]] = &[
            &["optimize"],
            &["optimize", "/dev/sdb1"],
            &["optimize", "/dev/sdb1", "--dry-run"],
            &["optimize", "/dev/sdb1", "--expect"],
            &["optimize", "/dev/sdb1", "--expect", ""],
            &["optimize", "/dev/sdb1", "--expect", "S", "--expect", "S"],
            &["optimize", "/dev/sdb1", "--expect=S"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--dry-run", "--dry-run"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--dry-run=1"],
            // No caller-chosen temp dir, no volatile-temp override, no --force, no raw flags.
            &["optimize", "/dev/sdb1", "--expect", "S", "--temp", "/tmp"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--temp=/tmp"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--temp", "/tmp", "--temp-volatile-i-accept-data-loss"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--temp-volatile-i-accept-data-loss"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--force"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--yes"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--json"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--target-fd", "5"],
            &["optimize", "/dev/sdb1", "--expect", "S", "--confirm", "S"],
            &["optimize", "/dev/sdb1", "--expect", "S", "/home/u"],
            &["optimize", "/dev/sdb1", "--expect", "S", "/dev/sdb1"],
            &["OPTIMIZE", "/dev/sdb1", "--expect", "S"],
            &["optimize", "/dev/nvme0n1", "--expect", "S"],
            &["optimize", "/dev/sdb1/../sda", "--expect", "S"],
        ];
        for c in cases {
            assert!(p(c).is_err(), "{c:?} accepted");
        }
        assert!(p(&["optimize", "/dev/sdb1"]).unwrap_err().contains("--expect"));
    }

    #[test]
    fn optimize_temp_rules() {
        assert_eq!(optimize_temp("/home/u").unwrap(), "/home/u/.cache/flashcp/optimize");
        assert_eq!(optimize_temp("/home/u//").unwrap(), "/home/u/.cache/flashcp/optimize");
        assert_eq!(optimize_temp("/var/lib/x y").unwrap(), "/var/lib/x y/.cache/flashcp/optimize");
        let long = format!("/{}", "a".repeat(MAX_HOME));
        for bad in [
            "",
            "/",
            "//",
            "home/u",
            "./u",
            "~",
            "/home/../root",
            "/home/./u",
            "/home//u",
            "/home/u\n",
            "/a\tb",
            "/a\0b",
            &long,
        ] {
            assert!(optimize_temp(bad).is_err(), "{bad:?} accepted");
            let r = Request::Optimize { dev: parse_dev("/dev/sdb").unwrap(), expect: "S".into(), dry_run: false };
            assert!(r.flashcp_runs(bad).is_err(), "{bad:?} accepted");
        }
        // Other ops never look at the home.
        assert!(p(&["info", "/dev/sdb", "--expect", "S"]).unwrap().flashcp_runs("").is_ok());
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
