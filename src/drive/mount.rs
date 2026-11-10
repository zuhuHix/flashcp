//! `flashcp mount`: the command (claiming the device, events, control socket, signals,
//! shutdown) around [`super::Fs`] and its FUSE adapter. The contract is
//! `docs/drive-mode-interface.md`; exit codes 0..6 are defined there.

use super::check::{self, Busy};
use super::{Fallback, Fs, FsOpts, Note, Recovered, Refused};
use crate::events::json_str;
use anyhow::Result;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct MountArgs {
    pub device: PathBuf,
    pub mountpoint: PathBuf,
    pub json: bool,
    pub control: Option<PathBuf>,
    pub device_fd: Option<i32>,
    pub idle_ms: u64,
    pub max_dirty_mb: Option<u64>,
    pub threads: usize,
}

static JSON: AtomicBool = AtomicBool::new(false);
static OUT: Mutex<()> = Mutex::new(());

/// One JSON line on stdout (with `--json`), flushed.
fn emit(line: String) {
    if JSON.load(Ordering::Relaxed) {
        let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{line}");
        let _ = o.flush();
    }
}

/// At most 1 KB of `s`, cut at a character boundary, on one line.
fn short(s: &str) -> String {
    let s = s.replace(['\n', '\r'], " ");
    if s.len() <= 1024 {
        return s;
    }
    let mut n = 1024;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    format!("{}...", &s[..n])
}

fn opt_str(s: &Option<String>) -> String {
    s.as_ref().map_or("null".into(), |s| json_str(&short(s)))
}

fn serial_str(s: Option<u32>) -> String {
    s.map_or("null".into(), |s| format!("\"{s:08x}\""))
}

fn io_code(e: &anyhow::Error) -> i32 {
    e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()).and_then(|io| io.raw_os_error()).unwrap_or(0)
}

fn is_busy_msg(m: &str) -> bool {
    m.contains("is in use") || m.contains("locked by another") || m.contains("attached to loop device")
}

fn dev_len(f: &File) -> Result<u64> {
    let md = f.metadata()?;
    if md.file_type().is_block_device() { crate::exfat::blk_size(f) } else { Ok(md.len()) }
}

/// `flashcp mount --check`: reads nothing but the device it is given (and sysfs/mountinfo for
/// the busy test), never the environment or HOME. Returns the exit code.
pub fn run_check(device: &Path, fd: Option<i32>, json: bool) -> i32 {
    JSON.store(json, Ordering::Relaxed);
    let file = match fd {
        Some(fd) => {
            if fd < 0 || unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
                eprintln!("flashcp mount: --device-fd {fd} is not an open file descriptor");
                return 2;
            }
            unsafe { File::from_raw_fd(fd) }
        }
        None => match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_EXCL).open(device) {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                return busy_exit(device, "in use (mounted or held by another driver)");
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("flashcp mount: {}: {e}", device.display());
                emit(format!(r#"{{"event":"error","message":{},"fatal":true}}"#, json_str(&short(&e.to_string()))));
                return 5;
            }
            Err(e) => {
                eprintln!("flashcp mount: {}: {e}", device.display());
                emit(format!(r#"{{"event":"error","message":{},"fatal":true}}"#, json_str(&short(&e.to_string()))));
                return 1;
            }
        },
    };
    let mut unknown = None;
    match check::busy(&file) {
        Busy::Free => {}
        Busy::InUse(m) => return busy_exit(device, &m),
        Busy::Unknown(why) => unknown = Some(why),
    }
    let len = match dev_len(&file) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("flashcp mount: {e:#}");
            emit(format!(r#"{{"event":"error","message":{},"fatal":true}}"#, json_str(&short(&format!("{e:#}")))));
            return 1;
        }
    };
    let rep = check::check(&file, len);
    let reason = rep.fallback.as_ref().map(|f| f.reason().to_string());
    let extra = unknown.as_ref().map_or(String::new(), |w| format!(r#","busy_unknown":{}"#, json_str(&short(w))));
    emit(format!(
        r#"{{"event":"check","ok":{},"reason":{},"label":{},"serial":{},"pending_journal":{}{extra}}}"#,
        rep.fallback.is_none(),
        reason.as_ref().map_or("null".into(), |r| json_str(r)),
        opt_str(&rep.label),
        serial_str(rep.serial),
        rep.pending_journal
    ));
    match rep.fallback {
        None => {
            if !json {
                println!("flashcp can serve this volume");
            }
            0
        }
        Some(f) => {
            emit(refused_json(&f, device, &rep.label, rep.serial));
            if !json {
                println!("flashcp does not serve this volume: {f}");
            }
            3
        }
    }
}

fn refused_json(f: &Fallback, device: &Path, label: &Option<String>, serial: Option<u32>) -> String {
    format!(
        r#"{{"event":"refused","reason":{},"message":{},"device":{},"label":{},"serial":{}}}"#,
        json_str(f.reason()),
        json_str(&short(&f.to_string())),
        json_str(&short(&device.display().to_string())),
        opt_str(label),
        serial_str(serial)
    )
}

fn busy_exit(device: &Path, msg: &str) -> i32 {
    eprintln!("flashcp mount: {}: {msg}", device.display());
    emit(format!(
        r#"{{"event":"busy","message":{},"device":{}}}"#,
        json_str(&short(msg)),
        json_str(&short(&device.display().to_string()))
    ));
    4
}

// ───────────── events from the driver ─────────────

struct DirtyEmit {
    last: (u64, u64),
    at: Instant,
}

impl DirtyEmit {
    /// At most ~4 per second, and always when it reaches zero (unless already reported).
    fn dirty(&mut self, bytes: u64, files: u64) {
        if (bytes, files) == self.last {
            return;
        }
        let zero = bytes == 0 && files == 0;
        if zero || self.at.elapsed() >= Duration::from_millis(250) {
            self.last = (bytes, files);
            self.at = Instant::now();
            emit(format!(r#"{{"event":"dirty_bytes","bytes":{bytes},"files":{files}}}"#));
        }
    }
}

enum Msg {
    Signal,
    Unmount,
    Fatal,
    Ended(std::io::Result<()>),
    Finished(Result<(), String>),
}

// ───────────── signals ─────────────

static SIG_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    let fd = SIG_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = 1u8;
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

fn install_signals(tx: Sender<Msg>) -> Result<()> {
    let mut fds = [0; 2];
    anyhow::ensure!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0, "pipe");
    SIG_FD.store(fds[1], Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let rfd = fds[0];
    std::thread::spawn(move || {
        let mut b = [0u8; 1];
        loop {
            let n = unsafe { libc::read(rfd, b.as_mut_ptr() as *mut libc::c_void, 1) };
            if n == 1 {
                let _ = tx.send(Msg::Signal);
            } else if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return;
            }
        }
    });
    Ok(())
}

// ───────────── control socket ─────────────

fn serve_control(path: &Path, fs: Fs, tx: Sender<Msg>) -> Result<()> {
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if md.file_type().is_socket() {
            let _ = std::fs::remove_file(path);
        } else {
            anyhow::bail!("{} exists and is not a socket", path.display());
        }
    }
    let old = unsafe { libc::umask(0o177) };
    let l = UnixListener::bind(path);
    unsafe { libc::umask(old) };
    let l = l?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(c) = c else { continue };
            let (fs, tx) = (fs.clone(), tx.clone());
            std::thread::spawn(move || {
                let Ok(w) = c.try_clone() else { return };
                let mut w = w;
                for line in BufReader::new(c).lines() {
                    let Ok(line) = line else { return };
                    let reply = control_reply(line.trim(), &fs, &tx);
                    if writeln!(w, "{reply}").is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(())
}

fn control_reply(cmd: &str, fs: &Fs, tx: &Sender<Msg>) -> String {
    let fail = |e: String| format!(r#"{{"ok":false,"error":{}}}"#, json_str(&short(&e)));
    match cmd {
        "status" => {
            let s = fs.status();
            format!(
                r#"{{"ok":true,"proto":1,"state":"{}","dirty_bytes":{},"dirty_files":{},"open_files":{},"free":{}}}"#,
                s.state, s.dirty_bytes, s.dirty_files, s.open_files, s.free_bytes
            )
        }
        "flush" => match fs.flush() {
            Ok(()) => r#"{"ok":true,"state":"idle"}"#.into(),
            Err(e) => fail(format!("{e:#}")),
        },
        "prepare-eject" => match fs.prepare_eject() {
            Ok(()) => r#"{"ok":true,"safe":true}"#.into(),
            Err(e) => fail(format!("{e:#}")),
        },
        "resume" => {
            fs.resume();
            r#"{"ok":true}"#.into()
        }
        "unmount" => {
            let _ = tx.send(Msg::Unmount);
            r#"{"ok":true}"#.into()
        }
        _ => fail("unknown command".into()),
    }
}

// ───────────── unmounting ─────────────

fn fusermount(mp: &Path, lazy: bool) -> bool {
    let mut c = std::process::Command::new("fusermount3");
    c.arg("-u");
    if lazy {
        c.arg("-z");
    }
    c.arg(mp)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn unmount(mp: &Path) {
    if !fusermount(mp, false) {
        fusermount(mp, true);
    }
}

fn fail_json(msg: &str) -> String {
    format!(r#"{{"event":"error","message":{},"fatal":true}}"#, json_str(&short(msg)))
}

/// `flashcp mount`. Returns the exit code.
pub fn run_mount(a: MountArgs) -> i32 {
    JSON.store(a.json, Ordering::Relaxed);
    // 1. the claim
    let claim = match a.device_fd {
        Some(fd) => claim_fd(fd),
        None => crate::exfat::open_target(&a.device),
    };
    let target = match claim {
        Ok(t) => t,
        Err(e) => {
            let m = format!("{e:#}");
            eprintln!("flashcp mount: {m}");
            let code = io_code(&e);
            return if is_busy_msg(&m) || code == libc::EBUSY || code == libc::EWOULDBLOCK {
                emit(format!(
                    r#"{{"event":"busy","message":{},"device":{}}}"#,
                    json_str(&short(&m)),
                    json_str(&short(&a.device.display().to_string()))
                ));
                4
            } else if code == libc::EACCES || code == libc::EPERM {
                emit(fail_json(&m));
                5
            } else {
                emit(fail_json(&m));
                1
            };
        }
    };
    // the driver does buffered, unaligned I/O
    crate::exfat::set_direct(&target.file, false);
    let len = target.bytes;
    let file = match target.file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            emit(fail_json(&e.to_string()));
            return 1;
        }
    };
    // 2. the driver
    let mut opts = FsOpts { idle: Duration::from_millis(a.idle_ms), ..FsOpts::default() };
    if let Some(mb) = a.max_dirty_mb {
        opts.max_dirty = mb.max(1) << 20;
    }
    let (fs, rec) = match super::mount_file(file, len, opts) {
        Ok(x) => x,
        Err(e) => {
            let m = format!("{e:#}");
            eprintln!("flashcp mount: {m}");
            if let Some(f) = e.downcast_ref::<Fallback>() {
                let rep = check::check(&target.file, len);
                emit(refused_json(f, &a.device, &rep.label, rep.serial));
                return 3;
            }
            emit(fail_json(&m));
            return if e.downcast_ref::<Refused>().is_some() { 6 } else { 1 };
        }
    };
    // 3. events from the driver
    let dirty =
        Arc::new(Mutex::new(DirtyEmit { last: (u64::MAX, u64::MAX), at: Instant::now() - Duration::from_secs(1) }));
    let (tx, rx) = channel::<Msg>();
    {
        let (dirty, tx) = (dirty.clone(), tx.clone());
        fs.on_note(move |n| match n {
            Note::Dirty { bytes, files } => dirty.lock().unwrap().dirty(bytes, files),
            Note::Committing { bytes, files, reason } => {
                emit(format!(r#"{{"event":"committing","bytes":{bytes},"files":{files},"reason":"{reason}"}}"#))
            }
            Note::Committed { bytes, files, secs } => {
                emit(format!(r#"{{"event":"committed","bytes":{bytes},"files":{files},"secs":{secs:.3}}}"#))
            }
            Note::Error { message, fatal } => {
                emit(format!(r#"{{"event":"error","message":{},"fatal":{fatal}}}"#, json_str(&short(&message))));
                if fatal {
                    let _ = tx.send(Msg::Fatal);
                }
            }
        });
    }
    if let Err(e) = install_signals(tx.clone()) {
        emit(fail_json(&format!("{e:#}")));
        let _ = fs.shutdown();
        return 1;
    }
    if let Some(p) = &a.control
        && let Err(e) = serve_control(p, fs.clone(), tx.clone())
    {
        emit(fail_json(&format!("control socket: {e:#}")));
        let _ = fs.shutdown();
        return 1;
    }
    // 4. FUSE
    let cfg = super::fuse::Adapter::config(a.threads);
    let session = match fuser::Session::new(super::fuse::Adapter::new(fs.clone()), &a.mountpoint, &cfg) {
        Ok(s) => s,
        Err(e) => {
            let m = format!("FUSE mount: {e}");
            eprintln!("flashcp mount: {m}");
            emit(fail_json(&m));
            let _ = fs.shutdown();
            if let Some(p) = &a.control {
                let _ = std::fs::remove_file(p);
            }
            return if matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound) {
                5
            } else {
                1
            };
        }
    };
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let r = session.run();
            let _ = tx.send(Msg::Ended(r));
        });
    }
    let st = fs.statfs();
    let (cluster, serial) = (fs.geometry().1, fs.geometry().2);
    let recovered = match rec {
        Recovered::Nothing => "null",
        Recovered::Replayed(_) => "\"replayed\"",
    };
    emit(format!(
        r#"{{"event":"mounted","proto":1,"device":{},"mountpoint":{},"label":{},"serial":"{serial:08x}","size":{},"free":{},"cluster":{cluster},"pid":{},"recovered":{recovered}}}"#,
        json_str(&short(&a.device.display().to_string())),
        json_str(&short(&a.mountpoint.display().to_string())),
        json_str(&short(&fs.label())),
        st.blocks * st.bsize as u64,
        st.bfree * st.bsize as u64,
        std::process::id()
    ));
    eprintln!("flashcp mount: serving {} at {}", a.device.display(), a.mountpoint.display());

    // 5. lifetime
    let ended = Arc::new(AtomicBool::new(false));
    let mut started: Option<(Instant, bool)> = None; // (when, graceful)
    let mut signals = 0;
    let mut fatal = false;
    let mp = a.mountpoint.clone();
    let start = |graceful: bool, ended: Arc<AtomicBool>, fs: Fs, dirty: Arc<Mutex<DirtyEmit>>, tx: Sender<Msg>| {
        let mp = mp.clone();
        std::thread::spawn(move || {
            let mut r: Result<()> = Ok(());
            if graceful {
                r = fs.flush();
                unmount(&mp);
                let t = Instant::now();
                while !ended.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            let s = fs.status();
            if s.dirty_bytes > 0 || s.dirty_files > 0 {
                dirty.lock().unwrap().dirty(s.dirty_bytes, s.dirty_files);
            }
            let r2 = fs.shutdown();
            let r = r.and(r2);
            dirty.lock().unwrap().dirty(fs.pending_bytes(), 0);
            let _ = tx.send(Msg::Finished(r.map_err(|e| format!("{e:#}"))));
        });
    };
    let force = |fs: &Fs, why: &str| -> ! {
        let lost = fs.pending_bytes();
        emit(fail_json(&format!("{why}: {lost} bytes of pending data were not written")));
        unmount(&mp);
        emit(r#"{"event":"unmounted","clean":false}"#.into());
        std::process::exit(1);
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Msg::Signal) => {
                signals += 1;
                match started {
                    None => {
                        started = Some((Instant::now(), true));
                        start(true, ended.clone(), fs.clone(), dirty.clone(), tx.clone());
                    }
                    Some((_, graceful)) => {
                        if signals >= 2 || graceful {
                            force(&fs, "second signal");
                        }
                    }
                }
            }
            Ok(Msg::Unmount) | Ok(Msg::Fatal) => {
                if started.is_none() {
                    started = Some((Instant::now(), true));
                    start(true, ended.clone(), fs.clone(), dirty.clone(), tx.clone());
                }
            }
            Ok(Msg::Ended(r)) => {
                if let Err(e) = r {
                    eprintln!("flashcp mount: FUSE session ended: {e}");
                }
                ended.store(true, Ordering::SeqCst);
                if started.is_none() {
                    // detached (gio mount -u, umount -l): finish every pending byte first
                    started = Some((Instant::now(), false));
                    start(false, ended.clone(), fs.clone(), dirty.clone(), tx.clone());
                }
            }
            Ok(Msg::Finished(r)) => {
                if let Some(p) = &a.control {
                    let _ = std::fs::remove_file(p);
                }
                return match r {
                    Ok(()) if !fatal => {
                        emit(r#"{"event":"unmounted","clean":true}"#.into());
                        0
                    }
                    other => {
                        let m = other.err().unwrap_or_else(|| "a commit failed".into());
                        emit(fail_json(&format!("{m}: {} bytes were not written", fs.pending_bytes())));
                        emit(r#"{"event":"unmounted","clean":false}"#.into());
                        1
                    }
                };
            }
            Err(_) => {
                if let Some((t, true)) = started
                    && t.elapsed() > Duration::from_secs(30)
                {
                    force(&fs, "commit did not finish within 30 s");
                }
            }
        }
        if !fatal && matches!(fs.status().state, "read_only") && started.is_some() {
            fatal = true;
        }
    }
}

/// `--device-fd N`: a block device goes through the usual checks; a regular file (an image)
/// is adopted and flock'ed.
fn claim_fd(fd: i32) -> Result<crate::exfat::Target> {
    anyhow::ensure!(
        fd >= 0 && unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1,
        "--device-fd {fd} is not an open file descriptor"
    );
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    anyhow::ensure!(unsafe { libc::fstat(fd, &mut st) } == 0, "fstat fd {fd}");
    if st.st_mode & libc::S_IFMT == libc::S_IFBLK {
        return crate::exfat::open_target_fd(fd, true);
    }
    anyhow::ensure!(
        st.st_mode & libc::S_IFMT == libc::S_IFREG,
        "--device-fd {fd} is neither a block device nor a regular file"
    );
    let acc = unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_ACCMODE;
    anyhow::ensure!(acc == libc::O_RDWR, "--device-fd {fd} must be opened read-write");
    let file = unsafe { File::from_raw_fd(fd) };
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    anyhow::ensure!(r == 0, "fd {fd} is locked by another process (another flashcp run?)");
    let md = file.metadata()?;
    anyhow::ensure!(md.len() > 0, "image file is empty");
    let flags = crate::exfat::FlagGuard::new(&file)?;
    Ok(crate::exfat::Target { bytes: md.len(), file, part_off: 0, direct: false, _flags: flags })
}
