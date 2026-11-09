//! The administrator's list of drives flashcp may write: `/etc/flashcp/trusted-drives`.
//!
//! One drive per line, tab-separated: `v1<TAB><key><TAB><wwn or -><TAB><model or -><TAB><size>`.
//! `#` lines and blank lines are ignored, malformed lines are skipped (and reported on stderr).
//! Only `flashcp-trust` writes it, atomically and under an flock.

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::identity::{clean, Identity};

const HEADER: &str = "# Drives flashcp may write to (managed by flashcp-trust).\n\
                      # v1<TAB>serial-or-wwn<TAB>wwn-or-<TAB>model-or-<TAB>size-in-bytes\n";

/// Where the trust file lives. Production uses [`TrustStore::system`].
#[derive(Debug, Clone)]
pub struct TrustStore {
    pub dir: PathBuf,
}

/// Parse a trust file. Returns the valid entries and one message per malformed line.
pub fn parse(text: &str) -> (Vec<Identity>, Vec<String>) {
    let (mut out, mut bad) = (Vec::new(), Vec::new());
    for (n, line) in text.split('\n').enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        match parse_line(line) {
            Some(id) => out.push(id),
            None => bad.push(format!("trusted-drives line {}: malformed, ignored", n + 1)),
        }
    }
    (out, bad)
}

fn parse_line(line: &str) -> Option<Identity> {
    let f: Vec<&str> = line.split('\t').collect();
    let [v, key, wwn, model, size] = f.as_slice() else { return None };
    let ok = *v == "v1"
        && [key, wwn, model].iter().all(|s| !s.is_empty() && clean(s))
        && !size.is_empty()
        && size.bytes().all(|b| b.is_ascii_digit());
    if !ok {
        return None;
    }
    let size = size.parse().ok().filter(|&s: &u64| s > 0)?;
    Some(Identity { key: key.to_string(), wwn: wwn.to_string(), model: model.to_string(), size })
}

/// The line `id` is stored as (with trailing newline).
pub fn line(id: &Identity) -> String {
    format!("v1\t{}\t{}\t{}\t{}\n", id.key, id.wwn, id.model, id.size)
}

/// A device matches an entry iff key, wwn, model and size are all equal.
pub fn matches(entries: &[Identity], id: &Identity) -> bool {
    entries.iter().any(|e| e == id)
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn open_raw(p: &Path, flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
    let c = cpath(p)?;
    // SAFETY: valid C string; the fd is owned below.
    let fd = cvt(unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW, mode as libc::c_uint) })?;
    // SAFETY: fresh fd.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The file (or directory) must belong to whoever runs us (root in production) and be writable
/// by nobody else, otherwise its contents could have been planted.
fn check_owner(p: &Path, md: &fs::Metadata) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if md.uid() != euid || md.mode() & 0o022 != 0 {
        return Err(format!("{} has unsafe ownership or permissions", p.display()));
    }
    Ok(())
}

impl TrustStore {
    pub fn system() -> Self {
        TrustStore { dir: "/etc/flashcp".into() }
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("trusted-drives")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join(".lock")
    }

    /// Read the trusted entries. A missing file (or directory) means "nothing trusted".
    pub fn load(&self) -> Result<Vec<Identity>, String> {
        match fs::symlink_metadata(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", self.dir.display())),
            Ok(md) if !md.is_dir() => return Err(format!("{} is not a directory", self.dir.display())),
            Ok(md) => check_owner(&self.dir, &md)?,
        }
        let file = self.file();
        let fd = match open_raw(&file, libc::O_RDONLY, 0) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", file.display())),
            Ok(fd) => fd,
        };
        let f = fs::File::from(fd);
        let md = f.metadata().map_err(|e| format!("{}: {e}", file.display()))?;
        if !md.is_file() {
            return Err(format!("{} is not a regular file", file.display()));
        }
        check_owner(&file, &md)?;
        let text = io::read_to_string(f).map_err(|e| format!("{}: {e}", file.display()))?;
        let (entries, bad) = parse(&text);
        for b in bad {
            eprintln!("flashcp: {b}");
        }
        Ok(entries)
    }

    /// Add `id`. Returns `Ok(false)` if an identical entry was already there (a no-op success).
    /// Existing lines (comments, other drives, even malformed ones) are kept verbatim.
    pub fn add(&self, id: &Identity) -> Result<bool, String> {
        if [&id.key, &id.wwn, &id.model].iter().any(|v| v.is_empty() || !clean(v)) || id.size == 0 {
            return Err("drive identity contains invalid characters".into());
        }
        self.ensure_dir()?;
        let lock = open_raw(&self.lock_path(), libc::O_RDWR | libc::O_CREAT, 0o600)
            .map_err(|e| format!("{}: {e}", self.lock_path().display()))?;
        // SAFETY: valid fd. Released when `lock` is closed.
        cvt(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }).map_err(|e| format!("flock: {e}"))?;

        let file = self.file();
        let old = match fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HEADER.to_string(),
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        if matches(&parse(&old).0, id) {
            return Ok(false);
        }
        let mut new = old;
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(&line(id));
        self.replace(&new).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        drop(lock);
        Ok(true)
    }

    fn ensure_dir(&self) -> Result<(), String> {
        let shown = self.dir.display();
        match fs::DirBuilder::new().mode(0o755).create(&self.dir) {
            Ok(()) => fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("{shown}: {e}"))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {shown}: {e}")),
        }
        let md = fs::symlink_metadata(&self.dir).map_err(|e| format!("{shown}: {e}"))?;
        if !md.is_dir() {
            return Err(format!("{shown} is not a directory"));
        }
        check_owner(&self.dir, &md)
    }

    /// tmp file (O_EXCL, 0644) in the same directory, fsync, rename over the file, fsync dir.
    fn replace(&self, content: &str) -> io::Result<()> {
        let tmp = self.dir.join(format!(".trusted-drives.{}.tmp", std::process::id()));
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
        let fd = match open_raw(&tmp, flags, 0o644) {
            // Only a crashed earlier run of this pid could have left it; we hold the lock.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&tmp)?;
                open_raw(&tmp, flags, 0o644)?
            }
            r => r?,
        };
        let mut f = fs::File::from(fd);
        let res = (|| {
            f.set_permissions(fs::Permissions::from_mode(0o644))?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, self.file())
        })();
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
            return res;
        }
        fs::File::open(&self.dir)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(key: &str) -> Identity {
        Identity { key: key.into(), wwn: "0x5001".into(), model: "Ultra Fit".into(), size: 32010928128 }
    }

    fn store() -> (tempfile::TempDir, TrustStore) {
        let t = tempfile::tempdir().unwrap();
        let s = TrustStore { dir: t.path().join("flashcp") };
        (t, s)
    }

    #[test]
    fn parse_skips_comments_blank_and_malformed() {
        let text = "# comment\n\
                    \n\
                    \t \n\
                    v1\tAA01\t0x5001\tUltra Fit\t32010928128\n\
                    v1\tBB02\t-\t-\t1024\n\
                    v2\tCC03\t-\t-\t1024\n\
                    v1\tDD04\t-\t-\n\
                    v1\tEE05\t-\t-\t1024\textra\n\
                    v1\t\t-\t-\t1024\n\
                    v1\tFF06\t-\t-\t+1024\n\
                    v1\tGG07\t-\t-\t0\n\
                    v1\tHH08\t-\t-\t99999999999999999999999\n\
                    v1\tII09\t\t-\t1024\n\
                    v1\tJJ10\t-\t-\t1024\r\n\
                    v1 KK11 - - 1024\n\
                    \u{20}# indented comment is not a comment\n\
                    v1\tLL12\t-\t-\t2048";
        let (e, bad) = parse(text);
        assert_eq!(
            e,
            [
                Identity { key: "AA01".into(), wwn: "0x5001".into(), model: "Ultra Fit".into(), size: 32010928128 },
                Identity { key: "BB02".into(), wwn: "-".into(), model: "-".into(), size: 1024 },
                Identity { key: "LL12".into(), wwn: "-".into(), model: "-".into(), size: 2048 },
            ]
        );
        assert_eq!(bad.len(), 11, "{bad:?}");
        assert!(bad[0].contains("line 6"));
    }

    #[test]
    fn match_needs_all_four_fields() {
        let e = vec![id("AA01")];
        assert!(matches(&e, &id("AA01")));
        assert!(!matches(&e, &id("AA02")));
        assert!(!matches(&e, &Identity { wwn: "0x5002".into(), ..id("AA01") }));
        assert!(!matches(&e, &Identity { model: "Ultra".into(), ..id("AA01") }));
        assert!(!matches(&e, &Identity { size: 32010928129, ..id("AA01") }));
        assert!(!matches(&[], &id("AA01")));
    }

    #[test]
    fn line_round_trips() {
        let i = Identity { key: "a b".into(), wwn: "-".into(), model: "-".into(), size: 1 };
        assert_eq!(line(&i), "v1\ta b\t-\t-\t1\n");
        assert_eq!(parse(&line(&i)).0, [i]);
    }

    #[test]
    fn add_creates_appends_and_is_idempotent() {
        let (_t, s) = store();
        assert_eq!(s.load().unwrap(), []);
        assert!(s.add(&id("AA01")).unwrap());
        let md = fs::metadata(s.file()).unwrap();
        assert_eq!(md.mode() & 0o7777, 0o644);
        assert_eq!(fs::metadata(&s.dir).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(s.load().unwrap(), [id("AA01")]);
        let first = fs::read_to_string(s.file()).unwrap();
        assert!(first.starts_with("# "));
        // Re-trusting the same drive changes nothing.
        assert!(!s.add(&id("AA01")).unwrap());
        assert_eq!(fs::read_to_string(s.file()).unwrap(), first);
        assert!(s.add(&id("BB02")).unwrap());
        assert_eq!(s.load().unwrap(), [id("AA01"), id("BB02")]);
        // No temp files are left behind.
        let names: Vec<String> =
            fs::read_dir(&s.dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert!(names.iter().all(|n| n == "trusted-drives" || n == ".lock"), "{names:?}");
    }

    #[test]
    fn add_preserves_existing_lines() {
        let (_t, s) = store();
        fs::create_dir(&s.dir).unwrap();
        fs::set_permissions(&s.dir, fs::Permissions::from_mode(0o755)).unwrap();
        let old = "# my notes\nv1\tOLD1\t-\t-\t512\ngarbage line\nv1\tOLD2\t-\t-\t1024";
        fs::write(s.file(), old).unwrap();
        fs::set_permissions(s.file(), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(s.add(&id("NEW1")).unwrap());
        assert_eq!(fs::read_to_string(s.file()).unwrap(), format!("{old}\n{}", line(&id("NEW1"))));
        assert_eq!(fs::metadata(s.file()).unwrap().mode() & 0o7777, 0o644);
        assert_eq!(s.load().unwrap().len(), 3);
    }

    #[test]
    fn add_refuses_bad_values() {
        let (_t, s) = store();
        for bad in [
            Identity { key: "a\tb".into(), ..id("x") },
            Identity { key: "".into(), ..id("x") },
            Identity { model: "m\n".into(), ..id("x") },
            Identity { wwn: "w\u{1}".into(), ..id("x") },
            Identity { size: 0, ..id("x") },
        ] {
            assert!(s.add(&bad).is_err(), "{bad:?}");
        }
        assert!(!s.file().exists());
    }

    #[test]
    fn unsafe_permissions_refused() {
        let (_t, s) = store();
        s.add(&id("AA01")).unwrap();
        fs::set_permissions(s.file(), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(s.load().unwrap_err().contains("unsafe"));
        fs::set_permissions(s.file(), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&s.dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(s.load().unwrap_err().contains("unsafe"));
        assert!(s.add(&id("BB02")).unwrap_err().contains("unsafe"));
        fs::set_permissions(&s.dir, fs::Permissions::from_mode(0o755)).unwrap();
        // A symlinked trust file is not followed.
        fs::remove_file(s.file()).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", s.file()).unwrap();
        assert!(s.load().is_err());
    }

    #[test]
    fn concurrent_adds_keep_every_entry() {
        let (_t, s) = store();
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || s.add(&id(&format!("K{i}"))).unwrap())
            })
            .collect();
        for h in hs {
            // Threads share the pid, so the tmp name could collide without the flock.
            assert!(h.join().unwrap());
        }
        assert_eq!(s.load().unwrap().len(), 8);
    }
}
