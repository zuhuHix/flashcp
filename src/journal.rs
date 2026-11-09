//! The redo journal of `flashcp sync`: every metadata write of a run, with the device bytes
//! before and after it, kept on the local machine (not on the device: exFAT has no spare
//! metadata area flashcp could use safely) at
//! `$XDG_STATE_HOME/flashcp/sync-journal/<serial>.json` (`~/.local/state/...` without it),
//! where `<serial>` is the VolumeSerialNumber as 8 hex digits.
//!
//! The journal is written and fsynced (file and directory) before the first write it covers
//! and removed after the last. A sync first writes an UNDO journal (the boot sector before
//! VolumeDirty was set), then sets VolumeDirty and writes the file data, then replaces it by the
//! REDO journal (alloc .. free and the final clean boot-sector write) before the first
//! metadata write. [`recover`] runs at the start of every sync (and alone for
//! `flashcp sync --recover`): for a dirty volume with a journal it checks that every 512-byte
//! sector a write touches holds one of the states the journal's writes take it through (the
//! state before the first of them, or after any of them: a crash, even a torn multi-sector
//! write, leaves each sector in one of those), then replays every write in order (a sync per
//! phase) and removes the journal. Anything else in such a sector means the volume changed
//! since: nothing is written. A journal for a clean volume is stale (the run finished, or
//! `fsck.exfat` repaired the volume since) and is removed.

use crate::add::Patches;
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileExt};
use std::path::{Path, PathBuf};

const FORMAT: &str = "flashcp-sync-journal";
const VERSION: u32 = 1;
const SECTOR: u64 = 512;

/// One metadata write: `post` replaces `pre` at device byte `off`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JWrite {
    pub(crate) phase: String,
    pub(crate) off: u64,
    pub(crate) pre: Vec<u8>,
    pub(crate) post: Vec<u8>,
}

/// A move of the run, recorded for humans and debugging (recovery does not need it).
#[derive(Clone, Debug)]
pub(crate) struct MoveRec {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) first: u32,
    pub(crate) len: u64,
    pub(crate) valid_len: u64,
    pub(crate) no_fat_chain: bool,
    pub(crate) mtime: (u32, u8, u8),
}

#[derive(Clone, Debug)]
pub(crate) struct Journal {
    pub(crate) serial: u32,
    pub(crate) device: String,
    /// "undo" (only the dirty flag is to be taken back) or "redo".
    pub(crate) mode: String,
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

/// The string or number stored under `"key":` in a one-line JSON object (no escapes needed:
/// only our own fields are read this way).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let rest = &line[line.find(&pat)? + pat.len()..];
    if let Some(r) = rest.strip_prefix('"') {
        return Some(&r[..r.find('"')?]);
    }
    Some(&rest[..rest.find([',', '}']).unwrap_or(rest.len())])
}

impl Journal {
    pub(crate) fn encode(&self) -> String {
        let mut s = format!(
            "{{\"format\":\"{FORMAT}\",\"version\":{VERSION},\"serial\":\"{:08x}\",\"mode\":\"{}\",\"device\":{},\
             \"moves\":[\n",
            self.serial,
            self.mode,
            crate::events::json_str(&self.device)
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

    pub(crate) fn decode(text: &str) -> Result<Journal> {
        let cut = text.rfind("\"check\":\"").context("the sync journal is truncated (no check field)")?;
        let (body, tail) = text.split_at(cut);
        let want = field(tail, "check").and_then(|c| u64::from_str_radix(c, 16).ok());
        ensure!(want == Some(xxhash_rust::xxh3::xxh3_64(body.as_bytes())), "the sync journal is damaged (check)");
        let head = body.lines().next().unwrap_or("");
        ensure!(field(head, "format") == Some(FORMAT), "not a flashcp sync journal");
        ensure!(
            field(head, "version") == Some(&VERSION.to_string()),
            "unsupported sync journal version {:?}",
            field(head, "version")
        );
        let serial = field(head, "serial").and_then(|s| u32::from_str_radix(s, 16).ok()).context("journal serial")?;
        let mode = field(head, "mode").context("journal mode")?.to_string();
        let device = field(head, "device").unwrap_or("").to_string();
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
        Ok(Journal { serial, device, mode, moves: Vec::new(), writes })
    }
}

/// `$XDG_STATE_HOME/flashcp/sync-journal` (or `$HOME/.local/state/flashcp/sync-journal`).
pub(crate) fn dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").context("neither XDG_STATE_HOME nor HOME is set")?)
            .join(".local/state"),
    };
    Ok(base.join("flashcp").join("sync-journal"))
}

pub(crate) fn path_for(serial: u32) -> Result<PathBuf> {
    Ok(dir()?.join(format!("{serial:08x}.json")))
}

fn fsync_dir(d: &Path) -> Result<()> {
    File::open(d).and_then(|f| f.sync_all()).with_context(|| format!("fsync {}", d.display()))
}

/// Writes the journal durably: a temp file, fsynced, renamed into place, the directory fsynced.
pub(crate) fn save(j: &Journal) -> Result<PathBuf> {
    let d = dir()?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&d).with_context(|| format!("create {}", d.display()))?;
    let p = path_for(j.serial)?;
    let tmp = d.join(format!("{:08x}.json.tmp", j.serial));
    let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(j.encode().as_bytes())
        .and_then(|_| f.sync_all())
        .with_context(|| format!("write {}", tmp.display()))?;
    drop(f);
    fs::rename(&tmp, &p).with_context(|| format!("rename {} into place", tmp.display()))?;
    fsync_dir(&d)?;
    Ok(p)
}

pub(crate) fn remove(serial: u32) -> Result<()> {
    let p = path_for(serial)?;
    match fs::remove_file(&p) {
        Ok(()) => fsync_dir(p.parent().unwrap()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", p.display())),
    }
}

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
                dev.read_exact_at(&mut buf, s0 * SECTOR).with_context(|| format!("read metadata at {off}"))?;
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
        dev.write_all_at(&w.post, w.off).with_context(|| format!("write metadata at {}", w.off))?;
    }
    dev.sync_all().context("sync")
}

/// Per touched sector: the bytes some write covers (mask) and every state they go through.
struct SectorHistory {
    mask: Vec<bool>,
    states: Vec<Vec<u8>>,
}

/// Checks that each sector `writes` touch holds one of its journaled states. Err names the
/// first sector that does not.
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
    for (&s, h) in &hist {
        let mut cur = vec![0u8; SECTOR as usize];
        dev.read_exact_at(&mut cur, s * SECTOR).with_context(|| format!("read sector {s}"))?;
        let same = |st: &Vec<u8>| (0..SECTOR as usize).all(|k| !h.mask[k] || st[k] == cur[k]);
        if !h.states.iter().any(same) {
            bail!(
                "the volume changed since the interrupted sync (device sector {s} holds none of the journaled \
                 states); nothing was written. Run fsck.exfat -y, then re-run the sync"
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
    /// The volume was clean: the journal was stale and is removed.
    Stale,
    /// The interrupted run's writes were replayed (or, for an undo journal, the dirty flag
    /// taken back): `writes` metadata writes.
    Replayed { writes: usize, undo: bool },
}

/// Replays the journal of an interrupted sync of the volume on `dev` (see the module doc).
pub(crate) fn recover(dev: &File) -> Result<Recovery> {
    let mut s0 = vec![0u8; 512];
    if dev.read_exact_at(&mut s0, 0).is_err() || &s0[3..11] != b"EXFAT   " {
        return Ok(Recovery::None);
    }
    let serial = u32::from_le_bytes(s0[100..104].try_into().unwrap());
    let dirty = u16::from_le_bytes([s0[106], s0[107]]) & crate::volume::FLAG_DIRTY != 0;
    // No state directory at all: there can be no journal (and a sync would refuse to start).
    let Ok(p) = path_for(serial) else { return Ok(Recovery::None) };
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Recovery::None),
        Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
    };
    if !dirty {
        remove(serial)?;
        return Ok(Recovery::Stale);
    }
    let j = Journal::decode(&text).with_context(|| format!("sync journal {}", p.display()))?;
    ensure!(j.serial == serial, "sync journal {} is for volume {:08x}", p.display(), j.serial);
    check(dev, &j.writes)?;
    for (_, ws) in phases(&j.writes) {
        apply(dev, ws)?;
    }
    remove(serial)?;
    Ok(Recovery::Replayed { writes: j.writes.len(), undo: j.mode == "undo" })
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn encode_decode_round_trip_and_damage_is_refused() {
        let j = Journal {
            serial: 0xDEAD_BEEF,
            device: "a \"dev\"".into(),
            mode: "redo".into(),
            moves: vec![MoveRec {
                from: "/a".into(),
                to: "/b".into(),
                first: 5,
                len: 9,
                valid_len: 9,
                no_fat_chain: true,
                mtime: (1, 2, 0x80),
            }],
            writes: vec![
                JWrite { phase: "alloc".into(), off: 4096, pre: vec![0, 1], post: vec![2, 3] },
                JWrite { phase: "free".into(), off: 7, pre: vec![0xff], post: vec![0] },
            ],
        };
        let text = j.encode();
        let back = Journal::decode(&text).unwrap();
        assert_eq!((back.serial, back.mode.as_str(), &back.writes), (j.serial, "redo", &j.writes));
        assert!(Journal::decode(&text.replace("4096", "4097")).is_err());
        assert!(Journal::decode(&text[..text.len() / 2]).is_err());
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
        fs::remove_file(p).unwrap();
        fs::remove_file(p2).unwrap();
    }
}
