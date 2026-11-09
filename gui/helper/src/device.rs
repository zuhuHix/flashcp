//! Decide whether a `/dev/sdX[N]` node is a USB drive that is safe to hand to the user.
//!
//! Everything is read through [`Roots`] and a [`DevStat`] so the whole decision can be tested
//! against a fake sysfs/udev/proc tree. Any missing or unreadable input is a rejection
//! (fail closed).

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::args::DevName;
use crate::mounts;

/// Mount points that mark a disk as the running system's disk.
const SYSTEM_MOUNTS: &[&str] = &["/", "/boot", "/boot/efi", "/usr", "/var", "/home", "/etc"];

/// Where the kernel/udev state lives. Production uses [`Roots::system`].
#[derive(Debug, Clone)]
pub struct Roots {
    pub dev: PathBuf,
    pub sys: PathBuf,
    pub udev_data: PathBuf,
    pub proc_mountinfo: PathBuf,
    pub proc_swaps: PathBuf,
}

impl Roots {
    pub fn system() -> Self {
        Roots {
            dev: "/dev".into(),
            sys: "/sys".into(),
            udev_data: "/run/udev/data".into(),
            proc_mountinfo: "/proc/self/mountinfo".into(),
            proc_swaps: "/proc/swaps".into(),
        }
    }
}

/// The parts of `lstat`/`fstat` we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStat {
    pub is_block: bool,
    pub is_symlink: bool,
    pub major: u32,
    pub minor: u32,
}

impl NodeStat {
    pub fn from_libc(st: &libc::stat) -> Self {
        let fmt = st.st_mode & libc::S_IFMT;
        let rdev = st.st_rdev;
        // glibc's major()/minor() encoding, spelled out to avoid depending on libc's fn signatures.
        let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
        let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
        NodeStat {
            is_block: fmt == libc::S_IFBLK,
            is_symlink: fmt == libc::S_IFLNK,
            major: major as u32,
            minor: minor as u32,
        }
    }
}

/// `fstat(2)` of an open fd.
pub fn fstat(fd: BorrowedFd) -> io::Result<NodeStat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and out-pointer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(NodeStat::from_libc(&st))
}

/// Open `/dev/<name>` (under `roots.dev`) the way the helper hands it to flashcp:
/// `O_RDWR|O_EXCL` for writing ops (the kernel refuses `O_EXCL` on a block device that is mounted
/// or otherwise claimed, and keeps others from claiming it while we hold it), `O_RDONLY` for
/// reading ones. Always `O_NOFOLLOW|O_CLOEXEC`. Validation happens afterwards, on this fd.
pub fn open_device(roots: &Roots, dev: &DevName, write: bool) -> io::Result<OwnedFd> {
    let path = roots.dev.join(&dev.name);
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mode = if write { libc::O_RDWR | libc::O_EXCL } else { libc::O_RDONLY };
    // SAFETY: valid C string; the returned fd is owned below.
    let raw = unsafe { libc::open(c.as_ptr(), mode | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh fd we own.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// A device that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidDevice {
    pub name: DevName,
    pub major: u32,
    pub minor: u32,
    /// `MAJ:MIN` of the whole disk (the target itself for a whole-disk target).
    pub disk_majmin: String,
    /// The whole disk's sysfs directory.
    pub disk_dir: PathBuf,
}

/// A block device as seen in sysfs.
struct Node {
    name: String,
    majmin: String,
    dir: PathBuf,
}

fn read_trim(p: &Path) -> Result<String, String> {
    fs::read_to_string(p).map(|s| s.trim().to_string()).map_err(|e| format!("cannot read {}: {e}", p.display()))
}

/// `usb1`, `usb2`, ...: the USB bus (root hub) directories in a sysfs device path.
fn is_usb_bus(c: Component) -> bool {
    let s = c.as_os_str().as_bytes();
    s.len() > 3 && s.starts_with(b"usb") && s[3..].iter().all(u8::is_ascii_digit)
}

/// Properties (`E:KEY=VALUE` lines) from the udev database entry for block device MAJ:MIN.
pub(crate) fn udev_props(roots: &Roots, majmin: &str) -> Result<Vec<(String, String)>, String> {
    let p = roots.udev_data.join(format!("b{majmin}"));
    let text = fs::read_to_string(&p).map_err(|_| format!("no udev database entry for {majmin}"))?;
    Ok(text
        .lines()
        .filter_map(|l| l.strip_prefix("E:"))
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect())
}

fn has(props: &[(String, String)], key: &str, val: &str) -> bool {
    props.iter().any(|(k, v)| k == key && v == val)
}

/// Does udev's view agree the device hangs off USB? `ID_BUS=usb` alone is not enough: for UAS
/// drives (most fast USB SSDs) udev's SCSI probe overwrites it with `ID_BUS=scsi`, but the
/// usb_id builtin still records `ID_USB_DRIVER` and the `-usb-` hop in `ID_PATH`.
fn udev_says_usb(props: &[(String, String)]) -> bool {
    props
        .iter()
        .any(|(k, v)| (k == "ID_BUS" && v == "usb") || k == "ID_USB_DRIVER" || (k == "ID_PATH" && v.contains("-usb-")))
}

/// Partitions of a disk: child directories that carry a `partition` file.
fn partitions(disk_dir: &Path, disk_name: &str) -> Result<Vec<Node>, String> {
    let rd = fs::read_dir(disk_dir).map_err(|e| format!("cannot list {}: {e}", disk_dir.display()))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let dir = e.path();
        if name.starts_with(disk_name) && dir.join("partition").is_file() {
            out.push(Node { majmin: read_trim(&dir.join("dev"))?, name, dir });
        }
    }
    Ok(out)
}

fn holders_empty(dir: &Path) -> Result<bool, String> {
    let mut rd = fs::read_dir(dir.join("holders")).map_err(|e| format!("cannot list holders: {e}"))?;
    Ok(rd.next().is_none())
}

/// Run every check on the device the caller already OPENED as `dev`; `st` is `fstat` of that fd,
/// so everything below is keyed by the rdev of the device that will actually be written, not by
/// whatever the path points at now.
///
/// `require_idle` adds the in-use checks (mounts, swap, holders) for the target. The trust
/// step passes `false` (a mounted stick may be trusted); a disk carrying the running system is
/// refused either way.
pub fn validate_device(roots: &Roots, st: NodeStat, dev: &DevName, require_idle: bool) -> Result<ValidDevice, String> {
    let shown = dev.path();

    // 1. The fd must be a block device (O_NOFOLLOW already refused a symlink at open). Its rdev
    //    is the identity everything below is looked up by.
    if st.is_symlink || !st.is_block {
        return Err(format!("{shown} is not a block device"));
    }
    let majmin = format!("{}:{}", st.major, st.minor);

    // 2. Resolve the device in sysfs by rdev (not by name), and make sure the name the caller
    //    gave matches what the kernel calls that device.
    let node_dir = fs::canonicalize(roots.sys.join("dev/block").join(&majmin))
        .map_err(|_| format!("{shown}: no sysfs entry for {majmin}"))?;
    let devices = fs::canonicalize(roots.sys.join("devices")).map_err(|e| format!("cannot resolve sysfs: {e}"))?;
    if !node_dir.starts_with(&devices) {
        return Err(format!("{shown}: unexpected sysfs location"));
    }
    if node_dir.file_name().map(|n| n.as_bytes()) != Some(dev.name.as_bytes()) {
        return Err(format!("{shown}: kernel name for {majmin} does not match"));
    }
    if read_trim(&node_dir.join("dev"))? != majmin {
        return Err(format!("{shown}: sysfs dev number mismatch"));
    }
    let is_part = node_dir.join("partition").is_file();
    if is_part != dev.is_partition() {
        return Err(format!("{shown}: partition/whole-disk mismatch"));
    }
    let disk_dir = if is_part { node_dir.parent().ok_or("bad sysfs path")?.to_path_buf() } else { node_dir.clone() };
    if disk_dir.file_name().map(|n| n.as_bytes()) != Some(dev.disk.as_bytes()) {
        return Err(format!("{shown}: parent disk name mismatch"));
    }
    // SCSI disks sit at .../<h:c:t:l>/block/<disk>; anything else is not a plain sd disk.
    if disk_dir.parent().and_then(Path::file_name).map(|n| n.as_bytes()) != Some(b"block".as_slice()) {
        return Err(format!("{shown}: unexpected sysfs layout"));
    }

    // 3. Transport. The physical topology in sysfs is built by the kernel from the bus the device
    //    enumerated on, so a USB device cannot fake being anything else and vice versa. We gate on
    //    USB alone: the `removable` attribute is 0 on most USB SSDs/enclosures (udisks treats all
    //    USB drives as removable for the same reason) and 1 on some internal card readers and
    //    hot-swap SATA bays, so it neither proves nor disproves "external".
    if !node_dir.components().any(is_usb_bus) {
        return Err(format!("{shown} is not a USB device"));
    }

    // 4. udev must also see a USB device (a cross-check; the topology above is the real gate), and must not have flagged it as a system disk or one to
    //    ignore (admins set these hints for disks that must never be touched).
    let disk_majmin = read_trim(&disk_dir.join("dev"))?;
    let disk_props = udev_props(roots, &disk_majmin)?;
    let target_props = if is_part { udev_props(roots, &majmin)? } else { disk_props.clone() };
    if !udev_says_usb(&disk_props) && !udev_says_usb(&target_props) {
        return Err(format!("{shown}: udev does not report a USB bus"));
    }
    for p in [&disk_props, &target_props] {
        if has(p, "UDISKS_SYSTEM", "1") || has(p, "UDISKS_IGNORE", "1") {
            return Err(format!("{shown} is marked as a system/ignored disk"));
        }
    }

    // 5. Not the system disk, and (if required) not in use. The helper's O_EXCL open is a second
    //    line of defence for the latter.
    let disk = Node { name: dev.disk.clone(), majmin: disk_majmin, dir: disk_dir.clone() };
    let parts = partitions(&disk_dir, &dev.disk)?;
    let mounts =
        mounts::parse_mountinfo(&fs::read_to_string(&roots.proc_mountinfo).map_err(|e| format!("mountinfo: {e}"))?);
    let swaps = mounts::parse_swaps(&fs::read_to_string(&roots.proc_swaps).map_err(|e| format!("swaps: {e}"))?);
    let mounts_of = |n: &Node| {
        let src = format!("/dev/{}", n.name);
        mounts
            .iter()
            .filter(move |m| m.dev == n.majmin || m.source == src)
            .map(|m| m.mount_point.clone())
            .collect::<Vec<_>>()
    };
    // A disk holding any of the system's own filesystems is refused outright, whichever
    // partition was asked for.
    for n in std::iter::once(&disk).chain(&parts) {
        if mounts_of(n).iter().any(|mp| SYSTEM_MOUNTS.contains(&mp.as_str())) {
            return Err(format!("{shown} is on a system disk"));
        }
    }
    // For a partition only that partition matters; a whole-disk target overwrites every
    // partition, so all of them must be idle.
    let target = Node { name: dev.name.clone(), majmin: majmin.clone(), dir: node_dir.clone() };
    let in_scope: Vec<&Node> = match (require_idle, is_part) {
        (false, _) => vec![],
        (true, true) => vec![&target],
        (true, false) => std::iter::once(&disk).chain(&parts).collect(),
    };
    for n in in_scope {
        if let Some(mp) = mounts_of(n).first() {
            return Err(format!("/dev/{} is mounted at {mp}", n.name));
        }
        if swaps.iter().any(|s| *s == format!("/dev/{}", n.name)) {
            return Err(format!("/dev/{} is in use as swap", n.name));
        }
        // holders/ lists device-mapper, md or LVM devices stacked on top; writing underneath them
        // would corrupt a live volume.
        if !holders_empty(&n.dir)? {
            return Err(format!("/dev/{} is in use by another block device (LVM/RAID/crypt)", n.name));
        }
    }

    // 6. A zero-size device is an empty card reader slot or a dead stick.
    let size: u64 = read_trim(&node_dir.join("size"))?.parse().map_err(|_| format!("{shown}: unreadable size"))?;
    if size == 0 {
        return Err(format!("{shown} has no medium (size 0)"));
    }

    Ok(ValidDevice { name: dev.clone(), major: st.major, minor: st.minor, disk_majmin: disk.majmin, disk_dir })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::parse_dev;
    use std::collections::HashMap;
    use std::os::fd::AsFd;
    use std::os::unix::fs::symlink;

    const USB_PATH: &str = "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0";
    const ATA_PATH: &str = "pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0";

    /// What `fstat` on the opened node would report, by node name (missing = open fails).
    struct FakeStat(HashMap<String, NodeStat>);

    struct Fake {
        _tmp: tempfile::TempDir,
        roots: Roots,
        stat: FakeStat,
    }

    impl Fake {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let r = tmp.path();
            let roots = Roots {
                dev: r.join("dev"),
                sys: r.join("sys"),
                udev_data: r.join("udev"),
                proc_mountinfo: r.join("mountinfo"),
                proc_swaps: r.join("swaps"),
            };
            for d in [&roots.dev, &roots.udev_data, &roots.sys.join("dev/block"), &roots.sys.join("devices")] {
                fs::create_dir_all(d).unwrap();
            }
            fs::write(&roots.proc_mountinfo, "22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n").unwrap();
            fs::write(&roots.proc_swaps, "Filename\tType\tSize\tUsed\tPriority\n").unwrap();
            Fake { _tmp: tmp, roots, stat: FakeStat(HashMap::new()) }
        }

        /// Create a block node under devices/<rel> with a /sys/dev/block link and an lstat entry.
        fn node(&mut self, rel: &str, name: &str, majmin: &str, partition: bool, bus: &str) -> PathBuf {
            let dir = self.roots.sys.join("devices").join(rel);
            fs::create_dir_all(dir.join("holders")).unwrap();
            fs::write(dir.join("dev"), format!("{majmin}\n")).unwrap();
            fs::write(dir.join("size"), "62521344\n").unwrap();
            if partition {
                fs::write(dir.join("partition"), "1\n").unwrap();
            } else {
                fs::write(dir.join("removable"), "0\n").unwrap();
            }
            symlink(format!("../../devices/{rel}"), self.roots.sys.join("dev/block").join(majmin)).unwrap();
            fs::write(self.roots.udev_data.join(format!("b{majmin}")), format!("S:disk/by-id/x\nE:ID_BUS={bus}\n"))
                .unwrap();
            let (ma, mi) = majmin.split_once(':').unwrap();
            let st =
                NodeStat { is_block: true, is_symlink: false, major: ma.parse().unwrap(), minor: mi.parse().unwrap() };
            self.stat.0.insert(name.to_string(), st);
            dir
        }

        /// A disk `disk` (MAJ:MIN `maj:min0`) with partitions 1..=nparts at `maj:min0+i`.
        fn disk(mut self, parent: &str, disk: &str, maj: u32, min0: u32, nparts: u32, bus: &str) -> Self {
            let rel = format!("{parent}/block/{disk}");
            self.node(&rel, disk, &format!("{maj}:{min0}"), false, bus);
            for i in 1..=nparts {
                self.node(
                    &format!("{rel}/{disk}{i}"),
                    &format!("{disk}{i}"),
                    &format!("{maj}:{}", min0 + i),
                    true,
                    bus,
                );
            }
            self
        }

        fn usb() -> Self {
            Fake::new().disk(USB_PATH, "sdb", 8, 16, 2, "usb")
        }

        fn run(&self, dev: &str, require_idle: bool) -> Result<ValidDevice, String> {
            let d = parse_dev(dev).unwrap();
            let st = *self.stat.0.get(&d.name).ok_or(format!("{dev}: No such file or directory"))?;
            validate_device(&self.roots, st, &d, require_idle)
        }

        /// The helper's view (idle required).
        fn check(&self, dev: &str) -> Result<ValidDevice, String> {
            self.run(dev, true)
        }

        /// The trust step's view (idle not required).
        fn trust(&self, dev: &str) -> Result<ValidDevice, String> {
            self.run(dev, false)
        }

        fn sys(&self, rel: &str) -> PathBuf {
            self.roots.sys.join("devices").join(USB_PATH).join("block").join(rel)
        }

        fn mount(&self, line: &str) {
            let mut t = fs::read_to_string(&self.roots.proc_mountinfo).unwrap();
            t.push_str(line);
            t.push('\n');
            fs::write(&self.roots.proc_mountinfo, t).unwrap();
        }
    }

    fn rejects(r: Result<ValidDevice, String>, needle: &str) {
        let e = r.expect_err("should be rejected");
        assert!(e.contains(needle), "error {e:?} does not mention {needle:?}");
    }

    #[test]
    fn usb_partition_accepted() {
        let f = Fake::usb();
        let v = f.check("/dev/sdb1").unwrap();
        assert_eq!((v.major, v.minor), (8, 17));
        assert_eq!(fs::read_to_string(f.sys("sdb/removable")).unwrap(), "0\n");
    }

    #[test]
    fn valid_device_reports_its_disk() {
        let f = Fake::usb();
        let v = f.check("/dev/sdb1").unwrap();
        assert_eq!(v.disk_majmin, "8:16");
        assert_eq!(fs::canonicalize(&v.disk_dir).unwrap(), fs::canonicalize(f.sys("sdb")).unwrap());
        assert_eq!(f.check("/dev/sdb").unwrap().disk_majmin, "8:16");
    }

    #[test]
    fn trust_does_not_require_idle_but_still_refuses_system_disk() {
        let f = Fake::usb();
        f.mount("90 22 8:17 / /media/u/S rw - exfat /dev/sdb1 rw");
        fs::write(&f.roots.proc_swaps, "Filename\tType\tSize\tUsed\tPriority\n/dev/sdb2 partition 1 0 -2\n").unwrap();
        fs::create_dir(f.sys("sdb/holders/dm-0")).unwrap();
        rejects(f.check("/dev/sdb1"), "mounted");
        rejects(f.check("/dev/sdb"), "in use by another block device");
        assert!(f.trust("/dev/sdb1").is_ok());
        assert!(f.trust("/dev/sdb").is_ok());
        f.mount("91 22 8:18 / /boot rw - ext4 /dev/sdb2 rw");
        rejects(f.trust("/dev/sdb"), "system disk");
        rejects(f.trust("/dev/sdb1"), "system disk");
        // Non-USB and udisks hints are refused for trust too.
        let f = Fake::new().disk(ATA_PATH, "sda", 8, 0, 1, "ata");
        rejects(f.trust("/dev/sda"), "not a USB device");
        let f = Fake::usb();
        fs::write(f.roots.udev_data.join("b8:16"), "E:ID_BUS=usb\nE:UDISKS_IGNORE=1\n").unwrap();
        rejects(f.trust("/dev/sdb"), "system/ignored");
    }

    #[test]
    fn usb_whole_disk_removable_accepted() {
        let f = Fake::new().disk(USB_PATH, "sdc", 8, 32, 0, "usb");
        fs::write(f.sys("sdc/removable"), "1\n").unwrap();
        assert_eq!(f.check("/dev/sdc").unwrap().minor, 32);
    }

    #[test]
    fn usb_partition_with_sibling_mounted_elsewhere_accepted() {
        let f = Fake::usb();
        f.mount("90 22 8:18 / /media/u/DATA rw - ext4 /dev/sdb2 rw");
        assert!(f.check("/dev/sdb1").is_ok());
        rejects(f.check("/dev/sdb"), "/dev/sdb2 is mounted");
    }

    #[test]
    fn sata_rejected_even_if_removable() {
        let f = Fake::new().disk(ATA_PATH, "sda", 8, 0, 1, "ata");
        rejects(f.check("/dev/sda"), "not a USB device");
        rejects(f.check("/dev/sda1"), "not a USB device");
        fs::write(f.roots.sys.join("devices").join(ATA_PATH).join("block/sda/removable"), "1\n").unwrap();
        rejects(f.check("/dev/sda"), "not a USB device");
    }

    #[test]
    fn sd_name_on_ata_path_with_usb_udev_rejected() {
        // udev claims usb, but the kernel topology says ata: topology wins.
        let f = Fake::new().disk(ATA_PATH, "sdd", 8, 48, 1, "usb");
        rejects(f.check("/dev/sdd1"), "not a USB device");
    }

    #[test]
    fn nvme_rejected() {
        // The name matcher refuses it before any device lookup...
        assert!(parse_dev("/dev/nvme0n1p1").is_err());
        // ...and an sd-named node whose rdev resolves to an NVMe namespace is refused too.
        let mut f = Fake::new();
        let rel = "pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/nvme0n1";
        f.node(rel, "nvme0n1", "259:0", false, "nvme");
        let st = f.stat.0["nvme0n1"];
        f.stat.0.insert("sde".into(), st);
        rejects(f.check("/dev/sde"), "does not match");
    }

    #[test]
    fn name_sysfs_mismatch_rejected() {
        // /dev/sdb1 node whose rdev is actually sdb2's.
        let mut f = Fake::usb();
        let st = f.stat.0["sdb2"];
        f.stat.0.insert("sdb1".into(), st);
        rejects(f.check("/dev/sdb1"), "does not match");
        // Whole-disk name pointing at a partition's rdev.
        let mut f = Fake::usb();
        let st = f.stat.0["sdb1"];
        f.stat.0.insert("sdb".into(), st);
        rejects(f.check("/dev/sdb"), "does not match");
    }

    #[test]
    fn symlink_or_non_block_node_rejected() {
        let mut f = Fake::usb();
        let st = f.stat.0["sdb1"];
        f.stat.0.insert("sdb1".into(), NodeStat { is_symlink: true, is_block: false, ..st });
        rejects(f.check("/dev/sdb1"), "not a block device");
        rejects(f.trust("/dev/sdb1"), "not a block device");
        f.stat.0.insert("sdb1".into(), NodeStat { is_block: false, ..st });
        rejects(f.check("/dev/sdb1"), "not a block device");
        f.stat.0.remove("sdb1");
        assert!(f.check("/dev/sdb1").is_err());
    }

    #[test]
    fn real_fstat_and_open_nofollow() {
        let n = std::fs::File::open("/dev/null").unwrap();
        let s = fstat(n.as_fd()).unwrap();
        assert!(!s.is_block && !s.is_symlink && (s.major, s.minor) == (1, 3));
        // open_device refuses a symlink at the final component (O_NOFOLLOW -> ELOOP).
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("dev")).unwrap();
        symlink("/dev/null", t.path().join("dev/sdb")).unwrap();
        let mut roots = Roots::system();
        roots.dev = t.path().join("dev");
        let dev = parse_dev("/dev/sdb").unwrap();
        let e = open_device(&roots, &dev, false).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ELOOP));
        // A regular file opens fine and fstat says "not a block device", which validation rejects.
        fs::remove_file(t.path().join("dev/sdb")).unwrap();
        fs::write(t.path().join("dev/sdb"), "x").unwrap();
        let fd = open_device(&roots, &dev, true).unwrap();
        let st = fstat(fd.as_fd()).unwrap();
        assert!(!st.is_block);
        assert!(validate_device(&roots, st, &dev, true).unwrap_err().contains("not a block device"));
        // The fd is close-on-exec.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn mounted_target_rejected() {
        let f = Fake::usb();
        f.mount("90 22 8:17 / /media/u/MY\\040STICK rw - exfat /dev/sdb1 rw");
        rejects(f.check("/dev/sdb1"), "mounted at /media/u/MY STICK");
    }

    #[test]
    fn mounted_by_source_rejected() {
        // btrfs reports an anonymous dev in field 3; the source still names the partition.
        let f = Fake::usb();
        f.mount("91 22 0:45 / /mnt/b rw - btrfs /dev/sdb1 rw");
        rejects(f.check("/dev/sdb1"), "mounted");
    }

    #[test]
    fn whole_disk_with_mounted_partition_rejected() {
        let f = Fake::usb();
        f.mount("90 22 8:17 / /media/u/S rw - exfat /dev/sdb1 rw");
        rejects(f.check("/dev/sdb"), "/dev/sdb1 is mounted");
    }

    #[test]
    fn system_disk_rejected() {
        for mp in ["/", "/boot/efi", "/home"] {
            let f = Fake::usb();
            f.mount(&format!("90 22 8:18 / {mp} rw - ext4 /dev/sdb2 rw"));
            rejects(f.check("/dev/sdb1"), "system disk");
            rejects(f.check("/dev/sdb"), "system disk");
        }
    }

    #[test]
    fn swap_rejected() {
        let f = Fake::usb();
        fs::write(&f.roots.proc_swaps, "Filename\tType\tSize\tUsed\tPriority\n/dev/sdb2 partition 1 0 -2\n").unwrap();
        rejects(f.check("/dev/sdb2"), "swap");
        rejects(f.check("/dev/sdb"), "swap");
        assert!(f.check("/dev/sdb1").is_ok());
    }

    #[test]
    fn holders_rejected() {
        let f = Fake::usb();
        fs::create_dir(f.sys("sdb/sdb1/holders/dm-0")).unwrap();
        rejects(f.check("/dev/sdb1"), "in use by another block device");
        rejects(f.check("/dev/sdb"), "in use by another block device");
        let f = Fake::usb();
        fs::create_dir(f.sys("sdb/holders/md0")).unwrap();
        rejects(f.check("/dev/sdb"), "in use by another block device");
    }

    #[test]
    fn udisks_hints_rejected() {
        for (who, key) in [("8:16", "UDISKS_SYSTEM"), ("8:17", "UDISKS_SYSTEM"), ("8:17", "UDISKS_IGNORE")] {
            let f = Fake::usb();
            fs::write(f.roots.udev_data.join(format!("b{who}")), format!("E:ID_BUS=usb\nE:{key}=1\n")).unwrap();
            rejects(f.check("/dev/sdb1"), "system/ignored");
        }
    }

    #[test]
    fn udev_bus_required() {
        let f = Fake::usb();
        for m in ["8:16", "8:17"] {
            fs::write(f.roots.udev_data.join(format!("b{m}")), "E:ID_BUS=ata\n").unwrap();
        }
        rejects(f.check("/dev/sdb1"), "udev does not report");
    }

    #[test]
    fn uas_drive_with_scsi_bus_accepted() {
        // Real udev db of a UAS USB SSD: ID_BUS=scsi, USB evidence only in ID_USB_*/ID_PATH.
        let f = Fake::usb();
        let uas = "E:ID_BUS=scsi\nE:ID_USB_DRIVER=uas\nE:ID_PATH=pci-0000:00:0d.0-usb-0:1:1.0-scsi-0:0:0:0\n";
        fs::write(f.roots.udev_data.join("b8:16"), uas).unwrap();
        fs::write(f.roots.udev_data.join("b8:17"), uas).unwrap();
        assert!(f.check("/dev/sdb1").is_ok());
        fs::write(f.roots.udev_data.join("b8:16"), "E:ID_BUS=scsi\nE:ID_PATH=pci-0000:00:17.0-ata-1\n").unwrap();
        fs::write(f.roots.udev_data.join("b8:17"), "E:ID_BUS=scsi\n").unwrap();
        rejects(f.check("/dev/sdb1"), "udev does not report");
    }

    #[test]
    fn missing_udev_db_rejected() {
        let f = Fake::usb();
        fs::remove_file(f.roots.udev_data.join("b8:17")).unwrap();
        rejects(f.check("/dev/sdb1"), "no udev database entry");
        fs::remove_file(f.roots.udev_data.join("b8:16")).unwrap();
        rejects(f.check("/dev/sdb"), "no udev database entry");
    }

    #[test]
    fn size_zero_rejected() {
        let f = Fake::usb();
        fs::write(f.sys("sdb/size"), "0\n").unwrap();
        rejects(f.check("/dev/sdb"), "size 0");
    }

    #[test]
    fn sysfs_link_outside_devices_rejected() {
        let mut f = Fake::usb();
        let outside = f.roots.sys.join("elsewhere/usb1/block/sdx");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, f.roots.sys.join("dev/block/8:99")).unwrap();
        f.stat.0.insert("sdx".into(), NodeStat { is_block: true, is_symlink: false, major: 8, minor: 99 });
        rejects(f.check("/dev/sdx"), "unexpected sysfs location");
    }
}
