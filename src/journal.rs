//! The redo journal of `flashcp sync`: every metadata write that commits a run, with the device
//! bytes before and after it, kept TWICE:
//!
//! - on the local machine, at `$XDG_STATE_HOME/flashcp/sync-journal/<serial>-<job>.json`
//!   (`~/.local/state/...` without it; `<serial>` is the VolumeSerialNumber as 8 hex digits,
//!   `<job>` the run's random 128-bit job id), written and fsynced (the file, its directory and
//!   every directory created on the way) before the first byte goes to the device;
//! - on the volume itself, as the regular file `/.flashcp-sync-journal` (Hidden | System |
//!   Archive), written into free clusters with the file data and linked into the root directory
//!   (one entry set inside one 512-byte sector, so it appears atomically) only once the data,
//!   bitmap and FAT phases are on the device; unlinked again (primary entry first) at the end.
//!
//! Both copies hold the job id, the identity of the volume and device ([`Ident`]:
//! VolumeSerialNumber, the boot sector geometry and the device's identity), the moves of the
//! run, and the writes of every phase from `unlink` to `clean` (the phases that change what the
//! volume shows, [`REPLAYED`]). The host copy also holds the writes before them (`data`, as far
//! as later metadata lands on it, `alloc`, `fat`, `journal`): they are checked, never
//! replayed. The volume copy describes writes and never contains itself (its own entry set and
//! bitmap bits are written in `journal` and `alloc`, which it does not hold). Each copy is
//! checksummed (xxh3), and a damaged one is refused.
//!
//! [`recover`] runs at the start of every sync (and alone for `flashcp sync --recover`). For a
//! journal it replays, it first checks that every 512-byte sector the writes touch holds one of
//! the states the writes take it through (the state before the first of them, or after any of
//! them: a crash, even a torn multi-sector write, leaves each sector in one of those; the boot
//! sector's VolumeFlags and PercentInUse are not compared), then replays every write in order (a
//! sync per phase). Anything else in such a sector means the volume changed since: nothing is
//! written. What it does, by what it finds (H = the host journals of this serial, V = a live
//! on-volume journal):
//!
//! | V                         | H                                   | volume | action                                           |
//! |---------------------------|-------------------------------------|--------|--------------------------------------------------|
//! | valid, job J              | one with job J and the same identity | any    | replay it; remove it                             |
//! | valid                     | none of job J with the same identity | any    | replay V; every H stays (warning)                |
//! | damaged, or serial / geometry ≠ the boot sector's | any      | any    | refused, nothing written, everything kept         |
//! | none                      | exactly one with the same identity  | dirty  | the dirty flag only is cleared (if every sector it covers holds a journaled state); removed |
//! | none                      | several with the same identity      | dirty  | refused (kept, warning)                          |
//! | none                      | same identity                       | clean  | stale (the run finished, or fsck ran): removed   |
//! | none                      | none with the same identity         | any    | kept (warning); a dirty volume is refused        |
//!
//! A clean volume with a live V (fsck.exfat cleared the dirty flag but kept the file) is
//! replayed like a dirty one, after setting the dirty flag again, if the check passes; else the
//! sync is refused and V kept (its moves are listed).
//!
//! The one case decided without the job id is "H, no V, dirty": no V means the run never got
//! past `fat` (V appears atomically in `journal`, after which it stays until `unjournal`), or
//! already passed `unjournal` (everything committed). Either way the volume shows a complete
//! tree, the old or the new one, plus clusters only this run allocated (leaked, as by an
//! interrupted `add`) and, after `unjournal`, the orphan secondaries of V's entry set (cleared
//! by the next sync). Nothing is replayed (the file data may be incomplete); clearing the flag
//! is the only write, and only when the serial, the geometry and the device identity all match
//! and the sectors the run would have changed hold its states.

use crate::add::Patches;
use crate::sim::trace::{self, Op as TOp};
use crate::volume::{
    FLAG_DIRTY, MAX_DIR_BYTES, SYNC_JOURNAL_NAME, checksum_ok_set, is_sync_journal_set, le16, le32, le64,
};
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const FORMAT: &str = "flashcp-sync-journal";
const VERSION: u32 = 2;
const SECTOR: u64 = 512;
/// The phases a recovery replays: those that change what the volume shows. The host copy also
/// holds the writes before them (`data` for what metadata later lands on, `alloc`, `fat`,
/// `journal`), for the check only: they are never replayed.
pub(crate) const REPLAYED: [&str; 6] = ["unlink", "link", "retire", "free", "unjournal", "clean"];
/// Test hook: replaces the device part of the identity.
pub(crate) const DEVICE_ID_ENV: &str = "FLASHCP_TEST_DEVICE_ID";
/// An on-volume journal larger than this is not one of ours.
const MAX_VOLUME_JOURNAL: u64 = 1 << 30;

/// One metadata write: `post` replaces `pre` at device byte `off`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JWrite {
    pub(crate) phase: String,
    pub(crate) off: u64,
    pub(crate) pre: Vec<u8>,
    pub(crate) post: Vec<u8>,
}

/// A move of the run (shown when a journal cannot be replayed; recovery does not need it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MoveRec {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) first: u32,
    pub(crate) len: u64,
    pub(crate) valid_len: u64,
    pub(crate) no_fat_chain: bool,
    pub(crate) mtime: (u32, u8, u8),
}

/// What a journal must match before anything is written: the volume (serial, geometry) and the
/// device holding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ident {
    pub(crate) serial: u32,
    pub(crate) geometry: String,
    pub(crate) device: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Journal {
    pub(crate) job: [u8; 16],
    pub(crate) ident: Ident,
    pub(crate) moves: Vec<MoveRec>,
    pub(crate) writes: Vec<JWrite>,
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 * b.len());
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// The string or number stored under `"key":` in a one-line JSON object (no escapes: only our
/// own hex and number fields are read this way).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let rest = &line[line.find(&pat)? + pat.len()..];
    if let Some(r) = rest.strip_prefix('"') {
        return Some(&r[..r.find('"')?]);
    }
    Some(&rest[..rest.find([',', '}']).unwrap_or(rest.len())])
}

/// The JSON string under `"key":` (as [`crate::events::json_str`] escapes it).
fn string_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let mut it = line[line.find(&pat)? + pat.len()..].chars();
    let mut out = String::new();
    loop {
        match it.next()? {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let h: String = (0..4).filter_map(|_| it.next()).collect();
                    out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?)?);
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

/// The geometry fields of a boot sector a journal must match: BytesPerSectorShift,
/// SectorsPerClusterShift, FatOffset, FatLength, ClusterHeapOffset, ClusterCount, VolumeLength,
/// PartitionOffset.
pub(crate) fn geometry_of(s0: &[u8]) -> String {
    format!(
        "{},{},{},{},{},{},{},{}",
        s0[108],
        s0[109],
        le32(s0, 80),
        le32(s0, 84),
        le32(s0, 88),
        le32(s0, 92),
        le64(s0, 72),
        le64(s0, 64)
    )
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The identity of the device `f` is open on. A block device: the disk's sysfs serial and WWN
/// (when it has them), its stable key (USB serial or WWID), a loop device's backing file, the
/// partition's start and the device size; an image file: its size, st_dev and st_ino.
/// `FLASHCP_TEST_DEVICE_ID` replaces it.
pub(crate) fn device_identity(f: &File) -> String {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if let Some(id) = std::env::var_os(DEVICE_ID_ENV) {
        return format!("test:{}", id.to_string_lossy());
    }
    let Ok(md) = f.metadata() else { return "unknown".into() };
    if md.file_type().is_block_device() {
        let rdev = md.rdev();
        let size = crate::exfat::blk_size(f).unwrap_or(0);
        let mut parts = Vec::new();
        if let Ok(mut p) = fs::canonicalize(format!("/sys/dev/block/{}:{}", libc::major(rdev), libc::minor(rdev))) {
            let start = read_trim(&p.join("start")).unwrap_or_else(|| "0".into());
            if p.join("partition").exists() {
                p.pop();
            }
            for (k, rel) in [
                ("serial", "device/serial"),
                ("wwid", "device/wwid"),
                ("wwn", "wwid"),
                ("backing", "loop/backing_file"),
            ] {
                if let Some(v) = read_trim(&p.join(rel)) {
                    parts.push(format!("{k}={v}"));
                }
            }
            if let Some(k) = crate::devprofile::device_key(rdev) {
                parts.push(format!("key={k}"));
            }
            parts.push(format!("start={start}"));
        }
        parts.push(format!("size={size}"));
        format!("block {}", parts.join(" "))
    } else {
        format!("file size={} dev={} ino={}", md.len(), md.dev(), md.ino())
    }
}

/// The identity of the volume on `dev` whose boot sector is `s0`.
pub(crate) fn ident_of(dev: &File, s0: &[u8]) -> Ident {
    Ident { serial: le32(s0, 100), geometry: geometry_of(s0), device: device_identity(dev) }
}

impl Journal {
    pub(crate) fn encode(&self) -> String {
        let mut s = format!(
            "{{\"format\":\"{FORMAT}\",\"version\":{VERSION},\"job\":\"{}\",\"serial\":\"{:08x}\",\"geometry\":\"{}\",\
             \"device\":\"{}\",\"moves\":[\n",
            hex(&self.job),
            self.ident.serial,
            self.ident.geometry,
            hex(self.ident.device.as_bytes()),
        );
        let moves: Vec<String> = self
            .moves
            .iter()
            .map(|m| {
                format!(
                    "{{\"from\":{},\"to\":{},\"first_cluster\":{},\"length\":{},\"valid_length\":{},\
                     \"no_fat_chain\":{},\"mtime\":\"{:08x}/{:02x}/{:02x}\"}}",
                    crate::events::json_str(&m.from),
                    crate::events::json_str(&m.to),
                    m.first,
                    m.len,
                    m.valid_len,
                    m.no_fat_chain,
                    m.mtime.0,
                    m.mtime.1,
                    m.mtime.2
                )
            })
            .collect();
        s += &moves.join(",\n");
        s += "\n],\"writes\":[\n";
        let writes: Vec<String> = self
            .writes
            .iter()
            .map(|w| {
                format!(
                    "{{\"phase\":\"{}\",\"offset\":{},\"pre\":\"{}\",\"post\":\"{}\"}}",
                    w.phase,
                    w.off,
                    hex(&w.pre),
                    hex(&w.post)
                )
            })
            .collect();
        s += &writes.join(",\n");
        s += "\n],";
        let check = xxhash_rust::xxh3::xxh3_64(s.as_bytes());
        s + &format!("\"check\":\"{check:016x}\"}}\n")
    }

    /// Decodes [`Journal::encode`]'s text (trailing padding after it ignored).
    pub(crate) fn decode(text: &str) -> Result<Journal> {
        let cut = text.rfind("\"check\":\"").context("the sync journal is truncated (no check field)")?;
        let (body, tail) = text.split_at(cut);
        let want = field(tail, "check").and_then(|c| u64::from_str_radix(c, 16).ok());
        ensure!(want == Some(xxhash_rust::xxh3::xxh3_64(body.as_bytes())), "the sync journal is damaged (check)");
        let head = body.lines().next().unwrap_or("");
        ensure!(field(head, "format") == Some(FORMAT), "not a flashcp sync journal");
        ensure!(
            field(head, "version") == Some(&VERSION.to_string()),
            "unsupported sync journal version {:?} (written by another flashcp version)",
            field(head, "version")
        );
        let job: [u8; 16] =
            field(head, "job").and_then(unhex).and_then(|b| b.try_into().ok()).context("journal job id")?;
        let serial = field(head, "serial").and_then(|s| u32::from_str_radix(s, 16).ok()).context("journal serial")?;
        let geometry = field(head, "geometry").context("journal geometry")?.to_string();
        let device = field(head, "device")
            .and_then(unhex)
            .and_then(|b| String::from_utf8(b).ok())
            .context("journal device identity")?;
        let mut moves = Vec::new();
        for line in body.lines().filter(|l| l.starts_with("{\"from\":")) {
            let bad = || format!("bad journal line {line:.80}");
            let mt: Vec<u32> = field(line, "mtime")
                .with_context(bad)?
                .split('/')
                .map(|x| u32::from_str_radix(x, 16))
                .collect::<Result<_, _>>()
                .ok()
                .filter(|v: &Vec<u32>| v.len() == 3)
                .with_context(bad)?;
            moves.push(MoveRec {
                from: string_field(line, "from").with_context(bad)?,
                to: string_field(line, "to").with_context(bad)?,
                first: field(line, "first_cluster").and_then(|x| x.parse().ok()).with_context(bad)?,
                len: field(line, "length").and_then(|x| x.parse().ok()).with_context(bad)?,
                valid_len: field(line, "valid_length").and_then(|x| x.parse().ok()).with_context(bad)?,
                no_fat_chain: field(line, "no_fat_chain") == Some("true"),
                mtime: (mt[0], mt[1] as u8, mt[2] as u8),
            });
        }
        let mut writes = Vec::new();
        for line in body.lines().filter(|l| l.starts_with("{\"phase\":")) {
            let bad = || format!("bad journal line {line:.80}");
            let w = JWrite {
                phase: field(line, "phase").with_context(bad)?.to_string(),
                off: field(line, "offset").and_then(|o| o.parse().ok()).with_context(bad)?,
                pre: field(line, "pre").and_then(unhex).with_context(bad)?,
                post: field(line, "post").and_then(unhex).with_context(bad)?,
            };
            ensure!(w.pre.len() == w.post.len() && !w.pre.is_empty(), "{}", bad());
            writes.push(w);
        }
        Ok(Journal { job, ident: Ident { serial, geometry, device }, moves, writes })
    }

    /// The moves, one per line, for a message.
    fn moves_shown(&self) -> String {
        if self.moves.is_empty() {
            return " (it lists no moves)".into();
        }
        let mut s = String::from("; the moves it lists:");
        for m in &self.moves {
            s += &format!("\n  {} → {} (first cluster {}, {} bytes)", m.from, m.to, m.first, m.len);
        }
        s
    }
}

// ───────────────────────── the host copy ─────────────────────────

#[cfg(test)]
thread_local! {
    /// Test hooks: the state directory to use, and a log of the durability steps.
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static TRACE: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Records a durability step (tests only: `mkdir X`, `fsync X`, `rename X`, `dirty`, ...).
pub(crate) fn trace(ev: impl FnOnce() -> String) {
    #[cfg(test)]
    TRACE.with(|t| {
        if let Some(v) = t.borrow_mut().as_mut() {
            v.push(ev());
        }
    });
    #[cfg(not(test))]
    let _ = ev;
}

/// `$XDG_STATE_HOME/flashcp/sync-journal` (or `$HOME/.local/state/flashcp/sync-journal`).
pub(crate) fn dir() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(d) = TEST_DIR.with(|d| d.borrow().clone()) {
        return Ok(d);
    }
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").context("neither XDG_STATE_HOME nor HOME is set")?)
            .join(".local/state"),
    };
    Ok(base.join("flashcp").join("sync-journal"))
}

fn file_name(serial: u32, job: &[u8; 16]) -> String {
    format!("{serial:08x}-{}.json", hex(job))
}

fn fsync_dir(d: &Path) -> Result<()> {
    File::open(d).and_then(|f| f.sync_all()).with_context(|| format!("fsync {}", d.display()))?;
    trace(|| format!("fsync {}", d.display()));
    Ok(())
}

/// Creates `d` and its missing ancestors (mode 0700), each made durable: after every mkdir,
/// its parent directory is fsynced.
fn create_dirs(d: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut cur = d;
    while fs::symlink_metadata(cur).is_err() {
        missing.push(cur.to_path_buf());
        cur = cur.parent().context("no existing ancestor of the journal directory")?;
    }
    for m in missing.iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(m) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && m.is_dir() => {}
            Err(e) => return Err(e).with_context(|| format!("create {}", m.display())),
        }
        trace(|| format!("mkdir {}", m.display()));
        fsync_dir(m.parent().context("journal directory has no parent")?)?;
    }
    ensure!(d.is_dir(), "{} is not a directory", d.display());
    Ok(())
}

/// Writes the host copy of `text` (the encoding of `j`) durably: every missing directory on
/// the way (see [`create_dirs`]), a temp file, fsynced, renamed into place, the directory
/// fsynced. Returns its path.
pub(crate) fn save(j: &Journal, text: &str) -> Result<PathBuf> {
    let d = dir()?;
    create_dirs(&d)?;
    let name = file_name(j.ident.serial, &j.job);
    let (p, tmp) = (d.join(&name), d.join(format!("{name}.tmp")));
    let mut f = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(text.as_bytes()).and_then(|_| f.sync_all()).with_context(|| format!("write {}", tmp.display()))?;
    trace(|| format!("fsync {}", tmp.display()));
    drop(f);
    fs::rename(&tmp, &p).with_context(|| format!("rename {} into place", tmp.display()))?;
    trace(|| format!("rename {}", p.display()));
    fsync_dir(&d)?;
    Ok(p)
}

/// Removes a host journal (and makes that durable).
pub(crate) fn remove(p: &Path) -> Result<()> {
    match fs::remove_file(p) {
        Ok(()) => fsync_dir(p.parent().unwrap()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", p.display())),
    }
}

/// The host journals kept for `serial`: `<serial>-*.json` (and a `<serial>.json` of an older
/// flashcp, which then fails to decode and is kept).
fn host_journals(serial: u32) -> Result<Vec<PathBuf>> {
    let Ok(d) = dir() else { return Ok(Vec::new()) };
    let rd = match fs::read_dir(&d) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", d.display())),
    };
    let (pre, old) = (format!("{serial:08x}-"), format!("{serial:08x}.json"));
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            (n.starts_with(&pre) && n.ends_with(".json")) || n == old
        })
        .map(|e| e.path())
        .collect();
    v.sort();
    Ok(v)
}

// ───────────────────────── the volume copy ─────────────────────────

/// The live on-volume journal of the volume whose boot sector is `s0`, found by a raw scan of
/// the root directory (its FAT chain, every 32-byte entry up to the end marker; nothing else is
/// parsed, so a torn entry set elsewhere does not hide it). Err for a damaged one.
fn on_volume(dev: &File, s0: &[u8]) -> Result<Option<Journal>> {
    if !(9..=12).contains(&s0[108]) || s0[109] as u32 > 25 - s0[108] as u32 {
        return Ok(None); // not a geometry flashcp writes to (the volume parser says why)
    }
    let bps = 1u64 << s0[108];
    let cs = bps << s0[109];
    let (fat_off, heap_off) = (le32(s0, 80) as u64 * bps, le32(s0, 88) as u64 * bps);
    let (cc, root) = (le32(s0, 92) as u64, le32(s0, 96));
    let valid = |c: u32| c >= 2 && (c as u64) < cc + 2;
    if !valid(root) {
        return Ok(None);
    }
    let fat = |c: u32| -> Result<u32> {
        let mut b = [0u8; 4];
        let at = fat_off + c as u64 * 4;
        trace::serial(TOp::Read, at, 4, || dev.read_exact_at(&mut b, at)).context("read the FAT")?;
        Ok(u32::from_le_bytes(b))
    };
    let read = |c: u32, len: u64| -> Result<Vec<u8>> {
        let mut b = vec![0u8; len as usize];
        let at = heap_off + (c as u64 - 2) * cs;
        trace::serial(TOp::Read, at, len, || dev.read_exact_at(&mut b, at)).context("read a cluster")?;
        Ok(b)
    };
    let mut data = Vec::new();
    let mut c = root;
    for _ in 0..MAX_DIR_BYTES / cs {
        data.extend(read(c, cs)?);
        let next = fat(c)?;
        if !valid(next) {
            break;
        }
        c = next;
    }
    let mut found: Vec<(u32, u64, bool)> = Vec::new();
    let mut i = 0;
    while i + 32 <= data.len() && data[i] != 0 {
        if data[i] == 0x85 {
            let len = 32 * (1 + data[i + 1] as usize);
            if i + len <= data.len() && checksum_ok_set(&data[i..i + len]) && is_sync_journal_set(&data[i..i + len]) {
                let set = &data[i..i + len];
                found.push((le32(set, 52), le64(set, 56), set[33] & 0x02 != 0));
            }
        }
        i += 32;
    }
    ensure!(found.len() <= 1, "the volume's root holds {} files {SYNC_JOURNAL_NAME}; run fsck.exfat", found.len());
    let Some((first, len, nfc)) = found.pop() else { return Ok(None) };
    let damaged = || format!("the on-volume sync journal /{SYNC_JOURNAL_NAME} is damaged; nothing was written");
    ensure!(len > 0 && len <= MAX_VOLUME_JOURNAL, "{}", damaged());
    let n = len.div_ceil(cs);
    let mut text = Vec::with_capacity(len as usize);
    let mut c = first;
    for k in 0..n {
        ensure!(valid(c), "{}", damaged());
        text.extend(read(c, cs)?);
        if k + 1 < n {
            c = if nfc { c + 1 } else { fat(c)? };
        }
    }
    text.truncate(len as usize);
    let text = String::from_utf8(text).ok().with_context(damaged)?;
    Journal::decode(&text).with_context(damaged).map(Some)
}

// ───────────────────────── planning, checking, replaying ─────────────────────────

/// The journal writes of `phases`, in order: per phase sorted by offset and merged where
/// contiguous; `pre` is what the device holds before the write, given every earlier write of
/// the list (read from `dev` now, earlier writes applied in memory).
pub(crate) fn plan_writes(dev: &File, phases: Vec<(&str, Patches)>) -> Result<Vec<JWrite>> {
    let mut model: HashMap<u64, Vec<u8>> = HashMap::new(); // sector → its bytes, as written so far
    let mut out = Vec::new();
    for (phase, p) in phases {
        let mut ws = p.into_inner();
        ws.sort_by_key(|w| w.0);
        let mut merged: Vec<(u64, Vec<u8>)> = Vec::new();
        for (off, b) in ws {
            match merged.last_mut() {
                Some(l) if l.0 + l.1.len() as u64 == off => l.1.extend_from_slice(&b),
                Some(l) => {
                    ensure!(l.0 + l.1.len() as u64 <= off, "internal error: overlapping metadata writes ({phase})");
                    merged.push((off, b));
                }
                None => merged.push((off, b)),
            }
        }
        for (off, post) in merged {
            let (s0, s1) = (off / SECTOR, (off + post.len() as u64).div_ceil(SECTOR));
            if (s0..s1).any(|s| !model.contains_key(&s)) {
                let mut buf = vec![0u8; ((s1 - s0) * SECTOR) as usize];
                trace::serial(TOp::Read, s0 * SECTOR, buf.len() as u64, || dev.read_exact_at(&mut buf, s0 * SECTOR))
                    .with_context(|| format!("read metadata at {off}"))?;
                for (k, s) in (s0..s1).enumerate() {
                    model.entry(s).or_insert_with(|| buf[k * SECTOR as usize..(k + 1) * SECTOR as usize].to_vec());
                }
            }
            let mut pre = Vec::with_capacity(post.len());
            for (i, &b) in post.iter().enumerate() {
                let at = off + i as u64;
                let sec = model.get_mut(&(at / SECTOR)).unwrap();
                pre.push(sec[(at % SECTOR) as usize]);
                sec[(at % SECTOR) as usize] = b;
            }
            out.push(JWrite { phase: phase.to_string(), off, pre, post });
        }
    }
    Ok(out)
}

/// The phases of `writes`, in order, each with its writes.
pub(crate) fn phases(writes: &[JWrite]) -> Vec<(&str, &[JWrite])> {
    let mut out: Vec<(&str, &[JWrite])> = Vec::new();
    let mut i = 0;
    while i < writes.len() {
        let mut j = i + 1;
        while j < writes.len() && writes[j].phase == writes[i].phase {
            j += 1;
        }
        out.push((&writes[i].phase, &writes[i..j]));
        i = j;
    }
    out
}

/// Writes one phase's `post`s and syncs.
pub(crate) fn apply(dev: &File, ws: &[JWrite]) -> Result<()> {
    for w in ws {
        trace::serial(TOp::Write, w.off, w.post.len() as u64, || dev.write_all_at(&w.post, w.off))
            .with_context(|| format!("write metadata at {}", w.off))?;
    }
    trace::sync(|| dev.sync_all()).context("sync")
}

/// Per touched sector: the bytes some write covers (mask) and every state they go through.
struct SectorHistory {
    mask: Vec<bool>,
    states: Vec<Vec<u8>>,
}

/// Checks that each sector `writes` touch holds one of its journaled states (in the boot
/// sector, VolumeFlags and PercentInUse are not compared: fsck.exfat rewrites them). Err names
/// the first sector that does not.
pub(crate) fn check(dev: &File, writes: &[JWrite]) -> Result<()> {
    let mut hist: BTreeMap<u64, SectorHistory> = BTreeMap::new();
    for w in writes {
        let (s0, s1) = (w.off / SECTOR, (w.off + w.pre.len() as u64).div_ceil(SECTOR));
        for s in s0..s1 {
            let h = hist.entry(s).or_insert_with(|| SectorHistory {
                mask: vec![false; SECTOR as usize],
                states: vec![vec![0u8; SECTOR as usize]],
            });
            let lo = (s * SECTOR).max(w.off);
            let hi = ((s + 1) * SECTOR).min(w.off + w.pre.len() as u64);
            let mut next = h.states.last().unwrap().clone();
            for at in lo..hi {
                let (k, i) = ((at - s * SECTOR) as usize, (at - w.off) as usize);
                if !h.mask[k] {
                    // First write of this byte: every earlier state held its `pre`.
                    h.mask[k] = true;
                    for st in &mut h.states {
                        st[k] = w.pre[i];
                    }
                    next[k] = w.pre[i];
                }
                next[k] = w.post[i];
            }
            h.states.push(next);
        }
    }
    if let Some(h) = hist.get_mut(&0) {
        for k in [106, 107, 112] {
            h.mask[k] = false;
        }
    }
    for (&s, h) in &hist {
        let mut cur = vec![0u8; SECTOR as usize];
        trace::serial(TOp::Read, s * SECTOR, SECTOR, || dev.read_exact_at(&mut cur, s * SECTOR))
            .with_context(|| format!("read sector {s}"))?;
        let same = |st: &Vec<u8>| (0..SECTOR as usize).all(|k| !h.mask[k] || st[k] == cur[k]);
        if !h.states.iter().any(same) {
            bail!(
                "the volume changed since the interrupted sync (device sector {s} holds none of the journaled \
                 states); nothing was written"
            );
        }
    }
    Ok(())
}

/// What [`recover`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    /// No journal for this volume (or not an exFAT volume at all).
    None,
    /// The volume was clean and a host journal of this very device was stale: removed.
    Stale,
    /// The interrupted run's commit writes were replayed (`writes` of them), from the journal on
    /// the volume (`from_volume`) or the host's copy of it.
    Replayed { writes: usize, from_volume: bool },
    /// The run had committed nothing yet (or everything but its cleanup): only its dirty flag
    /// was cleared.
    DirtyCleared,
}

fn warn_kept(p: &Path, why: &str) {
    eprintln!("warning: kept the sync journal {} ({why})", p.display());
}

fn set_dirty(dev: &File, s0: &[u8], dirty: bool) -> Result<()> {
    let f = le16(s0, 106);
    let f = if dirty { f | FLAG_DIRTY } else { f & !FLAG_DIRTY };
    trace::serial(TOp::Write, 106, 2, || dev.write_all_at(&f.to_le_bytes(), 106)).context("write VolumeFlags")?;
    trace::sync(|| dev.sync_all()).context("sync")
}

/// Completes (or cleans up after) an interrupted sync of the volume on `dev` (see the module
/// doc). Writes nothing unless the decision table says so.
pub(crate) fn recover(dev: &File) -> Result<Recovery> {
    let mut s0 = vec![0u8; 512];
    if trace::serial(TOp::Read, 0, 512, || dev.read_exact_at(&mut s0, 0)).is_err() || &s0[3..11] != b"EXFAT   " {
        return Ok(Recovery::None);
    }
    let me = ident_of(dev, &s0);
    let dirty = le16(&s0, 106) & FLAG_DIRTY != 0;
    let mut hosts: Vec<(PathBuf, Journal)> = Vec::new();
    for p in host_journals(me.serial)? {
        match fs::read_to_string(&p).map_err(anyhow::Error::from).and_then(|t| Journal::decode(&t)) {
            Ok(j) => hosts.push((p, j)),
            Err(e) => warn_kept(&p, &format!("{e:#}")),
        }
    }
    let vj = on_volume(dev, &s0)?;
    if let Some(v) = vj {
        ensure!(
            v.ident.serial == me.serial && v.ident.geometry == me.geometry,
            "the on-volume sync journal /{SYNC_JOURNAL_NAME} is for volume {:08x} ({}), not this one ({:08x}, {}); \
             nothing was written",
            v.ident.serial,
            v.ident.geometry,
            me.serial,
            me.geometry
        );
        let host = hosts.iter().position(|(_, h)| h.job == v.job && h.ident == me);
        for (k, (p, h)) in hosts.iter().enumerate() {
            if Some(k) != host {
                let why = if h.job != v.job { "another run's job id" } else { "the run's job id, another device" };
                warn_kept(p, why);
            }
        }
        let (j, from_volume) = match host {
            Some(k) => (&hosts[k].1, false),
            None => (&v, true),
        };
        check(dev, &j.writes).map_err(|e| {
            anyhow::anyhow!(
                "{e:#}. The interrupted sync's journal is kept, on the volume as /{SYNC_JOURNAL_NAME} (Hidden, \
                 System){}{}",
                if dirty { "" } else { " (the volume was marked clean since: fsck.exfat?)" },
                v.moves_shown()
            )
        })?;
        if !dirty {
            set_dirty(dev, &s0, true)?;
        }
        let mut n = 0;
        for (_, ws) in phases(&j.writes).into_iter().filter(|p| REPLAYED.contains(&p.0)) {
            apply(dev, ws)?;
            n += ws.len();
        }
        if let Some(k) = host {
            remove(&hosts[k].0)?;
        }
        return Ok(Recovery::Replayed { writes: n, from_volume });
    }
    let (mine, other): (Vec<_>, Vec<_>) = hosts.into_iter().partition(|(_, h)| h.ident == me);
    for (p, h) in &other {
        let why = if h.ident.geometry != me.geometry { "another volume geometry" } else { "another device" };
        warn_kept(p, why);
    }
    if mine.is_empty() {
        return Ok(Recovery::None);
    }
    if !dirty {
        for (p, _) in &mine {
            remove(p)?;
        }
        return Ok(Recovery::Stale);
    }
    if mine.len() > 1 {
        for (p, _) in &mine {
            warn_kept(p, "several journals of this device and no journal on the volume: which run is unknown");
        }
        return Ok(Recovery::None);
    }
    let (p, h) = &mine[0];
    check(dev, &h.writes).with_context(|| format!("sync journal {} (kept)", p.display()))?;
    set_dirty(dev, &s0, false)?;
    remove(p)?;
    Ok(Recovery::DirtyCleared)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn tmp(tag: &str, len: usize) -> (PathBuf, File) {
        let p = std::env::temp_dir().join(format!("flashcp-journal-{tag}-{}.img", std::process::id()));
        let f = File::options().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        let mut b = vec![0u8; len];
        for (i, x) in b.iter_mut().enumerate() {
            *x = (i * 7 % 251) as u8;
        }
        f.write_all_at(&b, 0).unwrap();
        (p, f)
    }

    fn patches(ws: &[(u64, &[u8])]) -> Patches {
        let mut p = Patches::default();
        for &(o, b) in ws {
            p.add(o, b.to_vec());
        }
        p
    }

    /// Runs `f` with the host journals in `d` and the durability log on; returns the log.
    pub(crate) fn traced<T>(d: &Path, f: impl FnOnce() -> T) -> (T, Vec<String>) {
        TEST_DIR.with(|t| *t.borrow_mut() = Some(d.to_path_buf()));
        TRACE.with(|t| *t.borrow_mut() = Some(Vec::new()));
        let r = f();
        TEST_DIR.with(|t| *t.borrow_mut() = None);
        (r, TRACE.with(|t| t.borrow_mut().take().unwrap()))
    }

    /// Every created directory's parent is fsynced after its mkdir, the file is fsynced before
    /// its rename and the directory after it, and all of it comes before `stop` (if given).
    pub(crate) fn assert_durable(log: &[String], base: &Path, stop: Option<&str>) {
        let pos = |e: &str| log.iter().position(|l| l == e).unwrap_or_else(|| panic!("no {e:?} in {log:#?}"));
        let end = stop.map_or(log.len(), pos);
        let mut created = 0;
        for (k, l) in log.iter().enumerate() {
            if let Some(d) = l.strip_prefix("mkdir ") {
                created += 1;
                let parent = Path::new(d).parent().unwrap().display().to_string();
                let f = log[k + 1..].iter().position(|l| *l == format!("fsync {parent}")).map(|x| x + k + 1);
                assert!(f.is_some_and(|f| f < end), "{d}: its parent is not fsynced after the mkdir: {log:#?}");
                assert!(k < end, "mkdir {d} after {stop:?}: {log:#?}");
            }
        }
        assert!(created >= 3, "expected the missing directory chain to be created: {log:#?}");
        let ren = log.iter().position(|l| l.starts_with("rename ")).expect("no rename");
        let file = log[ren].strip_prefix("rename ").unwrap();
        assert!(log[..ren].iter().any(|l| *l == format!("fsync {file}.tmp")), "temp file not fsynced: {log:#?}");
        let dirf = format!("fsync {}", Path::new(file).parent().unwrap().display());
        assert!(log[ren + 1..end].contains(&dirf), "directory not fsynced after the rename: {log:#?}");
        assert!(ren < end && Path::new(file).starts_with(base));
    }

    fn sample() -> Journal {
        Journal {
            job: [7; 16],
            ident: Ident { serial: 0xDEAD_BEEF, geometry: "9,3,2048".into(), device: "file \"x\" ino=5".into() },
            moves: vec![MoveRec {
                from: "/a \"q\"\\".into(),
                to: "/b\n".into(),
                first: 5,
                len: 9,
                valid_len: 9,
                no_fat_chain: true,
                mtime: (1, 2, 0x80),
            }],
            writes: vec![
                JWrite { phase: "unlink".into(), off: 4096, pre: vec![0, 1], post: vec![2, 3] },
                JWrite { phase: "free".into(), off: 7, pre: vec![0xff], post: vec![0] },
            ],
        }
    }

    #[test]
    fn encode_decode_round_trip_and_damage_is_refused() {
        let j = sample();
        let text = j.encode();
        let back = Journal::decode(&text).unwrap();
        assert_eq!((back.job, &back.ident, &back.moves, &back.writes), (j.job, &j.ident, &j.moves, &j.writes));
        // Padding after it (the on-volume copy fills its clusters) is ignored.
        assert!(Journal::decode(&format!("{text}\n\n\n   \n")).is_ok());
        assert!(Journal::decode(&text.replace("4096", "4097")).is_err());
        assert!(Journal::decode(&text[..text.len() / 2]).is_err());
    }

    #[test]
    fn saving_creates_every_missing_directory_durably() {
        let base = std::env::temp_dir().join(format!("flashcp-journal-dirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let d = base.join("state/flashcp/sync-journal");
        let j = sample();
        let (p, log) = traced(&d, || save(&j, &j.encode()).unwrap());
        assert_durable(&log, &base, None);
        assert_eq!(p, d.join(file_name(j.ident.serial, &j.job)));
        assert_eq!(Journal::decode(&fs::read_to_string(&p).unwrap()).unwrap().job, j.job);
        // Again, with the directories there: only the file is created.
        let (_, log) = traced(&d, || save(&j, &j.encode()).unwrap());
        assert!(!log.iter().any(|l| l.starts_with("mkdir")), "{log:#?}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn overlapping_writes_compose_and_every_intermediate_state_is_accepted() {
        let (p, f) = tmp("compose", 4096);
        let ws = plan_writes(
            &f,
            vec![
                ("link", patches(&[(500, &[1u8; 20]), (520, &[2u8; 4])])),
                ("retire", patches(&[(510, &[3u8; 2])])),
                ("free", patches(&[(505, &[4u8; 10])])),
            ],
        )
        .unwrap();
        // link merged into one write; retire's pre holds link's post.
        assert_eq!(ws.len(), 3);
        assert_eq!((ws[0].off, ws[0].post.len()), (500, 24));
        assert_eq!(ws[1].pre, vec![1, 1]);
        assert_eq!(ws[2].pre, vec![1, 1, 1, 1, 1, 3, 3, 1, 1, 1]);
        check(&f, &ws).unwrap(); // the state before
        for (k, (_, phase)) in phases(&ws).into_iter().enumerate() {
            apply(&f, phase).unwrap();
            check(&f, &ws).unwrap_or_else(|e| panic!("after phase {k}: {e}"));
        }
        // A torn write: only the first sector of the link write (500..512).
        let (p2, f2) = tmp("torn", 4096);
        f2.write_all_at(&ws[0].post[..12], 500).unwrap();
        check(&f2, &ws).unwrap();
        // A byte nobody journaled a state for is refused.
        f2.write_all_at(&[0xEE], 506).unwrap();
        assert!(format!("{:#}", check(&f2, &ws).unwrap_err()).contains("changed since"));
        // The boot sector's flags and PercentInUse are not compared.
        let ws0 = plan_writes(&f2, vec![("clean", patches(&[(0, &[9u8; 512])]))]).unwrap();
        f2.write_all_at(&[9u8; 512], 0).unwrap();
        f2.write_all_at(&[1, 2], 106).unwrap();
        f2.write_all_at(&[77], 112).unwrap();
        check(&f2, &ws0).unwrap();
        f2.write_all_at(&[1], 105).unwrap();
        assert!(check(&f2, &ws0).is_err());
        fs::remove_file(p).unwrap();
        fs::remove_file(p2).unwrap();
    }
}
