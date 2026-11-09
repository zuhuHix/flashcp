//! Kernel-mount helpers for the gated `add_kernel` tests.
//!
//! `Mounted` attaches an image to a loop device and mounts it with the kernel exFAT driver:
//! as root with losetup + mount, as a normal user with udisksctl. Dropping it (or calling
//! `unmount`) syncs, unmounts, and detaches the loop device, so a failed test does not leave
//! a loop device behind. flashcp refuses images that are attached to a loop device, so
//! `unmount` also waits until no loop device still names the image.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use crate::common::exfat_reader::{Volume, read_volume};
use crate::common::{XorShift, assert_success, flashcp, make_image};

/// True when the mount tests are enabled. Prints a SKIP line otherwise.
pub fn gated() -> bool {
    if std::env::var("FLASHCP_MOUNT_TESTS").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("SKIP kernel mount test: set FLASHCP_MOUNT_TESTS=1 to run it");
    false
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Finds an exfatprogs or util-linux tool on PATH, falling back to the sbin directories that
/// a normal user's PATH often lacks.
pub fn tool(name: &str) -> PathBuf {
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    for d in std::env::split_paths(&dirs).chain(["/usr/sbin", "/sbin"].iter().map(PathBuf::from)) {
        let p = d.join(name);
        if p.is_file() {
            return p;
        }
    }
    panic!("required tool {name} not found on PATH, /usr/sbin or /sbin");
}

fn run(prog: &str, args: &[&str]) -> Output {
    Command::new(tool(prog)).args(args).output().unwrap_or_else(|e| panic!("cannot run {prog}: {e}"))
}

/// Runs a command and returns its combined output, panicking with it when the command fails.
fn run_ok(prog: &str, args: &[&str]) -> String {
    let out = run(prog, args);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{prog} {args:?} failed:\n{text}");
    text
}

/// First whitespace-separated token starting with `prefix`, without trailing punctuation.
fn token_with(text: &str, prefix: &str) -> Option<String> {
    text.split_whitespace().find(|w| w.starts_with(prefix)).map(|w| w.trim_end_matches(['.', ',']).to_string())
}

/// `mkfs.exfat` on an existing image file.
pub fn mkfs_exfat(img: &Path, cluster: Option<u32>, label: &str) {
    let img_s = img.to_str().expect("UTF-8 path");
    let cluster_s = cluster.map(|c| c.to_string());
    let mut args: Vec<&str> = vec!["-L", label];
    if let Some(c) = cluster_s.as_deref() {
        args.push("-c");
        args.push(c);
    }
    args.push(img_s);
    run_ok("mkfs.exfat", &args);
}

/// Creates an image of `bytes` bytes formatted as exFAT.
pub fn fresh_exfat_image(img: &Path, bytes: u64, cluster: Option<u32>, label: &str) {
    make_image(img, bytes);
    mkfs_exfat(img, cluster, label);
}

/// `fsck.exfat -n`, asserting a clean result. Unlike `common::fsck_clean` it never skips.
pub fn fsck_strict(img: &Path) {
    let out = run("fsck.exfat", &["-n", img.to_str().expect("UTF-8 path")]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("clean"), "fsck.exfat -n {} not clean:\n{text}", img.display());
}

/// Reads back the volume with the independent parser (strict: any error fails the test).
pub fn read_back(img: &Path) -> Volume {
    read_volume(img).unwrap_or_else(|e| panic!("read-back of {} failed: {e}", img.display()))
}

/// `flashcp info IMG --json`, returning the stdout text.
pub fn info_json(img: &Path) -> String {
    let out = flashcp().arg("info").arg(img).arg("--json").output().expect("run flashcp info");
    assert_success(&out, "flashcp info --json");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Unsigned number stored under `"key":` in a flat JSON object.
pub fn json_u64(json: &str, key: &str) -> u64 {
    let pat = format!("\"{key}\":");
    let at = json.find(&pat).unwrap_or_else(|| panic!("no key {key} in {json}")) + pat.len();
    let digits: String = json[at..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or_else(|_| panic!("key {key} is not a number in {json}"))
}

/// Recursively copies a directory tree with std::fs (contents only, no metadata).
pub fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create copy target");
    let mut ents: Vec<_> = fs::read_dir(src).expect("read source dir").map(|e| e.expect("entry")).collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let to = dst.join(e.file_name());
        let ft = e.file_type().expect("file type");
        if ft.is_dir() {
            copy_tree(&e.path(), &to);
        } else if ft.is_file() {
            fs::copy(e.path(), &to).expect("copy file");
        } else {
            panic!("unsupported source entry {}", e.path().display());
        }
    }
}

/// Deterministic pseudo-random bytes (the same seed always gives the same bytes).
pub fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    XorShift(seed | 1).fill(&mut buf);
    buf
}

/// Writes `pattern(seed, len)` to `path`, creating parent directories.
pub fn write_pattern(path: &Path, seed: u64, len: usize) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, pattern(seed, len)).expect("write pattern file");
}

pub fn read_bytes(path: &Path) -> Vec<u8> {
    let mut v = Vec::new();
    File::open(path).and_then(|mut f| f.read_to_end(&mut v)).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    v
}

/// `diff -r` between two directories, skipping the given names at every level.
/// Panics with the diff output when the trees differ.
pub fn diff_r(a: &Path, b: &Path, skip: &[&str]) {
    let mut args: Vec<String> = vec!["-r".into()];
    for s in skip {
        args.push("-x".into());
        args.push((*s).to_string());
    }
    args.push(a.to_str().expect("UTF-8 path").into());
    args.push(b.to_str().expect("UTF-8 path").into());
    let out = Command::new(tool("diff")).args(&args).output().expect("run diff");
    assert!(
        out.status.success(),
        "diff -r {} {} differs:\n{}{}",
        a.display(),
        b.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Whether some loop device is still attached to the image (matched by device and inode).
fn loop_attached(img: &Path) -> bool {
    let Ok(md) = fs::metadata(img) else { return false };
    let Ok(entries) = fs::read_dir("/sys/block") else { return false };
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().starts_with("loop") {
            continue;
        }
        let Ok(bf) = fs::read_to_string(e.path().join("loop/backing_file")) else { continue };
        let bf = bf.trim_end_matches('\n');
        let bf = bf.strip_suffix(" (deleted)").unwrap_or(bf);
        if let Ok(m) = fs::metadata(bf)
            && m.dev() == md.dev()
            && m.ino() == md.ino()
        {
            return true;
        }
    }
    false
}

/// Waits up to 5 s for every loop device on the image to go away.
fn wait_detached(img: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while loop_attached(img) {
        assert!(Instant::now() < deadline, "loop device still attached to {} after 5 s", img.display());
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Mount point of `dev` according to /proc/mounts, polled for up to `wait`.
fn mount_point_in_proc(dev: &str, wait: Duration) -> Option<PathBuf> {
    let deadline = Instant::now() + wait;
    loop {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let mut f = line.split(' ');
                if let (Some(src), Some(at)) = (f.next(), f.next())
                    && src == dev
                {
                    return Some(PathBuf::from(at.replace("\\040", " ")));
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An exFAT image attached to a loop device and mounted by the kernel.
pub struct Mounted {
    /// Loop device, e.g. /dev/loop3.
    pub dev: String,
    /// Mount point.
    pub at: PathBuf,
    img: PathBuf,
    root: bool,
    done: bool,
}

impl Mounted {
    /// Attaches and mounts `img` read-write. As root: losetup + mount at a directory next to
    /// the image. As a normal user: udisksctl loop-setup + mount.
    pub fn new(img: &Path) -> Mounted {
        let img_s = img.to_str().expect("UTF-8 path").to_string();
        let root = is_root();
        if root {
            let dev = run_ok("losetup", &["--find", "--show", &img_s]).trim().to_string();
            let at = img.with_extension("mnt");
            fs::create_dir_all(&at).expect("create mount point");
            let mounted = run("mount", &[&dev, at.to_str().expect("UTF-8 path")]);
            if !mounted.status.success() {
                let _ = run("losetup", &["--detach", &dev]);
                panic!("mount {dev} failed:\n{}", String::from_utf8_lossy(&mounted.stderr));
            }
            Mounted { dev, at, img: img.to_path_buf(), root, done: false }
        } else {
            let setup = run_ok("udisksctl", &["loop-setup", "-f", &img_s, "--no-user-interaction"]);
            let Some(dev) = token_with(&setup, "/dev/loop") else {
                panic!("no loop device in udisksctl output:\n{setup}");
            };
            // The desktop's udisks may auto-mount a recognised filesystem right after attach;
            // use that mount if it shows up, otherwise mount it ourselves.
            let at = match mount_point_in_proc(&dev, Duration::from_millis(1500)) {
                Some(at) => at,
                None => {
                    let mounted = run("udisksctl", &["mount", "-b", &dev, "--no-user-interaction"]);
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&mounted.stdout),
                        String::from_utf8_lossy(&mounted.stderr)
                    );
                    let parsed = if mounted.status.success() {
                        text.split(" at ").nth(1).map(|s| s.trim().trim_end_matches('.').to_string())
                    } else {
                        text.split("already mounted at `").nth(1).and_then(|s| s.split('\'').next()).map(String::from)
                    };
                    match parsed {
                        Some(at) => PathBuf::from(at),
                        None => {
                            let _ = run("udisksctl", &["loop-delete", "-b", &dev, "--no-user-interaction"]);
                            panic!("udisksctl mount {dev} failed:\n{text}");
                        }
                    }
                }
            };
            Mounted { dev, at, img: img.to_path_buf(), root, done: false }
        }
    }

    /// Syncs, unmounts, detaches, and waits until the image is no longer attached. Asserts
    /// that each step succeeds.
    pub fn unmount(mut self) {
        self.teardown(true);
    }

    /// Unmounts, retrying for up to 10 s while the mount is busy (desktop indexers often scan a
    /// freshly mounted volume for a moment).
    fn unmount_retrying(&self) -> Output {
        let at_s = self.at.to_str().unwrap_or("").to_string();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = if self.root {
                run("umount", &[&at_s])
            } else {
                run("udisksctl", &["unmount", "-b", &self.dev, "--no-user-interaction"])
            };
            if out.status.success() || Instant::now() >= deadline {
                return out;
            }
            let _ = run("sync", &[]);
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn teardown(&mut self, strict: bool) {
        if self.done {
            return;
        }
        let _ = run("sync", &[]);
        let umount = self.unmount_retrying();
        if !umount.status.success() {
            // Still mounted: nothing else can be detached. Leave `done` unset so Drop (which
            // runs after a panic here) makes one more attempt.
            assert!(
                !strict,
                "unmount {} failed:\n{}{}",
                self.at.display(),
                String::from_utf8_lossy(&umount.stdout),
                String::from_utf8_lossy(&umount.stderr)
            );
            return;
        }
        self.done = true;
        let dev = self.dev.clone();
        // udisks reports loop-delete inconsistently (it can succeed without detaching, or
        // refuse when the loop was already released), so the result is checked by waiting.
        if self.root {
            let _ = run("losetup", &["--detach", &dev]);
        } else {
            let _ = run("udisksctl", &["loop-delete", "-b", &dev, "--no-user-interaction"]);
        }
        if self.root {
            let _ = fs::remove_dir(&self.at);
        }
        if strict {
            wait_detached(&self.img);
        }
    }
}

impl Drop for Mounted {
    /// Best effort when a test fails before `unmount`: errors are ignored.
    fn drop(&mut self) {
        self.teardown(false);
    }
}
