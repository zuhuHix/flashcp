//! Workload driver for the simulator: parses `scenarios.txt` (the real measurements and their
//! rankings) and records the device-op trace of each scenario by running flashcp on sparse image
//! files ([`Workbench::record`]). Recordings are cached under the workbench root, so a second
//! run of the same scenario returns at once. Nothing here touches a physical drive: the
//! "drive" is an image file, and the drive's strategy comes from `FLASHCP_SIM_DEVICE`.
//!
//! Layout of the workbench root:
//! - `data/<name>`: generated datasets (see [`Workbench::dataset`])
//! - `devices/<drive>.dev`: the device file handed to flashcp as `FLASHCP_SIM_DEVICE`
//! - `images/<drive>--<layout>.img` (+ `.stamp`): base images, built once per key
//! - `work/`: scratch copies of base images and pull destinations, removed after each run
//! - `traces/<id>-<hash>.jsonl`: the recordings

use crate::sim::device::SimDevice;
use crate::util::Rng;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, UNIX_EPOCH};

const SCENARIOS_TXT: &str = include_str!("scenarios.txt");
const DATASETS: &[&str] = &["small", "smallB", "many", "mixed", "file512m"];
const DRIVES: &[&str] = &["sandisk", "msi", "kingston"];
const OPS: &[&str] = &["build", "add", "pull", "micro"];
const MICRO_KINDS: &[&str] = &["seqw", "randw", "flush", "seqr"];

/// One measured scenario (a line of `scenarios.txt`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scenario {
    pub id: String,
    pub drive: String,
    pub op: String,
    pub data: String,
    pub layout: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub real: Option<f64>,
    pub lo: Option<f64>,
    pub hi: Option<f64>,
    pub src: String,
    pub kind: Option<String>,
    pub bs: Option<u64>,
    pub qd: Option<usize>,
    pub bytes: Option<u64>,
}

/// A validated ranking: `faster` was measured faster than `slower` on the drive.
#[derive(Clone, Debug, PartialEq)]
pub struct Pair {
    pub faster: String,
    pub slower: String,
}

/// Parses `scenarios.txt` text. Strict: unknown keys, ops, drives, datasets and layouts,
/// duplicate ids and pairs naming unknown ids are errors (with the line number).
pub fn parse_scenarios(text: &str) -> Result<(Vec<Scenario>, Vec<Pair>)> {
    let mut scs: Vec<(usize, Scenario)> = Vec::new();
    let mut pairs: Vec<(usize, Pair)> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let ln = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fail = |msg: String| anyhow!("scenarios.txt line {ln}: {msg}");
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks[0] == "pair" {
            let (mut faster, mut slower) = (None, None);
            for t in &toks[1..] {
                match t.split_once('=') {
                    Some(("faster", v)) if faster.is_none() => faster = Some(v.to_string()),
                    Some(("slower", v)) if slower.is_none() => slower = Some(v.to_string()),
                    _ => return Err(fail(format!("pair: expected faster=<id> slower=<id>, got `{t}`"))),
                }
            }
            match (faster, slower) {
                (Some(faster), Some(slower)) => pairs.push((ln, Pair { faster, slower })),
                _ => return Err(fail("pair needs both faster=<id> and slower=<id>".into())),
            }
            continue;
        }
        let mut sc = Scenario::default();
        let mut seen = HashSet::new();
        for t in toks {
            let (k, v) = t.split_once('=').ok_or_else(|| fail(format!("expected key=value, got `{t}`")))?;
            if !seen.insert(k) {
                return Err(fail(format!("duplicate key `{k}`")));
            }
            match k {
                "id" => sc.id = v.to_string(),
                "drive" => sc.drive = v.to_string(),
                "op" => sc.op = v.to_string(),
                "data" => sc.data = v.to_string(),
                "layout" => sc.layout = v.to_string(),
                "src" => sc.src = v.to_string(),
                "kind" => sc.kind = Some(v.to_string()),
                "args" => sc.args = split_list(v).map_err(|e| fail(format!("args: {e}")))?,
                "env" => sc.env = split_env(v).map_err(|e| fail(format!("env: {e}")))?,
                "real" => sc.real = Some(number(k, v).map_err(|e| fail(format!("{e:#}")))?),
                "lo" => sc.lo = Some(number(k, v).map_err(|e| fail(format!("{e:#}")))?),
                "hi" => sc.hi = Some(number(k, v).map_err(|e| fail(format!("{e:#}")))?),
                "bs" => sc.bs = Some(number(k, v).map_err(|e| fail(format!("{e:#}")))?),
                "qd" => sc.qd = Some(number(k, v).map_err(|e| fail(format!("{e:#}")))?),
                "bytes" => sc.bytes = Some(number(k, v).map_err(|e| fail(format!("{e:#}")))?),
                _ => return Err(fail(format!("unknown key `{k}`"))),
            }
        }
        scs.push((ln, sc));
    }

    let mut ids: HashMap<&str, usize> = HashMap::new();
    for (ln, sc) in &scs {
        check(sc).map_err(|e| anyhow!("scenarios.txt line {ln}: {e:#}"))?;
        if ids.insert(&sc.id, *ln).is_some() {
            bail!("scenarios.txt line {ln}: duplicate id `{}`", sc.id);
        }
    }
    for (ln, p) in &pairs {
        for id in [&p.faster, &p.slower] {
            if !ids.contains_key(id.as_str()) {
                bail!("scenarios.txt line {ln}: pair names unknown id `{id}`");
            }
        }
    }
    Ok((scs.into_iter().map(|(_, s)| s).collect(), pairs.into_iter().map(|(_, p)| p).collect()))
}

/// The scenarios and rankings shipped in `scenarios.txt`. Panics if that file is invalid
/// (a test covers it).
pub fn builtin_scenarios() -> (Vec<Scenario>, Vec<Pair>) {
    parse_scenarios(SCENARIOS_TXT).expect("src/sim/scenarios.txt is invalid")
}

/// The device file (a devprofile with `sim.*` keys) for a drive name.
pub fn device_file_text(drive: &str) -> Option<&'static str> {
    match drive {
        "sandisk" => Some(include_str!("devices/sandisk.dev")),
        "msi" => Some(include_str!("devices/msi.dev")),
        "kingston" => Some(include_str!("devices/kingston.dev")),
        _ => None,
    }
}

/// The built-in profile name of a drive (the `sim.name` of its device file).
pub fn drive_model_name(drive: &str) -> &str {
    match drive {
        "sandisk" => "sandisk-cruzer-usb2",
        "msi" => "msi-datamag-uas",
        "kingston" => "kingston-dt3-usb3",
        other => other,
    }
}

/// Where recordings, datasets and images live, and which flashcp binary runs.
pub struct Workbench {
    pub root: PathBuf,
    pub exe: PathBuf,
    /// Print each command and its duration to stderr.
    pub verbose: bool,
    locks: KeyLocks,
}

impl Workbench {
    /// Opens (and creates) the workbench at `root`, default `$XDG_CACHE_HOME/flashcp/sim`
    /// (else `$HOME/.cache/flashcp/sim`). `exe` is the running binary.
    pub fn new(root: Option<PathBuf>) -> Result<Workbench> {
        let root = match root {
            Some(r) => r,
            None => default_root()?,
        };
        for d in ["data", "devices", "images", "work", "traces"] {
            let p = root.join(d);
            fs::create_dir_all(&p).with_context(|| format!("create {}", p.display()))?;
        }
        let exe = std::env::current_exe().context("cannot find the current executable (flashcp)")?;
        Ok(Workbench { root, exe, verbose: false, locks: KeyLocks::default() })
    }

    /// The dataset `name`, generated once into `root/data/<name>` (via `flashcp gen`, or in
    /// process for `file512m`). Generated into a temporary directory and renamed, so an
    /// interrupted run never leaves a half dataset.
    pub fn dataset(&self, name: &str) -> Result<PathBuf> {
        if !DATASETS.contains(&name) {
            bail!("unknown dataset `{name}` (small | smallB | many | mixed | file512m)");
        }
        let dst = self.root.join("data").join(name);
        let lock = self.locks.get(&format!("data/{name}"));
        let _g = lock_guard(&lock);
        if dst.exists() {
            return Ok(dst);
        }
        let tmp = self.root.join("data").join(format!("{name}.tmp-{}-{}", std::process::id(), next_n()));
        let _ = fs::remove_dir_all(&tmp);
        let made = match name {
            "file512m" => write_big_file(&tmp),
            "small" => self.run_gen(&tmp, "small", 42),
            "smallB" => self.run_gen(&tmp, "small", 7),
            "many" => self.run_gen(&tmp, "many", 42),
            _ => self.run_gen(&tmp, "mixed", 42),
        };
        if let Err(e) = made {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e.context(format!("generate dataset {name}")));
        }
        if let Err(e) = fs::rename(&tmp, &dst) {
            let _ = fs::remove_dir_all(&tmp);
            if !dst.exists() {
                return Err(e).with_context(|| format!("rename dataset into {}", dst.display()));
            }
        }
        Ok(dst)
    }

    /// Records the device-op trace (JSONL, see `sim::trace`) of a non-micro scenario, cached
    /// under `root/traces`. Micro scenarios are synthesized by another module and are refused.
    pub fn record(&self, sc: &Scenario) -> Result<PathBuf> {
        check(sc)?;
        if sc.op == "micro" {
            bail!("{}: micro scenarios are synthesized by the simulator, not recorded", sc.id);
        }
        let dev_text = device_file_text(&sc.drive).ok_or_else(|| anyhow!("{}: unknown drive `{}`", sc.id, sc.drive))?;
        let dev = SimDevice::parse(dev_text).map_err(|e| anyhow!("{}: device file of `{}`: {e}", sc.id, sc.drive))?;
        let size =
            dev.size_bytes.ok_or_else(|| anyhow!("{}: device file of `{}` has no sim.size_bytes", sc.id, sc.drive))?;
        let exe_stamp = self.exe_stamp()?;
        let hash = trace_hash(sc, dev_text, &exe_stamp);
        let final_path = self.root.join("traces").join(format!("{}-{hash}.jsonl", sc.id));
        let lock = self.locks.get(&format!("trace/{hash}/{}", sc.id));
        let _g = lock_guard(&lock);
        if final_path.is_file() {
            return Ok(final_path);
        }

        let dev_path = self.write_device_file(&sc.drive, dev_text)?;
        let base = self.ensure_base(&sc.layout, &sc.drive, dev_text, &dev_path, size, &exe_stamp)?;
        let (n, pid) = (next_n(), std::process::id());
        let tmp = self.root.join("traces").join(format!("{}-{hash}.jsonl.tmp-{pid}-{n}", sc.id));
        // Removes the temporary trace, work image and pull destination on every path out.
        let mut scratch = Scratch(vec![tmp.clone()]);
        let (dev_s, tmp_s) = (p(&dev_path), p(&tmp));
        let mut envs = env(&[("FLASHCP_SIM_DEVICE", dev_s.as_str()), ("FLASHCP_TRACE", tmp_s.as_str())]);
        envs.extend(sc.env.iter().cloned());
        let mut args: Vec<String> = match sc.op.as_str() {
            "build" => {
                let ds = self.dataset(&sc.data)?;
                let work = self.work_image(sc, &base, &mut scratch, n, pid)?;
                vec!["build".into(), p(&ds), p(&work), "--yes-erase".into()]
            }
            "add" => {
                let ds = self.dataset(&sc.data)?;
                let work = self.work_image(sc, &base, &mut scratch, n, pid)?;
                vec!["add".into(), p(&ds), p(&work)]
            }
            _ => {
                let dest = self.root.join("work").join(format!("pull-{}-{pid}-{n}", sc.id));
                let _ = fs::remove_dir_all(&dest);
                scratch.0.push(dest.clone());
                vec!["pull".into(), p(&base), p(&dest)]
            }
        };
        args.extend(sc.args.iter().cloned());

        let desc = describe(&envs, &self.exe, &args);
        if self.verbose {
            eprintln!("[sim] {}: {desc}", sc.id);
        }
        let t0 = Instant::now();
        run_checked(self.exe_cmd(&envs, &args), &desc)?;
        if !tmp.is_file() {
            bail!("{}: `{desc}` finished but wrote no trace to {}", sc.id, tmp.display());
        }
        fs::rename(&tmp, &final_path).with_context(|| format!("rename trace into {}", final_path.display()))?;
        if self.verbose {
            eprintln!("[sim] {}: recorded in {:.1} s", sc.id, t0.elapsed().as_secs_f64());
        }
        Ok(final_path)
    }

    /// Records every scenario with up to `jobs` in parallel. Results are in input order.
    pub fn record_all(&self, scs: &[Scenario], jobs: usize) -> Vec<Result<PathBuf>> {
        let slots: Vec<Mutex<Option<Result<PathBuf>>>> = scs.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let threads = jobs.clamp(1, scs.len().max(1));
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= scs.len() {
                            break;
                        }
                        let r = self.record(&scs[i]);
                        *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                    }
                });
            }
        });
        slots
            .into_iter()
            .map(|m| {
                m.into_inner()
                    .unwrap_or_else(|e| e.into_inner())
                    .unwrap_or_else(|| Err(anyhow!("internal error: scenario was not run")))
            })
            .collect()
    }

    fn run_gen(&self, dir: &Path, profile: &str, seed: u64) -> Result<()> {
        let args: Vec<String> =
            vec!["gen".into(), p(dir), "--profile".into(), profile.into(), "--seed".into(), seed.to_string()];
        let desc = describe(&[], &self.exe, &args);
        run_checked(self.exe_cmd(&[], &args), &desc)
    }

    /// Writes the device file of `drive` to `root/devices` (only when its content changed).
    fn write_device_file(&self, drive: &str, text: &str) -> Result<PathBuf> {
        let path = self.root.join("devices").join(format!("{drive}.dev"));
        if fs::read_to_string(&path).ok().as_deref() != Some(text) {
            let tmp = self.root.join("devices").join(format!("{drive}.dev.tmp-{}-{}", std::process::id(), next_n()));
            fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
            fs::rename(&tmp, &path).with_context(|| format!("install {}", path.display()))?;
        }
        Ok(path)
    }

    /// Identifies the flashcp binary (size and mtime), so a rebuilt flashcp invalidates caches.
    fn exe_stamp(&self) -> Result<String> {
        let m = fs::metadata(&self.exe).with_context(|| format!("stat {}", self.exe.display()))?;
        let mtime = m.modified()?.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        Ok(format!("{}:{mtime}", m.len()))
    }

    /// The base image of (drive, layout): built once (under a per-image lock) and reused until
    /// its key (layout, drive, flashcp, device file) changes.
    fn ensure_base(
        &self,
        layout: &str,
        drive: &str,
        dev_text: &str,
        dev_path: &Path,
        size: u64,
        exe_stamp: &str,
    ) -> Result<PathBuf> {
        let lay = parse_layout(layout)?;
        let name = format!("{drive}--{}", sanitize(layout));
        let img = self.root.join("images").join(format!("{name}.img"));
        let stamp = self.root.join("images").join(format!("{name}.stamp"));
        let key = format!("{layout}|{drive}|{exe_stamp}|{}", hash_parts(&[dev_text]));
        let lock = self.locks.get(&format!("img/{name}"));
        let _g = lock_guard(&lock);
        if img.is_file() && fs::read_to_string(&stamp).ok().as_deref() == Some(key.as_str()) {
            return Ok(img);
        }
        let _ = fs::remove_file(&stamp);
        let _ = fs::remove_file(&img);
        let tmp = self.root.join("images").join(format!("{name}.img.tmp"));
        let _ = fs::remove_file(&tmp);
        let f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.set_len(size).with_context(|| format!("size {} to {size} bytes", tmp.display()))?;
        drop(f);
        if let Err(e) = self.build_layout(&lay, &tmp, dev_path) {
            let _ = fs::remove_file(&tmp);
            return Err(e.context(format!("base image {name} (layout {layout})")));
        }
        fs::rename(&tmp, &img).with_context(|| format!("install {}", img.display()))?;
        fs::write(&stamp, &key).with_context(|| format!("write {}", stamp.display()))?;
        Ok(img)
    }

    fn build_layout(&self, lay: &Layout, path: &Path, dev_path: &Path) -> Result<()> {
        match lay {
            Layout::Blank => Ok(()),
            Layout::Mkfs { cluster } => mkfs(cluster, path),
            Layout::Flashcp { data, cluster } => {
                let ds = self.dataset(data)?;
                let mut args: Vec<String> = vec!["build".into(), p(&ds), p(path), "--yes-erase".into()];
                if let Some(c) = cluster {
                    args.extend(["--cluster".into(), c.to_string()]);
                }
                let dev_s = p(dev_path);
                self.run_flashcp(&env(&[("FLASHCP_SIM_DEVICE", dev_s.as_str())]), &args)
            }
            Layout::Kernel { cluster, data } => {
                mkfs(cluster, path)?;
                let ds = self.dataset(data)?;
                let args: Vec<String> = vec!["add".into(), p(&ds), p(path)];
                let envs = env(&[("FLASHCP_TEST_NO_ADD_BUILD", "1"), ("FLASHCP_TEST_GAP_POLICY", "skip")]);
                self.run_flashcp(&envs, &args)
            }
        }
    }

    fn work_image(&self, sc: &Scenario, base: &Path, scratch: &mut Scratch, n: u64, pid: u32) -> Result<PathBuf> {
        let work = self.root.join("work").join(format!("{}-{pid}-{n}.img", sc.id));
        scratch.0.push(work.clone());
        let mut cmd = Command::new("cp");
        cmd.arg("--sparse=always").arg(base).arg(&work);
        run_checked(cmd, &format!("cp --sparse=always {} {}", base.display(), work.display()))?;
        Ok(work)
    }

    fn run_flashcp(&self, envs: &[(String, String)], args: &[String]) -> Result<()> {
        let desc = describe(envs, &self.exe, args);
        run_checked(self.exe_cmd(envs, args), &desc)
    }

    /// The flashcp command with every inherited `FLASHCP_*` variable removed (so a stray
    /// `FLASHCP_TRACE` or test switch in the caller's shell cannot change a recording), then
    /// `envs` set.
    fn exe_cmd(&self, envs: &[(String, String)], args: &[String]) -> Command {
        let mut c = Command::new(&self.exe);
        clear_flashcp_env(&mut c);
        c.envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        c.args(args);
        c
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Layout {
    Blank,
    Mkfs { cluster: String },
    Flashcp { data: String, cluster: Option<u32> },
    Kernel { cluster: String, data: String },
}

fn parse_layout(s: &str) -> Result<Layout> {
    if s == "blank" {
        return Ok(Layout::Blank);
    }
    if let Some(c) = s.strip_prefix("mkfs") {
        return Ok(Layout::Mkfs { cluster: kcluster(c)? });
    }
    if let Some(rest) = s.strip_prefix("flashcp:") {
        let mut it = rest.split(':');
        let data = check_data(it.next().unwrap_or(""))?;
        let cluster = match it.next() {
            Some(c) => Some(c.parse::<u32>().map_err(|_| anyhow!("layout `{s}`: cluster `{c}` is not a number"))?),
            None => None,
        };
        if it.next().is_some() {
            bail!("layout `{s}`: too many fields");
        }
        return Ok(Layout::Flashcp { data, cluster });
    }
    if let Some(rest) = s.strip_prefix("kernel") {
        let (c, d) = rest.split_once(':').ok_or_else(|| anyhow!("layout `{s}`: expected kernel<C>:<D>"))?;
        return Ok(Layout::Kernel { cluster: kcluster(c)?, data: check_data(d)? });
    }
    bail!("layout `{s}`: expected blank, mkfs<C>, flashcp:<D>[:<C>] or kernel<C>:<D>")
}

/// A cluster size as mkfs.exfat wants it: `32k` or `32K` becomes `32K`.
fn kcluster(c: &str) -> Result<String> {
    let digits = c.strip_suffix(['k', 'K']).ok_or_else(|| anyhow!("cluster `{c}`: expected a size like 32k"))?;
    let n: u32 = digits.parse().map_err(|_| anyhow!("cluster `{c}`: expected a size like 32k"))?;
    if n == 0 {
        bail!("cluster `{c}` must be positive");
    }
    Ok(format!("{n}K"))
}

fn check_data(d: &str) -> Result<String> {
    if DATASETS.contains(&d) {
        Ok(d.to_string())
    } else {
        bail!("unknown dataset `{d}` (small | smallB | many | mixed | file512m)")
    }
}

/// Validates one scenario (the parser calls it; `record` calls it for hand-built ones).
fn check(sc: &Scenario) -> Result<()> {
    if sc.id.is_empty() || sc.id.contains(char::is_whitespace) {
        bail!("scenario id `{}` is empty or contains spaces", sc.id);
    }
    let id = &sc.id;
    if !DRIVES.contains(&sc.drive.as_str()) {
        bail!("{id}: unknown drive `{}` (sandisk | msi)", sc.drive);
    }
    if !OPS.contains(&sc.op.as_str()) {
        bail!("{id}: unknown op `{}` (build | add | pull | micro)", sc.op);
    }
    if sc.op == "micro" {
        match sc.kind.as_deref() {
            Some(k) if MICRO_KINDS.contains(&k) => {}
            _ => bail!("{id}: micro needs kind= (seqw | randw | flush | seqr)"),
        }
        if sc.bs.is_none_or(|b| b == 0) || sc.qd.is_none_or(|q| q == 0) || sc.bytes.is_none() {
            bail!("{id}: micro needs bs=, qd= (>= 1) and bytes=");
        }
    } else {
        if sc.kind.is_some() || sc.bs.is_some() || sc.qd.is_some() || sc.bytes.is_some() {
            bail!("{id}: only micro scenarios take kind/bs/qd/bytes");
        }
        check_data(&sc.data).with_context(|| id.clone())?;
        parse_layout(&sc.layout).with_context(|| id.clone())?;
    }
    for (k, _) in &sc.env {
        if k.is_empty() {
            bail!("{id}: empty environment variable name");
        }
    }
    Ok(())
}

/// Comma-separated list (`args=--cluster,512`); empty items are errors.
fn split_list(v: &str) -> Result<Vec<String>> {
    v.split(',').map(|x| if x.is_empty() { Err(anyhow!("empty item in `{v}`")) } else { Ok(x.to_string()) }).collect()
}

fn split_env(v: &str) -> Result<Vec<(String, String)>> {
    split_list(v)?
        .into_iter()
        .map(|item| {
            item.split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow!("`{item}` is not NAME=VALUE"))
        })
        .collect()
}

fn join_list(items: &[String]) -> String {
    items.join(",")
}

fn join_env(env: &[(String, String)]) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}

fn number<T: FromStr>(k: &str, v: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    v.parse::<T>().map_err(|e| anyhow!("key `{k}`: `{v}` is not a number ({e})"))
}

/// The hash that names a recording: every field that changes the run, the device file and
/// the flashcp binary.
fn trace_hash(sc: &Scenario, dev_text: &str, exe_stamp: &str) -> String {
    let (args, env) = (join_list(&sc.args), join_env(&sc.env));
    hash_parts(&[
        sc.op.as_str(),
        sc.data.as_str(),
        sc.layout.as_str(),
        args.as_str(),
        env.as_str(),
        sc.drive.as_str(),
        dev_text,
        exe_stamp,
    ])
}

/// FNV-1a 64 over the parts (each followed by a NUL), as 16 hex digits.
fn hash_parts(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in part.as_bytes().iter().chain(std::iter::once(&0u8)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

fn default_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => {
            let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("neither XDG_CACHE_HOME nor HOME is set"))?;
            PathBuf::from(home).join(".cache")
        }
    };
    Ok(base.join("flashcp").join("sim"))
}

fn write_big_file(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("big.bin");
    let mut f = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let mut rng = Rng(42);
    let mut buf = vec![0u8; 8 << 20];
    let mut left: u64 = 512 << 20;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        rng.fill(&mut buf[..n]);
        f.write_all(&buf[..n]).with_context(|| format!("write {}", path.display()))?;
        left -= n as u64;
    }
    Ok(())
}

fn mkfs(cluster: &str, path: &Path) -> Result<()> {
    let mut cmd = Command::new("mkfs.exfat");
    cmd.arg("-c").arg(cluster).arg(path);
    run_checked(cmd, &format!("mkfs.exfat -c {cluster} {}", path.display()))
}

/// Runs `cmd`; a non-zero exit is an error naming the command line and the tail of stderr.
fn run_checked(mut cmd: Command, desc: &str) -> Result<()> {
    let prog = cmd.get_program().to_string_lossy().into_owned();
    let out: Output = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && prog == "mkfs.exfat" => {
            bail!(
                "mkfs.exfat not found in PATH: install the exfatprogs package (needed by layouts mkfs<C> and kernel<C>)"
            )
        }
        Err(e) => bail!("cannot run `{desc}`: {e}"),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = err.lines().collect();
        let tail = lines[lines.len().saturating_sub(30)..].join("\n");
        bail!("`{desc}` failed ({}):\n{tail}", out.status);
    }
    Ok(())
}

fn describe(envs: &[(String, String)], prog: &Path, args: &[String]) -> String {
    let mut s = String::new();
    for (k, v) in envs {
        s.push_str(&format!("{k}={v} "));
    }
    s.push_str(&prog.display().to_string());
    for a in args {
        s.push(' ');
        s.push_str(a);
    }
    s
}

fn clear_flashcp_env(c: &mut Command) {
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("FLASHCP_") {
            c.env_remove(k);
        }
    }
}

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn p(path: &Path) -> String {
    path.display().to_string()
}

/// Removes its paths (files or directories) when dropped, on success and on error.
#[derive(Default)]
struct Scratch(Vec<PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.0 {
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// One mutex per key (image, dataset, trace), so shared work runs once across threads.
#[derive(Default)]
struct KeyLocks(Mutex<HashMap<String, Arc<Mutex<()>>>>);

impl KeyLocks {
    fn get(&self, key: &str) -> Arc<Mutex<()>> {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(key.to_string()).or_default().clone()
    }
}

fn lock_guard(l: &Arc<Mutex<()>>) -> MutexGuard<'_, ()> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

fn next_n() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Result<(Vec<Scenario>, Vec<Pair>)> {
        parse_scenarios(line)
    }

    #[test]
    fn builtin_file_parses_and_is_consistent() {
        let (scs, pairs) = builtin_scenarios();
        assert!(scs.len() > 40, "{} scenarios", scs.len());
        assert_eq!(pairs.len(), 16);
        let pull = scs.iter().find(|s| s.id == "sd-pull-flashcp").unwrap();
        assert_eq!((pull.drive.as_str(), pull.op.as_str(), pull.layout.as_str()), ("sandisk", "pull", "flashcp:small"));
        assert_eq!(pull.real, Some(16.0));
        let skip1 = scs.iter().find(|s| s.id == "sd-add-32k-skip1").unwrap();
        assert_eq!(skip1.env, vec![("FLASHCP_TEST_GAP_POLICY".to_string(), "skip:1".to_string())]);
        let micro = scs.iter().find(|s| s.id == "sd-micro-seqw4k").unwrap();
        assert_eq!((micro.kind.as_deref(), micro.bs, micro.qd), (Some("seqw"), Some(4096), Some(1)));
    }

    #[test]
    fn every_builtin_layout_and_device_is_known() {
        let (scs, _) = builtin_scenarios();
        for s in scs.iter().filter(|s| s.op != "micro") {
            assert!(device_file_text(&s.drive).is_some(), "{}", s.id);
            parse_layout(&s.layout).unwrap_or_else(|e| panic!("{}: {e:#}", s.id));
        }
        for d in ["sandisk", "msi", "kingston"] {
            let dev = SimDevice::parse(device_file_text(d).unwrap()).unwrap();
            assert_eq!(dev.name, drive_model_name(d));
            assert!(dev.size_bytes.is_some());
        }
    }

    #[test]
    fn rejects_unknown_key() {
        let e = one("id=a drive=sandisk op=build data=small layout=blank foo=1").unwrap_err();
        assert!(format!("{e:#}").contains("unknown key `foo`"), "{e:#}");
    }

    #[test]
    fn rejects_duplicate_id() {
        let t = "id=a drive=sandisk op=build data=small layout=blank\nid=a drive=msi op=build data=small layout=blank";
        let e = one(t).unwrap_err();
        assert!(format!("{e:#}").contains("duplicate id `a`"), "{e:#}");
    }

    #[test]
    fn rejects_dangling_pair() {
        let t = "id=a drive=sandisk op=build data=small layout=blank\npair faster=a slower=nope";
        let e = one(t).unwrap_err();
        assert!(format!("{e:#}").contains("unknown id `nope`"), "{e:#}");
    }

    #[test]
    fn rejects_unknown_op_drive_and_dataset() {
        for (t, what) in [
            ("id=a drive=sandisk op=copy data=small layout=blank", "unknown op `copy`"),
            ("id=a drive=usb9 op=build data=small layout=blank", "unknown drive `usb9`"),
            ("id=a drive=sandisk op=build data=huge layout=blank", "unknown dataset `huge`"),
            ("id=a drive=sandisk op=build data=small layout=mkfs32", "expected a size like 32k"),
        ] {
            let e = one(t).unwrap_err();
            assert!(format!("{e:#}").contains(what), "{what}: {e:#}");
        }
    }

    #[test]
    fn args_and_env_round_trip() {
        let (scs, _) = one(
            "id=a drive=sandisk op=add data=smallB layout=kernel32k:small args=--cluster,512,--to,x env=FLASHCP_A=1,FLASHCP_B=x=y",
        )
        .unwrap();
        let s = &scs[0];
        assert_eq!(s.args, ["--cluster", "512", "--to", "x"]);
        assert_eq!(s.env, [("FLASHCP_A".into(), "1".into()), ("FLASHCP_B".into(), "x=y".into())]);
        assert_eq!(join_list(&split_list(&join_list(&s.args)).unwrap()), "--cluster,512,--to,x");
        assert_eq!(split_env(&join_env(&s.env)).unwrap(), s.env);
        assert!(split_list("a,,b").is_err());
        assert!(split_env("NOEQUALS").is_err());
    }

    #[test]
    fn hash_changes_with_args_env_and_device() {
        let (scs, _) = builtin_scenarios();
        let base = scs.iter().find(|s| s.id == "sd-add-32k-skip1").unwrap().clone();
        let h0 = trace_hash(&base, "dev", "exe");
        let mut a = base.clone();
        a.args.push("--chunk-kb".into());
        a.args.push("8".into());
        assert_ne!(trace_hash(&a, "dev", "exe"), h0);
        let mut e = base.clone();
        e.env[0].1 = "skip:8".into();
        assert_ne!(trace_hash(&e, "dev", "exe"), h0);
        assert_ne!(trace_hash(&base, "dev2", "exe"), h0);
        assert_ne!(trace_hash(&base, "dev", "exe2"), h0);
        assert_eq!(trace_hash(&base, "dev", "exe"), h0);
        // The id is not part of the run, only of the file name.
        let mut renamed = base.clone();
        renamed.id = "other".into();
        assert_eq!(trace_hash(&renamed, "dev", "exe"), h0);
    }

    #[test]
    fn micro_is_not_recorded() {
        let wb = Workbench {
            root: std::env::temp_dir(),
            exe: PathBuf::from("/nonexistent"),
            verbose: false,
            locks: KeyLocks::default(),
        };
        let (scs, _) = builtin_scenarios();
        let m = scs.iter().find(|s| s.op == "micro").unwrap();
        let e = wb.record(m).unwrap_err();
        assert!(format!("{e:#}").contains("micro scenarios are synthesized"), "{e:#}");
    }

    fn test_exe() -> Option<PathBuf> {
        let prof = if cfg!(debug_assertions) { "debug" } else { "release" };
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(prof).join("flashcp");
        p.is_file().then_some(p)
    }

    /// Records sd-pull-flashcp on a temp workbench: the first line is the header naming the
    /// simulated device, reads are present, and a second call hits the cache.
    #[test]
    fn records_sd_pull_flashcp() {
        let Some(exe) = test_exe() else {
            println!("SKIP records_sd_pull_flashcp: no flashcp binary under target/ (run cargo build)");
            return;
        };
        let root = std::env::temp_dir().join(format!("flashcp-workload-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut wb = Workbench::new(Some(root.clone())).unwrap();
        wb.exe = exe;
        let (scs, _) = builtin_scenarios();
        let sc = scs.iter().find(|s| s.id == "sd-pull-flashcp").unwrap();
        let path = wb.record(sc).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let first = text.lines().next().unwrap();
        assert!(first.starts_with(r#"{"v":2,"ev":"start""#), "{first}");
        assert!(first.contains(r#""device":"sandisk-cruzer-usb2""#), "{first}");
        assert!(text.lines().any(|l| l.contains(r#""ev":"io""#) && l.contains(r#""op":"r""#)));
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(wb.record(sc).unwrap(), path);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime, "second call must hit the cache");
        let _ = fs::remove_dir_all(&root);
    }
}
