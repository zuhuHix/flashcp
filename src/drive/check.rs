//! `flashcp mount --check`: decides, read-only and with a bounded number of reads, whether
//! flashcp serves a volume or the kernel driver gets it. It runs on every plug-in, possibly in
//! a sandbox that allows no file access at all, so it opens nothing but what it is given
//! (sysfs and /proc/self/mountinfo only for the busy test, and only when they are readable),
//! never consults the environment, and reads at most: the boot region (12 sectors), the FAT
//! pages the root, bitmap and up-case chains need (64 KB each), the root directory (refused
//! past [`MAX_ROOT`]), the up-case table (≤ 128 KB) and the drive journal's header.

use super::Fallback;
use crate::volume::{FLAG_DIRTY, Vol, le32};
use std::fs::File;

/// A root directory larger than this is refused (`unsupported`) rather than read.
pub const MAX_ROOT: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckReport {
    /// None: flashcp can serve it.
    pub fallback: Option<Fallback>,
    pub label: Option<String>,
    pub serial: Option<u32>,
    /// An interrupted flashcp commit (or sync) the mount would replay first.
    pub pending_journal: bool,
}

/// What the busy test found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Busy {
    Free,
    /// Mounted (device, mountpoint) or held by another driver (holder device).
    InUse(String),
    /// sysfs or mountinfo could not be read: the caller decides.
    Unknown(String),
}

pub(crate) fn classify(e: &anyhow::Error) -> Fallback {
    let m = format!("{e:#}");
    let unsupported = ["TexFAT", "unsupported", "two allocation bitmaps", "second FAT", "larger than 1 MB"];
    if unsupported.iter().any(|k| m.contains(k)) {
        Fallback::Unsupported(m)
    } else if m.contains("too small") {
        Fallback::TooSmall
    } else if m.contains("no EXFAT signature") {
        Fallback::NotExfat
    } else {
        Fallback::Invalid(m)
    }
}

/// The decision for the volume in `dev` (`len` bytes), see the module doc.
pub fn check(dev: &File, len: u64) -> CheckReport {
    let mut r = CheckReport { fallback: None, label: None, serial: None, pending_journal: false };
    if len < (1 << 20) {
        r.fallback = Some(Fallback::TooSmall);
        return r;
    }
    let mut s0 = [0u8; 512];
    use std::os::unix::fs::FileExt;
    if dev.read_exact_at(&mut s0, 0).is_err() || &s0[3..11] != b"EXFAT   " {
        r.fallback = Some(Fallback::NotExfat);
        return r;
    }
    r.serial = Some(le32(&s0, 100));
    let res = (|| -> anyhow::Result<CheckReport> {
        let f = dev.try_clone()?;
        let mut v = Vol::open_boot(f, len, true, false)?;
        // bound the root before reading it
        let root = le32(&v.sector0, 96);
        let cl = v.chain(root, "root directory", u64::MAX).ok();
        match cl {
            Some(c) if c.len() as u64 * v.cs <= MAX_ROOT => {}
            Some(_) => anyhow::bail!("root directory larger than 1 MB"),
            None => {
                // too long, a loop or out of range: let the full parse say which (bounded)
                v.chain(root, "root directory", MAX_ROOT / v.cs)?;
            }
        }
        let dirty = v.flags & FLAG_DIRTY != 0;
        let mut pending = false;
        if let Some(p) = super::djournal::locate_raw(&mut v, MAX_ROOT)? {
            pending = super::djournal::header_state(&v, &p) == Some(true);
        }
        let v = Vol::open_rest(v, false)?;
        let sync_journal =
            v.dirs[0].children.values().any(|c| crate::volume::is_sync_journal(c) && !super::djournal::is_journal(c));
        pending |= sync_journal;
        let mut out =
            CheckReport { fallback: None, label: Some(v.label.clone()), serial: r.serial, pending_journal: pending };
        if dirty && !pending {
            out.fallback = Some(Fallback::Dirty);
        }
        Ok(out)
    })();
    match res {
        Ok(rep) => rep,
        Err(e) => {
            r.fallback = Some(classify(&e));
            r
        }
    }
}

/// Whether the block device behind `dev` (or a partition or holder of it) is mounted or
/// held. Regular files (images) are never busy here (the mount's own claim decides).
pub fn busy(dev: &File) -> Busy {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(md) = dev.metadata() else { return Busy::Unknown("fstat failed".into()) };
    if !md.file_type().is_block_device() {
        return Busy::Free;
    }
    let id = format!("{}:{}", libc::major(md.rdev()), libc::minor(md.rdev()));
    let devs = match crate::exfat::related_devs(std::path::Path::new(crate::exfat::SYS_BLOCK), &id) {
        Ok(d) => d,
        Err(e) => return Busy::Unknown(format!("sysfs: {e:#}")),
    };
    if devs.len() > 1 {
        // a holder (dm, md, LVM) or a mounted partition below: in use unless none is mounted
        let holders: Vec<&String> = devs.iter().filter(|d| **d != id).collect();
        let hold =
            std::fs::read_dir(format!("{}/{id}/holders", crate::exfat::SYS_BLOCK)).map(|r| r.count()).unwrap_or(0);
        if hold > 0 {
            return Busy::InUse(format!("held by device {}", holders[0]));
        }
    }
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(m) => match crate::exfat::mounted_dev(&m, &devs) {
            Some((d, at)) => Busy::InUse(format!("device {d} is mounted at {at}")),
            None => Busy::Free,
        },
        Err(e) => Busy::Unknown(format!("/proc/self/mountinfo: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::tests::{image, open};

    /// The check's reads through the trace hook (bytes), plus its own boot-sector read.
    fn traced_reads(f: impl FnOnce()) -> u64 {
        let a = crate::sim::trace::test_reads();
        f();
        crate::sim::trace::test_reads() - a + 512
    }

    #[test]
    fn check_decisions_and_read_budget() {
        let img = image("check", 64, 4096);
        // a used volume: the journal exists, files in the root
        let fs = open(&img.path);
        for k in 0..50 {
            let a = fs.create(crate::drive::ROOT, std::ffi::OsStr::new(&format!("f{k}")), true, false).unwrap();
            fs.write(a.ino, 0, b"x").unwrap();
        }
        fs.shutdown().unwrap();
        let f = File::open(&img.path).unwrap();
        let len = f.metadata().unwrap().len();
        let mut rep = None;
        let bytes = traced_reads(|| rep = Some(check(&f, len)));
        let rep = rep.unwrap();
        assert_eq!(rep.fallback, None, "{rep:?}");
        assert!(!rep.pending_journal);
        assert_eq!(rep.label.as_deref(), Some("DRIVE"));
        assert!(bytes <= 2 << 20, "the check read {bytes} bytes");
        eprintln!("check read {bytes} bytes");
        assert_eq!(busy(&f), Busy::Free);
        // dirty without a journal: the kernel's
        use std::os::unix::fs::FileExt;
        let rw = File::options().read(true).write(true).open(&img.path).unwrap();
        let mut fl = [0u8; 2];
        rw.read_exact_at(&mut fl, 106).unwrap();
        rw.write_all_at(&[fl[0] | 2, fl[1]], 106).unwrap();
        assert_eq!(check(&f, len).fallback, Some(Fallback::Dirty));
        // not exFAT
        rw.write_all_at(b"NTFS    ", 3).unwrap();
        assert_eq!(check(&f, len).fallback, Some(Fallback::NotExfat));
        assert_eq!(check(&f, 4096).fallback, Some(Fallback::TooSmall));
    }

    /// Hostile boot regions and root directories: the check never panics, never reads past
    /// its budget, and anything it accepts the full parser accepts too. A bounded, seeded
    /// mutation corpus (FLASHCP_FUZZ_ITERS, default 300).
    #[test]
    fn check_survives_hostile_volumes() {
        let img = image("check-fuzz", 16, 512);
        let fs = open(&img.path);
        let d = fs.mkdir(crate::drive::ROOT, std::ffi::OsStr::new("dir")).unwrap();
        for k in 0..20 {
            let a = fs.create(d.ino, std::ffi::OsStr::new(&format!("file number {k}.txt")), true, false).unwrap();
            fs.write(a.ino, 0, &vec![k as u8; 700]).unwrap();
        }
        fs.shutdown().unwrap();
        let pristine = std::fs::read(&img.path).unwrap();
        let s0 = &pristine[..512];
        let bps = 1usize << s0[108];
        let cs = bps << s0[109];
        let heap = le32(s0, 88) as usize * bps;
        let root = (le32(s0, 96) as usize - 2) * cs + heap;
        let fat = le32(s0, 80) as usize * bps;
        let iters: u64 = std::env::var("FLASHCP_FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
        let mut rng = crate::util::Rng(0xC0FFEE);
        let path = img.dir.join("fuzz.img");
        for it in 0..iters {
            let mut b = pristine.clone();
            // mutate: the boot sector, the FAT head, the root directory
            let regions = [(0usize, 512usize), (fat, 64), (root, cs.min(4096)), (11 * 512, 512)];
            for _ in 0..1 + rng.next() % 8 {
                let (a, l) = regions[(rng.next() % regions.len() as u64) as usize];
                let at = a + (rng.next() % l as u64) as usize;
                match rng.next() % 3 {
                    0 => b[at] = rng.next() as u8,
                    1 => b[at] ^= 1 << (rng.next() % 8),
                    _ => {
                        let v = [0u8, 0xFF, 0x80, 0x7F][(rng.next() % 4) as usize];
                        b[at] = v;
                    }
                }
            }
            if it % 3 == 0 {
                // keep the boot checksum valid so mutations reach the parser past it
                let want = crate::exfat::checksum32(&b[..11 * bps], &[106, 107, 112], 0);
                for k in 0..bps / 4 {
                    b[11 * bps + 4 * k..11 * bps + 4 * k + 4].copy_from_slice(&want.to_le_bytes());
                }
            }
            std::fs::write(&path, &b).unwrap();
            let f = File::open(&path).unwrap();
            let mut rep = None;
            let bytes = traced_reads(|| rep = Some(check(&f, b.len() as u64)));
            assert!(bytes <= 2 << 20, "iteration {it}: the check read {bytes} bytes");
            if rep.unwrap().fallback.is_none() {
                // accepted: mounting must work (or refuse cleanly), and reads must not panic
                let rw = File::options().read(true).write(true).open(&path).unwrap();
                if let Ok(fs) = crate::drive::tests::open_with(
                    &path,
                    std::sync::Arc::new(crate::drive::dev::FileDev::new(std::sync::Arc::new(rw))),
                    crate::drive::tests::manual(),
                ) {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut stack = vec![crate::drive::ROOT];
                        while let Some(i) = stack.pop() {
                            for (c, _, is_dir) in fs.readdir(i).unwrap_or_default() {
                                if is_dir {
                                    stack.push(c);
                                } else {
                                    let _ = fs.read(c, 0, 4096);
                                }
                            }
                        }
                    }))
                    .map_err(|_| panic!("iteration {it}: reading an accepted volume panicked"));
                }
            }
        }
    }
}
