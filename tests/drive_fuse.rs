//! Tier-2 tests of `flashcp mount`: the real binary, a real FUSE mount, real tools (cp, mv,
//! rsync, dd, ...), then fsck.exfat and a remount to check the content.
//!
//! Skipped (prints SKIP) unless /dev/fuse is usable, fusermount3 exists and mkfs.exfat exists.
//! FLASHCP_REQUIRE_FUSE=1 makes a missing FUSE a failure; FLASHCP_REQUIRE_FSCK=1 does the same
//! for the exfatprogs tools.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_flashcp");

fn have(cmd: &str, arg: &str) -> bool {
    Command::new(cmd).arg(arg).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

/// True when the test can run.
fn gate() -> bool {
    let fuse =
        std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse").is_ok() && have("fusermount3", "-V");
    if !fuse {
        assert!(std::env::var_os("FLASHCP_REQUIRE_FUSE").is_none(), "FUSE is required but not usable");
        println!("SKIP: /dev/fuse or fusermount3 unavailable");
        return false;
    }
    if !have("mkfs.exfat", "-V") {
        assert!(std::env::var_os("FLASHCP_REQUIRE_FSCK").is_none(), "mkfs.exfat is required");
        println!("SKIP: mkfs.exfat unavailable");
        return false;
    }
    true
}

struct Dir(PathBuf);
impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("flashcp-fuse-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
    fn p(&self, s: &str) -> PathBuf {
        self.0.join(s)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn image(d: &Dir, mb: u64) -> PathBuf {
    let img = d.p("v.img");
    let f = std::fs::File::create(&img).unwrap();
    f.set_len(mb << 20).unwrap();
    let st = Command::new("mkfs.exfat").arg("-L").arg("FCP").arg(&img).stdout(Stdio::null()).status().unwrap();
    assert!(st.success());
    std::fs::create_dir_all(d.p("mnt")).unwrap();
    img
}

fn fsck(img: &Path) {
    if !have("fsck.exfat", "-V") {
        assert!(std::env::var_os("FLASHCP_REQUIRE_FSCK").is_none(), "fsck.exfat is required");
        return;
    }
    let o = Command::new("fsck.exfat").arg("-n").arg(img).output().unwrap();
    assert!(
        o.status.success(),
        "fsck.exfat failed:\n{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}

fn run(cmd: &mut Command) -> String {
    let o = cmd.output().unwrap();
    assert!(
        o.status.success(),
        "{cmd:?} failed: {}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A running `flashcp mount`.
struct Mount {
    child: Child,
    events: Arc<Mutex<Vec<String>>>,
    mnt: PathBuf,
    sock: PathBuf,
}

impl Mount {
    fn start(d: &Dir, img: &Path, extra: &[&str]) -> Mount {
        let mnt = d.p("mnt");
        let sock = d.p("ctl.sock");
        let mut child = Command::new(BIN)
            .arg("mount")
            .arg(img)
            .arg(&mnt)
            .args(["--json", "--control"])
            .arg(&sock)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().unwrap();
        let ev = events.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                ev.lock().unwrap().push(l);
            }
        });
        let m = Mount { child, events, mnt, sock };
        m.wait_event("\"event\":\"mounted\"");
        m
    }

    fn wait_event(&self, pat: &str) -> String {
        let t = Instant::now();
        loop {
            if let Some(l) = self.events.lock().unwrap().iter().find(|l| l.contains(pat)) {
                return l.clone();
            }
            assert!(t.elapsed() < Duration::from_secs(60), "no event {pat}; got {:?}", self.events.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn has_event(&self, pat: &str) -> bool {
        self.events.lock().unwrap().iter().any(|l| l.contains(pat))
    }

    fn ctl(&self, cmd: &str) -> String {
        let mut s = UnixStream::connect(&self.sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        writeln!(s, "{cmd}").unwrap();
        let mut l = String::new();
        BufReader::new(s).read_line(&mut l).unwrap();
        l
    }

    fn wait_exit(&mut self, secs: u64) -> i32 {
        let t = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st.code().unwrap_or(-1);
            }
            assert!(t.elapsed() < Duration::from_secs(secs), "flashcp mount did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Unmounts through the control socket and checks the clean exit.
    fn unmount(mut self) {
        assert!(self.ctl("unmount").contains("\"ok\":true"));
        assert_eq!(self.wait_exit(30), 0, "events: {:?}", self.events.lock().unwrap());
        assert!(self.has_event("\"event\":\"unmounted\",\"clean\":true"));
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("fusermount3").args(["-u", "-z"]).arg(&self.mnt).status();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A deterministic tree: nested dirs, empty/small/large files, a unicode name.
fn make_tree(root: &Path) {
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut bytes = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    };
    for (path, n) in [
        ("top.txt", 12usize),
        ("empty", 0),
        ("a/one.bin", 5000),
        ("a/b/two.bin", 300_000),
        ("a/b/c/three.bin", 70_000),
        ("a/ünï/日本語.txt", 99),
        ("d/e/f/g/deep.bin", 4097),
        ("big.bin", 3_000_000),
    ] {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes(n)).unwrap();
    }
    std::fs::create_dir_all(root.join("emptydir")).unwrap();
}

fn diff(a: &Path, b: &Path) {
    run(Command::new("diff").arg("-r").arg(a).arg(b));
}

/// Remounts the image, runs `f` on the mount, unmounts.
fn remount(d: &Dir, img: &Path, f: impl FnOnce(&Path)) {
    let m = Mount::start(d, img, &[]);
    f(&m.mnt.clone());
    m.unmount();
}

#[test]
fn cp_tree_then_remount_matches() {
    if !gate() {
        return;
    }
    let d = Dir::new("cp");
    let img = image(&d, 64);
    let src = d.p("src");
    make_tree(&src);
    let m = Mount::start(&d, &img, &[]);
    run(Command::new("cp").arg("-r").arg(&src).arg(m.mnt.join("t")));
    diff(&src, &m.mnt.join("t"));
    m.unmount();
    fsck(&img);
    remount(&d, &img, |mnt| diff(&src, &mnt.join("t")));
    fsck(&img);
}

#[test]
fn mv_and_rm() {
    if !gate() {
        return;
    }
    let d = Dir::new("mv");
    let img = image(&d, 64);
    let src = d.p("src");
    make_tree(&src);
    let m = Mount::start(&d, &img, &[]);
    let t = m.mnt.join("t");
    run(Command::new("cp").arg("-r").arg(&src).arg(&t));
    // commit, so the moves below work on committed files as well as new ones
    assert!(m.ctl("flush").contains("\"ok\":true"));
    run(Command::new("mv").arg(t.join("top.txt")).arg(t.join("renamed.txt"))); // within a dir
    run(Command::new("mv").arg(t.join("a/b/two.bin")).arg(t.join("d/two-moved.bin"))); // across dirs
    run(Command::new("mv").arg(t.join("a/one.bin")).arg(t.join("big.bin"))); // onto an existing file
    run(Command::new("mv").arg(t.join("a/b/c")).arg(t.join("d/e/c-dir"))); // a directory
    // names are case-insensitive (a case-only rename is a no-op in the kernel: same inode)
    std::fs::write(t.join("CaseTest"), b"x").unwrap();
    assert_eq!(std::fs::read(t.join("casetest")).unwrap(), b"x");
    run(Command::new("rm").arg("-r").arg(t.join("d/e/f")));
    // expected tree, built locally with the same operations
    let exp = d.p("exp");
    run(Command::new("cp").arg("-r").arg(&src).arg(&exp));
    run(Command::new("mv").arg(exp.join("top.txt")).arg(exp.join("renamed.txt")));
    run(Command::new("mv").arg(exp.join("a/b/two.bin")).arg(exp.join("d/two-moved.bin")));
    run(Command::new("mv").arg(exp.join("a/one.bin")).arg(exp.join("big.bin")));
    run(Command::new("mv").arg(exp.join("a/b/c")).arg(exp.join("d/e/c-dir")));
    std::fs::write(exp.join("CaseTest"), b"x").unwrap();
    run(Command::new("rm").arg("-r").arg(exp.join("d/e/f")));
    diff(&exp, &t);
    // rm -r of everything (after a commit and before it)
    m.unmount();
    fsck(&img);
    remount(&d, &img, |mnt| {
        diff(&exp, &mnt.join("t"));
        run(Command::new("rm").arg("-r").arg(mnt.join("t")));
        assert_eq!(std::fs::read_dir(mnt).unwrap().count(), 0);
    });
    fsck(&img);
    remount(&d, &img, |mnt| assert_eq!(std::fs::read_dir(mnt).unwrap().count(), 0));
}

#[test]
fn rsync_twice_is_idempotent() {
    if !gate() || !have("rsync", "--version") {
        println!("SKIP: rsync unavailable");
        return;
    }
    let d = Dir::new("rsync");
    let img = image(&d, 64);
    let src = d.p("src");
    make_tree(&src);
    let m = Mount::start(&d, &img, &[]);
    let dst = m.mnt.join("r");
    let sync = |m: &Mount| {
        // -a would try to preserve owner/perms; exFAT has neither, so -rt (times matter for
        // the second run to find nothing to do)
        run(Command::new("rsync")
            .args(["-rt", "--itemize-changes"])
            .arg(format!("{}/", src.display()))
            .arg(format!("{}/", m.mnt.join("r").display())))
    };
    let first = sync(&m);
    assert!(first.contains("big.bin"), "{first}");
    diff(&src, &dst);
    assert!(m.ctl("flush").contains("\"ok\":true"));
    let second = sync(&m);
    assert!(second.trim().is_empty(), "second rsync transferred: {second}");
    diff(&src, &dst);
    // change one file: only it is rewritten (temp file + rename inside the mount)
    std::fs::write(src.join("a/one.bin"), b"changed").unwrap();
    let third = sync(&m);
    assert!(third.contains("one.bin") && !third.contains("big.bin"), "{third}");
    m.unmount();
    fsck(&img);
    remount(&d, &img, |mnt| diff(&src, &mnt.join("r")));
}

#[test]
fn overwrite_in_place_and_truncate() {
    if !gate() {
        return;
    }
    let d = Dir::new("dd");
    let img = image(&d, 64);
    let m = Mount::start(&d, &img, &[]);
    let f = m.mnt.join("f.bin");
    let local = d.p("f.local");
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&f, &data).unwrap();
    std::fs::write(&local, &data).unwrap();
    assert!(m.ctl("flush").contains("\"ok\":true")); // now committed data is overwritten
    for p in [&f, &local] {
        run(Command::new("dd")
            .args(["if=/dev/zero", "bs=1", "count=777", "seek=123456", "conv=notrunc"])
            .arg(format!("of={}", p.display()))
            .stderr(Stdio::null()));
        run(Command::new("dd")
            .args(["if=/dev/urandom", "bs=4096", "count=0"])
            .arg(format!("of={}", p.display()))
            .stderr(Stdio::null()));
        run(Command::new("truncate").args(["-s", "1500000"]).arg(p)); // grow (zeros)
        run(Command::new("dd")
            .args(["if=/dev/zero", "bs=512", "count=3", "seek=2000", "conv=notrunc"])
            .arg(format!("of={}", p.display()))
            .stderr(Stdio::null()));
        run(Command::new("truncate").args(["-s", "400000"]).arg(p)); // shrink
    }
    assert_eq!(std::fs::read(&f).unwrap(), std::fs::read(&local).unwrap());
    m.unmount();
    fsck(&img);
    remount(&d, &img, |mnt| assert_eq!(std::fs::read(mnt.join("f.bin")).unwrap(), std::fs::read(&local).unwrap()));
}

#[test]
fn control_socket_prepare_eject_and_resume() {
    if !gate() {
        return;
    }
    let d = Dir::new("ctl");
    let img = image(&d, 64);
    let m = Mount::start(&d, &img, &["--idle-ms", "60000", "--close-ms", "0"]);
    let st = m.ctl("status");
    assert!(st.contains("\"proto\":1") && st.contains("\"state\":\"idle\""), "{st}");
    std::fs::write(m.mnt.join("a.txt"), vec![7u8; 100_000]).unwrap();
    let st = m.ctl("status");
    assert!(st.contains("\"state\":\"dirty\"") && st.contains("\"dirty_bytes\":100000"), "{st}");
    assert!(m.ctl("bogus").contains("unknown command"));
    assert!(m.ctl("flush").contains("\"state\":\"idle\""));
    assert!(m.ctl("status").contains("\"dirty_bytes\":0"));
    std::fs::write(m.mnt.join("b.txt"), b"before eject").unwrap();
    let r = m.ctl("prepare-eject");
    assert!(r.contains("\"safe\":true"), "{r}");
    assert!(m.ctl("status").contains("\"state\":\"ejecting\""));
    let e = std::fs::write(m.mnt.join("c.txt"), b"nope").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EROFS), "{e}");
    assert_eq!(std::fs::read(m.mnt.join("b.txt")).unwrap(), b"before eject"); // reads still work
    assert!(m.ctl("prepare-eject").contains("\"safe\":true")); // a no-op
    assert!(m.ctl("resume").contains("\"ok\":true"));
    std::fs::write(m.mnt.join("c.txt"), b"after resume").unwrap();
    m.unmount();
    fsck(&img);
    remount(&d, &img, |mnt| {
        assert_eq!(std::fs::read(mnt.join("a.txt")).unwrap(), vec![7u8; 100_000]);
        assert_eq!(std::fs::read(mnt.join("b.txt")).unwrap(), b"before eject");
        assert_eq!(std::fs::read(mnt.join("c.txt")).unwrap(), b"after resume");
    });
}

#[test]
fn sigterm_while_dirty_commits() {
    if !gate() {
        return;
    }
    let d = Dir::new("term");
    let img = image(&d, 64);
    let src = d.p("src");
    make_tree(&src);
    let mut m = Mount::start(&d, &img, &["--idle-ms", "60000", "--close-ms", "0"]);
    run(Command::new("cp").arg("-r").arg(&src).arg(m.mnt.join("t")));
    assert!(m.ctl("status").contains("\"state\":\"dirty\""));
    unsafe { libc::kill(m.child.id() as i32, libc::SIGTERM) };
    assert_eq!(m.wait_exit(30), 0, "events: {:?}", m.events.lock().unwrap());
    assert!(m.has_event("\"event\":\"committed\""));
    assert!(m.has_event("\"event\":\"unmounted\",\"clean\":true"));
    // the mount is gone
    assert!(std::fs::read_dir(d.p("mnt")).unwrap().next().is_none());
    fsck(&img);
    remount(&d, &img, |mnt| diff(&src, &mnt.join("t")));
}

#[test]
fn lazy_detach_while_dirty_still_commits() {
    if !gate() {
        return;
    }
    let d = Dir::new("detach");
    let img = image(&d, 64);
    let src = d.p("src");
    make_tree(&src);
    let mut m = Mount::start(&d, &img, &["--idle-ms", "60000", "--close-ms", "0"]);
    run(Command::new("cp").arg("-r").arg(&src).arg(m.mnt.join("t")));
    assert!(m.ctl("status").contains("\"state\":\"dirty\""));
    // what `gio mount -u` / `umount -l` does: the mount is gone at once, nothing was flushed
    run(Command::new("fusermount3").args(["-u", "-z"]).arg(&m.mnt));
    assert_eq!(m.wait_exit(30), 0, "events: {:?}", m.events.lock().unwrap());
    assert!(m.has_event("\"event\":\"committed\""));
    assert!(m.has_event("\"event\":\"dirty_bytes\",\"bytes\":0,\"files\":0"));
    assert!(m.has_event("\"event\":\"unmounted\",\"clean\":true"));
    fsck(&img);
    remount(&d, &img, |mnt| diff(&src, &mnt.join("t")));
}

#[test]
fn check_exit_codes() {
    if !gate() {
        return;
    }
    let d = Dir::new("check");
    let img = image(&d, 16);
    let check = |p: &Path| {
        let mut c = Command::new(BIN);
        c.args(["mount", "--check", "--json"]).arg(p).env_clear().stdin(Stdio::null());
        let o = c.output().unwrap();
        (o.status.code().unwrap(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let (code, out) = check(&img);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(
        out.contains("\"ok\":true") && out.contains("\"label\":\"FCP\"") && out.contains("\"pending_journal\":false")
    );

    // not exFAT
    let junk = d.p("junk.img");
    std::fs::File::create(&junk).unwrap().set_len(8 << 20).unwrap();
    let (code, out) = check(&junk);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("\"ok\":false") && out.contains("\"reason\":\"not_exfat\""), "{out}");
    assert!(out.contains("\"event\":\"refused\""), "{out}");

    // VolumeDirty set by someone else
    let dirty = d.p("dirty.img");
    std::fs::copy(&img, &dirty).unwrap();
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&dirty).unwrap();
        let mut b = [0u8; 1];
        f.read_exact_at(&mut b, 106).unwrap();
        b[0] |= 2;
        f.write_all_at(&b, 106).unwrap();
    }
    let (code, out) = check(&dirty);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("\"reason\":\"dirty\""), "{out}");

    // --device-fd with an O_RDONLY fd
    let f = std::fs::File::open(&img).unwrap();
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);
    let o = {
        let mut c = Command::new(BIN);
        c.args(["mount", "--check", "--json", "--device-fd", &fd.to_string()]).arg(&img);
        // the child inherits the fd only if CLOEXEC is cleared
        use std::os::unix::process::CommandExt;
        unsafe {
            c.pre_exec(move || {
                let fl = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
                Ok(())
            });
        }
        c.output().unwrap()
    };
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stdout));

    // a mounted image is busy: exit 4 (the image is held with flock by the running mount; the
    // claim fails for the second mount, and --check on a regular file does not claim)
    let m = Mount::start(&d, &img, &[]);
    let o = Command::new(BIN).args(["mount"]).arg(&img).arg(d.p("mnt2")).arg("--json").output().unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(String::from_utf8_lossy(&o.stdout).contains("\"event\":\"busy\""));
    m.unmount();
}

/// `sync -f` and `syncfs(2)` never reach a FUSE driver mounted without `fuseblk` (the kernel
/// forwards FUSE_SYNCFS only to virtiofs and fuseblk servers): they return at once and commit
/// nothing. What bounds the loss after `cp && sync` is the close commit: closing the last file
/// open for writing commits after --close-ms (150 ms). fsync commits at once.
#[test]
fn sync_is_not_forwarded_but_closing_commits() {
    if !gate() {
        return;
    }
    let d = Dir::new("syncfs");
    let img = image(&d, 64);
    let m = Mount::start(&d, &img, &["--idle-ms", "600000"]);
    let mut f = std::fs::File::create(m.mnt.join("open.bin")).unwrap();
    f.write_all(&vec![3u8; 300_000]).unwrap();
    let st = Command::new("sync").arg("-f").arg(&m.mnt).status().unwrap();
    assert!(st.success());
    assert_eq!(unsafe { libc::syncfs(std::os::unix::io::AsRawFd::as_raw_fd(&f)) }, 0);
    std::thread::sleep(Duration::from_millis(400));
    let s = m.ctl("status");
    assert!(s.contains("\"dirty_bytes\":300000"), "sync/syncfs committed after all (update DESIGN.md): {s}");
    assert!(!m.has_event("\"event\":\"committing\""));
    // the last write handle closes: committed within the linger (plus the commit)
    drop(f);
    // the idle commit is 10 minutes away: a commit now is the close commit (generous timeout:
    // loaded CI machines)
    m.wait_event("\"event\":\"committed\"");
    assert!(m.wait_event("\"reason\":\"close\"").contains("committing"));
    assert!(m.ctl("status").contains("\"dirty_bytes\":0"));
    // a read-only handle that creates (O_RDONLY|O_CREAT) is no writer: closes still commit after it
    let c = std::ffi::CString::new(m.mnt.join("ro-created").into_os_string().into_encoded_bytes()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CREAT, 0o644) };
    assert!(fd >= 0);
    unsafe { libc::close(fd) };
    // a whole-file write (open, write, close) is durable a linger later, no sync needed
    std::fs::write(m.mnt.join("second.txt"), b"second").unwrap();
    let t = Instant::now();
    loop {
        if m.ctl("status").contains("\"dirty_bytes\":0")
            && m.events.lock().unwrap().iter().filter(|l| l.contains("\"event\":\"committed\"")).count() >= 2
        {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "second close commit missing");
        std::thread::sleep(Duration::from_millis(20));
    }
    m.unmount();
    fsck(&img);
}

fn errno<T>(r: std::io::Result<T>) -> Option<i32> {
    r.err().and_then(|e| e.raw_os_error())
}

/// The mount runs without `default_permissions` (which cost a GETATTR per created file), so
/// the driver makes the checks the kernel made against the modes it reports (0644/0755,
/// 0444/0555 with the read-only attribute). One assertion per check.
#[test]
fn permission_checks_without_default_permissions() {
    if !gate() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let d = Dir::new("perm");
    let img = image(&d, 64);
    let m = Mount::start(&d, &img, &[]);
    let (f, rd) = (m.mnt.join("ro.txt"), m.mnt.join("rodir"));
    std::fs::write(&f, b"read only").unwrap();
    std::fs::create_dir(&rd).unwrap();
    std::fs::write(rd.join("inside"), b"x").unwrap();
    std::fs::create_dir(rd.join("sub")).unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::set_permissions(&rd, std::fs::Permissions::from_mode(0o555)).unwrap();
    let eacces = Some(libc::EACCES);
    // 1. open for writing a read-only file
    assert_eq!(errno(std::fs::OpenOptions::new().write(true).open(&f)), eacces);
    // 2. truncate by path a read-only file
    let c = std::ffi::CString::new(f.clone().into_os_string().into_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::truncate(c.as_ptr(), 0) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), eacces);
    // 2b. O_RDONLY|O_TRUNC, O_CREAT|O_TRUNC on the existing file, and ftruncate through a handle
    //     opened for writing before the attribute was set: no truncation in any of them
    let raw = |flags: i32| unsafe { libc::open(c.as_ptr(), flags, 0o644) };
    assert_eq!(raw(libc::O_RDONLY | libc::O_TRUNC), -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), eacces);
    assert_eq!(raw(libc::O_RDONLY | libc::O_CREAT | libc::O_TRUNC), -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), eacces);
    let g = m.mnt.join("held-open.txt");
    let held = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&g).unwrap();
    std::io::Write::write_all(&mut &held, b"keep me").unwrap();
    std::fs::set_permissions(&g, std::fs::Permissions::from_mode(0o444)).unwrap();
    assert_eq!(errno(held.set_len(0)), eacces);
    drop(held);
    assert_eq!(std::fs::read(&g).unwrap(), b"keep me");
    assert_eq!(std::fs::read(&f).unwrap(), b"read only");
    // 3. create and 4. mkdir in a read-only directory
    assert_eq!(errno(std::fs::write(rd.join("new"), b"n")), eacces);
    assert_eq!(errno(std::fs::create_dir(rd.join("newdir"))), eacces);
    // 5. unlink and 6. rmdir in it
    assert_eq!(errno(std::fs::remove_file(rd.join("inside"))), eacces);
    assert_eq!(errno(std::fs::remove_dir(rd.join("sub"))), eacces);
    // 7. rename out of it and into it
    assert_eq!(errno(std::fs::rename(rd.join("inside"), m.mnt.join("out"))), eacces);
    std::fs::write(m.mnt.join("w.txt"), b"w").unwrap();
    assert_eq!(errno(std::fs::rename(m.mnt.join("w.txt"), rd.join("w.txt"))), eacces);
    // 8. access(2): W_OK on read-only objects, X_OK on files
    let acc = |p: &Path, mode| {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        unsafe { libc::access(c.as_ptr(), mode) }
    };
    assert_eq!(acc(&f, libc::W_OK), -1);
    assert_eq!(acc(&rd, libc::W_OK), -1);
    assert_eq!(acc(&m.mnt.join("w.txt"), libc::W_OK), 0);
    assert_eq!(acc(&m.mnt.join("w.txt"), libc::X_OK), -1);
    assert_eq!(acc(&rd, libc::X_OK), 0);
    // 9. executing a file (never executable): the kernel still refuses that itself
    assert_eq!(errno(Command::new(m.mnt.join("w.txt")).status()), eacces);
    // 10. chown to someone else
    let c = std::ffi::CString::new(m.mnt.join("w.txt").into_os_string().into_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::chown(c.as_ptr(), 0, 0) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    // reads still work, and the attribute comes off again
    assert_eq!(std::fs::read(&f).unwrap(), b"read only");
    std::fs::set_permissions(&rd, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(rd.join("new"), b"n").unwrap();
    m.unmount();
    fsck(&img);
}

/// FUSE requests per file, from the driver's own counters (`stats`): creating files costs
/// LOOKUP, CREATE, WRITE, RELEASE (no GETATTR since default_permissions is gone); reading
/// them back OPEN, READ, RELEASE.
#[test]
fn fuse_requests_per_file() {
    if !gate() {
        return;
    }
    let d = Dir::new("reqs");
    let img = image(&d, 64);
    let src = d.p("src/tree");
    std::fs::create_dir_all(&src).unwrap();
    for k in 0..300 {
        std::fs::write(src.join(format!("f{k:03}")), vec![k as u8; 3000]).unwrap();
    }
    let total = |m: &Mount| -> f64 {
        let s = m.ctl("stats");
        let k = s.find("\"requests\":").unwrap() + 11;
        s[k..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
    };
    let m = Mount::start(&d, &img, &[]);
    let base = total(&m);
    run(Command::new("cp").arg("-r").arg(&src).arg(&m.mnt));
    let per_create = (total(&m) - base) / 300.0;
    let s = m.ctl("stats");
    m.unmount();
    assert!(per_create <= 4.2, "{per_create:.2} requests per created file (was 5.04): {s}");
    let m = Mount::start(&d, &img, &[]);
    let base = total(&m);
    let out = d.p("out");
    run(Command::new("cp").arg("-r").arg(m.mnt.join("tree")).arg(&out));
    let per_read = (total(&m) - base) / 300.0;
    m.unmount();
    assert!(per_read <= 3.2, "{per_read:.2} requests per file read");
    eprintln!("requests per created file {per_create:.2}, per read file {per_read:.2}");
}
