//! `flashcp sim`: the command line of the trace-driven simulator (see SIMULATION.md).
//!
//! - `run` replays a trace on one or more device profiles;
//! - `profiles` lists the built-in profiles or prints one rendered model;
//! - `sweep` records a synthetic workload for every combination of setting values and ranks
//!   the combinations by simulated time;
//! - `calibrate` fits a model to the measured scenarios and cross-validates it.
//!
//! Output is deterministic: the same traces and models always print the same text.

use crate::sim::calib::{self, CalPoint};
use crate::sim::model::{self, Model, SimResult, Spread};
use crate::sim::profiles;
use crate::sim::replay::{Trace, TraceStats};
use crate::sim::workload::{self, Scenario, Workbench};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum SimCmd {
    /// Replay a recorded trace on one or more device profiles
    Run(RunArgs),
    /// List the built-in device profiles, or print one rendered model
    Profiles {
        /// Built-in profile to print (omit to list them)
        name: Option<String>,
    },
    /// Record a workload for every combination of setting values and rank them by simulated time
    Sweep(SweepArgs),
    /// Fit a device model to the measured scenarios and cross-validate it
    Calibrate(CalibrateArgs),
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Trace file (JSON lines, as written by FLASHCP_TRACE or --trace)
    pub trace: PathBuf,
    /// Device profile: a built-in name, a model file or a devprofile (repeat for several)
    #[arg(long, required = true, value_name = "PROFILE")]
    pub profile: Vec<String>,
    /// FTL seeds; with more than one, also print the mean and p10-p90 over the seeds
    #[arg(long, default_value_t = 1)]
    pub seeds: usize,
    /// Print JSON instead of a table
    #[arg(long)]
    pub json: bool,
    /// Replay a trace whose run did not finish cleanly (no end record, or it panicked)
    #[arg(long)]
    pub allow_truncated: bool,
}

#[derive(Args, Debug)]
pub struct SweepArgs {
    /// Device profile to simulate on
    #[arg(long)]
    pub profile: String,
    /// Workload dataset: small | smallB | many | mixed | file512m
    #[arg(long)]
    pub workload: String,
    /// Operation to record and simulate
    #[arg(long, value_parser = ["build", "add", "pull"])]
    pub op: String,
    /// Image layout before the op (default: blank, flashcp:<WORKLOAD>, or kernel32k/kernel128k:small)
    #[arg(long)]
    pub layout: Option<String>,
    /// Vary a setting over values, KEY=V1,V2,... (repeat for more keys; first key varies slowest)
    #[arg(long = "vary", value_name = "KEY=V1,V2")]
    pub vary: Vec<String>,
    /// FTL seeds per point; with more than one, also print the mean and p10-p90
    #[arg(long, default_value_t = 1)]
    pub seeds: usize,
    /// Workbench directory for recordings (default: $XDG_CACHE_HOME/flashcp/sim)
    #[arg(long, value_name = "DIR")]
    pub work: Option<PathBuf>,
    /// Parallel recordings (default: number of CPUs)
    #[arg(long)]
    pub jobs: Option<usize>,
    /// Drive to record on (default: the profile's drive; other profiles record as sandisk)
    #[arg(long, value_parser = ["sandisk", "msi", "kingston"])]
    pub record_as: Option<String>,
    /// Print JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CalibrateArgs {
    /// Drive to calibrate
    #[arg(long, default_value = "all", value_parser = ["sandisk", "msi", "kingston", "all"])]
    pub drive: String,
    /// Scenario file (default: the built-in scenarios.txt)
    #[arg(long, value_name = "FILE")]
    pub scenarios: Option<PathBuf>,
    /// Parameters to fit, comma-separated (default: every parameter the base model can use)
    #[arg(long, value_delimiter = ',', value_name = "K1,K2")]
    pub free: Vec<String>,
    /// Workbench directory for recordings (default: $XDG_CACHE_HOME/flashcp/sim)
    #[arg(long, value_name = "DIR")]
    pub work: Option<PathBuf>,
    /// Parallel recordings (default: number of CPUs)
    #[arg(long)]
    pub jobs: Option<usize>,
    /// Write all tables and summaries to this markdown file
    #[arg(long, value_name = "FILE.md")]
    pub out: Option<PathBuf>,
    /// Write each drive's fitted model into this directory
    #[arg(long, value_name = "DIR")]
    pub fitted_dir: Option<PathBuf>,
    /// What the fit starts from and is pulled toward: `probe` (default; parameters that were
    /// tuned on these same scenarios are reset to generic guesses, so held-out errors are
    /// honest) or `profile` (the built-in profile as shipped)
    #[arg(long, default_value = "probe", value_parser = ["probe", "profile"])]
    pub prior: String,
    /// Quick look: a quarter of the optimizer evaluations and 5 point-wise folds
    #[arg(long)]
    pub fast: bool,
}

/// Runs one `flashcp sim` subcommand.
pub fn run(cmd: SimCmd) -> Result<()> {
    match cmd {
        SimCmd::Run(a) => replay(&a),
        SimCmd::Profiles { name } => list_profiles(name.as_deref()),
        SimCmd::Sweep(a) => sweep(&a),
        SimCmd::Calibrate(a) => calibrate_cmd(&a),
    }
}

// ───────────────────────────── shared helpers ─────────────────────────────

/// Settings of one sweep point: flashcp arguments and environment variables.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// One point of a sweep: `(key, value)` pairs in `--vary` order.
pub type Combo = Vec<(String, String)>;

const DATASETS: &[&str] = &["small", "smallB", "many", "mixed", "file512m"];
const DRIVES: &[&str] = &["sandisk", "msi", "kingston"];

fn require_seeds(seeds: usize) -> Result<()> {
    if seeds == 0 {
        bail!("--seeds must be at least 1");
    }
    Ok(())
}

fn jobs_or_default(jobs: Option<usize>) -> usize {
    jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get())).max(1)
}

/// The drive a profile's workload is recorded on: `--record-as` if given, else the profile's
/// own drive, else sandisk.
fn record_drive<'a>(profile: &str, record_as: Option<&'a str>) -> &'a str {
    if let Some(d) = record_as {
        return d;
    }
    match profile {
        "msi-datamag-uas" | "msi" => "msi",
        "kingston-dt3-usb3" | "kingston" => "kingston",
        _ => "sandisk",
    }
}

/// Keeps ASCII letters, digits and `-`; everything else becomes `_`.
pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// Renders rows as a left-aligned table with two spaces between columns.
fn render_table(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut w: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate().take(w.len()) {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |cells: &[String]| -> String {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate().take(w.len()) {
            if i > 0 {
                s.push_str("  ");
            }
            let _ = write!(s, "{c:<width$}", width = w[i]);
        }
        s.trim_end().to_string()
    };
    let mut out = line(headers);
    out.push('\n');
    for r in rows {
        out.push_str(&line(r));
        out.push('\n');
    }
    out
}

/// A JSON string literal.
fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A JSON number (`null` if not finite).
fn jnum(x: f64) -> String {
    if x.is_finite() { format!("{x:.6}") } else { "null".to_string() }
}

/// A JSON object from already-rendered values.
fn jobj(fields: Vec<(&str, String)>) -> String {
    let body: Vec<String> = fields.iter().map(|(k, v)| format!("{}:{v}", jstr(k))).collect();
    format!("{{{}}}", body.join(","))
}

fn jarr(items: Vec<String>) -> String {
    format!("[{}]", items.join(","))
}

// ───────────────────────────── vary, mapping, scenarios (pure) ─────────────────────────────

/// One `--vary KEY=V1,V2,...` argument.
#[derive(Clone, Debug, PartialEq)]
pub struct Vary {
    pub key: String,
    pub values: Vec<String>,
}

/// Parses `KEY=V1,V2,...`. Keys and values must be non-empty.
pub fn parse_vary(spec: &str) -> Result<Vary> {
    let (key, vals) = spec.split_once('=').ok_or_else(|| anyhow!("--vary `{spec}`: expected KEY=V1,V2,..."))?;
    if key.is_empty() {
        bail!("--vary `{spec}`: empty key");
    }
    let values: Vec<String> = vals.split(',').map(str::to_string).collect();
    if values.iter().any(|v| v.is_empty()) {
        bail!("--vary `{spec}`: empty value");
    }
    Ok(Vary { key: key.to_string(), values })
}

/// The keys `--vary` accepts for an op.
pub fn valid_keys(op: &str) -> &'static [&'static str] {
    match op {
        "pull" => &["gap_kb", "inflight", "chunk_kb", "tail", "buffered"],
        "add" => &["gap", "qd", "build"],
        "build" => &["cluster", "fat"],
        _ => &[],
    }
}

/// Rejects unknown ops, unknown keys (listing the valid ones) and repeated keys.
pub fn check_vary(op: &str, varies: &[Vary]) -> Result<()> {
    if valid_keys(op).is_empty() {
        bail!("unknown op `{op}` (build | add | pull)");
    }
    let valid = valid_keys(op);
    for (i, v) in varies.iter().enumerate() {
        if !valid.contains(&v.key.as_str()) {
            bail!("unknown --vary key `{}` for {op} (valid keys: {})", v.key, valid.join(", "));
        }
        if varies[..i].iter().any(|w| w.key == v.key) {
            bail!("--vary key `{}` is given twice", v.key);
        }
    }
    Ok(())
}

/// The cartesian product of the value lists, first key slowest and last key fastest.
pub fn cartesian(varies: &[Vary]) -> Vec<Combo> {
    let mut out: Vec<Combo> = vec![Vec::new()];
    for v in varies {
        out = out
            .iter()
            .flat_map(|combo| {
                v.values.iter().map(move |val| {
                    let mut c = combo.clone();
                    c.push((v.key.clone(), val.clone()));
                    c
                })
            })
            .collect();
    }
    out
}

fn uint(key: &str, v: &str, min: u64) -> Result<String> {
    match v.parse::<u64>() {
        Ok(n) if n >= min => Ok(v.to_string()),
        _ => bail!("{key}: `{v}` is not an integer >= {min}"),
    }
}

fn check_gap(g: &str) -> Result<()> {
    if matches!(g, "skip" | "fill") {
        return Ok(());
    }
    match g.strip_prefix("pad").map(str::parse::<u64>) {
        Some(Ok(_)) => Ok(()),
        _ => bail!("gap must be skip, fill or pad<bytes>, not `{g}`"),
    }
}

/// Maps one sweep point to flashcp arguments and environment. Keys not valid for `op` are
/// errors. Keys not given keep flashcp's defaults.
pub fn settings_for(op: &str, combo: &[(String, String)]) -> Result<Settings> {
    let valid = valid_keys(op);
    for (k, _) in combo {
        if !valid.contains(&k.as_str()) {
            bail!("unknown --vary key `{k}` for {op} (valid keys: {})", valid.join(", "));
        }
    }
    let get = |key: &str| combo.iter().find(|(k, _)| k.as_str() == key).map(|(_, v)| v.as_str());
    let mut s = Settings::default();
    match op {
        "pull" => {
            for (key, flag) in [("gap_kb", "--gap-kb"), ("inflight", "--inflight"), ("chunk_kb", "--chunk-kb")] {
                if let Some(v) = get(key) {
                    let min = if key == "gap_kb" { 0 } else { 1 };
                    s.args.push(flag.to_string());
                    s.args.push(uint(key, v, min)?);
                }
            }
            match get("tail") {
                None | Some("512") => {}
                Some("4096") => s.env.push(("FLASHCP_TEST_TAIL_4K".to_string(), "1".to_string())),
                Some(t) => bail!("tail must be 512 or 4096, not `{t}`"),
            }
            if let Some(b) = get("buffered") {
                if !matches!(b, "true" | "false") {
                    bail!("buffered must be true or false, not `{b}`");
                }
                s.args.push("--buffered".to_string());
                s.args.push(b.to_string());
            }
        }
        "add" => {
            if get("gap").is_some() || get("qd").is_some() {
                let gap = get("gap").unwrap_or("skip");
                check_gap(gap)?;
                let qd = uint("qd", get("qd").unwrap_or("1"), 1)?;
                s.env.push(("FLASHCP_TEST_GAP_POLICY".to_string(), format!("{gap}:{qd}")));
            }
            match get("build") {
                None | Some("yes") => {}
                Some("no") => s.env.push(("FLASHCP_TEST_NO_ADD_BUILD".to_string(), "1".to_string())),
                Some(b) => bail!("build must be yes or no, not `{b}`"),
            }
        }
        "build" => {
            if let Some(c) = get("cluster") {
                s.args.push("--cluster".to_string());
                s.args.push(uint("cluster", c, 1)?);
            }
            if let Some(f) = get("fat") {
                if !matches!(f, "full" | "prefix") {
                    bail!("fat must be full or prefix, not `{f}`");
                }
                s.args.push("--fat".to_string());
                s.args.push(f.to_string());
            }
        }
        _ => bail!("unknown op `{op}` (build | add | pull)"),
    }
    Ok(s)
}

/// `sweep-<op>-<workload>[-<combo>]`, the combo sanitized (`gap_kb=0` becomes `gap_kb_0`).
pub fn scenario_id(op: &str, workload: &str, combo: &[(String, String)]) -> String {
    let slug = sanitize(&combo.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("_"));
    if slug.is_empty() { format!("sweep-{op}-{workload}") } else { format!("sweep-{op}-{workload}-{slug}") }
}

/// The dataset an `add` layout holds (`kernel32k:small` gives `small`).
fn layout_data(layout: &str) -> Option<&str> {
    layout.split(':').nth(1)
}

/// Whether two datasets write the same file names: the same one, or two of the `small` family
/// (`small` and `smallB` come from one generator with different seeds).
fn shares_names(a: &str, b: &str) -> bool {
    let small = |d: &str| matches!(d, "small" | "smallB");
    a == b || (small(a) && small(b))
}

/// One synthetic scenario per combo. Layout defaults: blank (build), flashcp:<W> (pull),
/// kernel32k:small (add, sandisk) or kernel128k:small (add, msi). For add, `--to B` is passed
/// when the workload's dataset shares names with the layout's dataset.
pub fn sweep_scenarios(
    op: &str,
    workload: &str,
    drive: &str,
    layout: Option<&str>,
    combos: &[Combo],
) -> Result<Vec<Scenario>> {
    if !DATASETS.contains(&workload) {
        bail!("unknown workload `{workload}` (small | smallB | many | mixed | file512m)");
    }
    let default = match op {
        "build" => "blank".to_string(),
        "pull" => format!("flashcp:{workload}"),
        "add" => if drive == "sandisk" { "kernel32k:small" } else { "kernel128k:small" }.to_string(),
        _ => bail!("unknown op `{op}` (build | add | pull)"),
    };
    let layout = layout.map(str::to_string).unwrap_or(default);
    let shares = layout_data(&layout).is_some_and(|d| shares_names(d, workload));
    combos
        .iter()
        .map(|combo| -> Result<Scenario> {
            let mut st = settings_for(op, combo)?;
            if op == "add" && shares {
                st.args.push("--to".to_string());
                st.args.push("B".to_string());
            }
            Ok(Scenario {
                id: scenario_id(op, workload, combo),
                drive: drive.to_string(),
                op: op.to_string(),
                data: workload.to_string(),
                layout: layout.clone(),
                args: st.args,
                env: st.env,
                src: "sweep".to_string(),
                ..Default::default()
            })
        })
        .collect()
}

/// Free parameters when `--free` is not given: every key the base model can use. The model
/// has no per-transport helper, so the ftl.* keys are left out unless the model has an FTL.
/// Parameters whose base value is not positive are left out too (calibration fits in log
/// space and needs a positive start); each one left out is reported on stderr.
/// The workload family of a scenario for grouped cross-validation: drive × op × dataset ×
/// layout (micro-probe points: drive × kind). Scenarios that differ only in flashcp settings
/// (args/env) share a family and are held out together.
fn family_of(sc: &Scenario) -> String {
    if sc.op == "micro" {
        format!("{}/micro/{}", sc.drive, sc.kind.as_deref().unwrap_or("?"))
    } else {
        format!("{}/{}/{}/{}", sc.drive, sc.op, sc.data, sc.layout)
    }
}

const STRUCTURAL: &[&str] = &["lanes", "max_req", "rd.streams", "ftl.au", "ftl.open", "ftl.gc_mean", "ftl.gc_sigma"];

fn default_free(base: &Model) -> Vec<String> {
    let mut free = Vec::new();
    for k in Model::keys() {
        if base.ftl.is_none() && k.starts_with("ftl.") {
            continue;
        }
        // Structure, not cost: integers from sysfs or the drive class (lanes, request size,
        // tracked streams, AU geometry), and the GC shape, which a deterministic (mean) fit
        // cannot see apart from gc_p.
        if STRUCTURAL.contains(k) || (base.lanes <= 1 && k.ends_with(".ser")) {
            continue;
        }
        match base.get(k) {
            Some(v) if v > 0.0 => free.push(k.to_string()),
            _ => eprintln!("[sim] {}: {k} is not positive in the base model, not fitted", base.name),
        }
    }
    free
}

fn check_free(names: &[String]) -> Result<Vec<String>> {
    for n in names {
        if !Model::keys().contains(&n.as_str()) {
            bail!("--free: unknown parameter `{n}` (valid: {})", Model::keys().join(", "));
        }
    }
    Ok(names.to_vec())
}

// ───────────────────────────── subcommands ─────────────────────────────

fn replay(a: &RunArgs) -> Result<()> {
    require_seeds(a.seeds)?;
    let t = Trace::load_opts(&a.trace, a.allow_truncated)?;
    let st = t.stats();
    let mut results: Vec<(&str, SimResult, Spread)> = Vec::new();
    for p in &a.profile {
        let m = profiles::resolve(p)?;
        let (sim, spread) = model::simulate_seeds(&t, &m, a.seeds);
        results.push((p.as_str(), sim, spread));
    }
    if a.json {
        let items = results
            .iter()
            .map(|(p, sim, spread)| {
                let phases =
                    sim.phases.iter().map(|(n, s)| jobj(vec![("name", jstr(n)), ("secs", jnum(*s))])).collect();
                let mut f = vec![
                    ("profile", jstr(p)),
                    ("secs", jnum(sim.secs)),
                    ("device_busy_secs", jnum(sim.device_busy_secs)),
                    ("host_secs", jnum(sim.host_secs)),
                    ("requests", sim.requests.to_string()),
                    ("jumps", sim.jumps.to_string()),
                    ("merges", sim.merges.to_string()),
                    ("stalls", sim.stalls.to_string()),
                    ("phases", jarr(phases)),
                ];
                if a.seeds > 1 {
                    f.push(("seeds", spread.n.to_string()));
                    f.push(("mean_secs", jnum(spread.mean)));
                    f.push(("p10_secs", jnum(spread.p10)));
                    f.push(("p90_secs", jnum(spread.p90)));
                }
                jobj(f)
            })
            .collect();
        println!("{}", jarr(items));
        return Ok(());
    }
    println!(
        "trace {} (cmd {}, device {}): {} reads ({:.1} MB), {} writes ({:.1} MB), {} syncs",
        a.trace.display(),
        t.cmd,
        t.device.as_deref().unwrap_or("-"),
        st.reads,
        st.read_bytes as f64 / 1e6,
        st.writes,
        st.write_bytes as f64 / 1e6,
        st.syncs
    );
    let mut headers: Vec<String> =
        ["profile", "secs", "device s", "host s", "requests", "jumps", "merges", "stalls"].map(str::to_string).to_vec();
    if a.seeds > 1 {
        headers.extend(["mean s", "p10-p90 s"].map(str::to_string));
    }
    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|(p, sim, spread)| {
            let mut r = vec![
                p.to_string(),
                format!("{:.2}", sim.secs),
                format!("{:.2}", sim.device_busy_secs),
                format!("{:.2}", sim.host_secs),
                sim.requests.to_string(),
                sim.jumps.to_string(),
                sim.merges.to_string(),
                sim.stalls.to_string(),
            ];
            if a.seeds > 1 {
                r.push(format!("{:.2}", spread.mean));
                r.push(format!("{:.2}-{:.2}", spread.p10, spread.p90));
            }
            r
        })
        .collect();
    print!("{}", render_table(&headers, &rows));
    for (p, sim, _) in &results {
        if !sim.phases.is_empty() {
            let phases: Vec<String> = sim.phases.iter().map(|(n, s)| format!("{n} {s:.2} s")).collect();
            println!("{p} phases: {}", phases.join(", "));
        }
    }
    Ok(())
}

fn list_profiles(name: Option<&str>) -> Result<()> {
    match name {
        None => {
            for n in profiles::builtin_names() {
                println!("{n}");
            }
        }
        Some(n) => {
            let text = profiles::resolve(n)?.render();
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(())
}

struct SweepRow {
    combo: Combo,
    spread: Spread,
    stats: TraceStats,
}

fn sweep(a: &SweepArgs) -> Result<()> {
    require_seeds(a.seeds)?;
    let model = profiles::resolve(&a.profile)?;
    let varies = a.vary.iter().map(|s| parse_vary(s)).collect::<Result<Vec<_>>>()?;
    check_vary(&a.op, &varies)?;
    let combos = cartesian(&varies);
    let drive = record_drive(&a.profile, a.record_as.as_deref());
    let scs = sweep_scenarios(&a.op, &a.workload, drive, a.layout.as_deref(), &combos)?;
    let wb = Workbench::new(a.work.clone())?;
    // Host time comes from the recording: on a drive where it dominates (UAS), parallel
    // recordings would inflate it with their own contention.
    let jobs = a.jobs.unwrap_or(if model.lanes > 1 { 1 } else { jobs_or_default(None) }).max(1);
    eprintln!("[sim] recording {} points on {drive} ({jobs} jobs)", scs.len());
    let recorded = wb.record_all(&scs, jobs);

    let mut rows = Vec::new();
    let mut failed = Vec::new();
    for (combo, (sc, r)) in combos.iter().zip(scs.iter().zip(recorded)) {
        match r.and_then(|p| Trace::load(&p)) {
            Ok(t) => {
                let (_, spread) = model::simulate_seeds(&t, &model, a.seeds);
                rows.push(SweepRow { combo: combo.clone(), spread, stats: t.stats() });
            }
            Err(e) => failed.push(format!("{}: {e:#}", sc.id)),
        }
    }
    if !failed.is_empty() {
        bail!("{} of {} points could not be recorded:\n  {}", failed.len(), scs.len(), failed.join("\n  "));
    }
    rows.sort_by(|x, y| x.spread.mean.total_cmp(&y.spread.mean));
    let keys: Vec<String> = varies.iter().map(|v| v.key.clone()).collect();
    if a.json {
        println!("{}", sweep_json(&keys, &rows));
    } else {
        print!("{}", sweep_table(&keys, &rows, a.seeds));
    }
    Ok(())
}

fn sweep_table(keys: &[String], rows: &[SweepRow], seeds: usize) -> String {
    let best = rows.first().map_or(0.0, |r| r.spread.mean);
    let mut headers = vec!["rank".to_string()];
    headers.extend(keys.iter().cloned());
    headers.push(if seeds > 1 { "mean s" } else { "sim s" }.to_string());
    if seeds > 1 {
        headers.push("p10-p90 s".to_string());
    }
    headers.extend(["vs best", "reads/writes", "MB moved"].map(str::to_string));
    let body: Vec<Vec<String>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut c = vec![(i + 1).to_string()];
            c.extend(keys.iter().map(|k| value_of(&r.combo, k).to_string()));
            c.push(format!("{:.2}", r.spread.mean));
            if seeds > 1 {
                c.push(format!("{:.2}-{:.2}", r.spread.p10, r.spread.p90));
            }
            c.push(format!("{:+.1}%", (r.spread.mean / best - 1.0) * 100.0));
            c.push(format!("{}/{}", r.stats.reads, r.stats.writes));
            c.push(format!("{:.1}", (r.stats.read_bytes + r.stats.write_bytes) as f64 / 1e6));
            c
        })
        .collect();
    render_table(&headers, &body)
}

fn value_of<'a>(combo: &'a Combo, key: &str) -> &'a str {
    combo.iter().find(|(k, _)| k == key).map_or("-", |(_, v)| v.as_str())
}

fn sweep_json(keys: &[String], rows: &[SweepRow]) -> String {
    let best = rows.first().map_or(0.0, |r| r.spread.mean);
    let items = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let values = keys.iter().map(|k| (k.as_str(), jstr(value_of(&r.combo, k)))).collect();
            jobj(vec![
                ("rank", (i + 1).to_string()),
                ("values", jobj(values)),
                ("secs", jnum(r.spread.mean)),
                ("p10_secs", jnum(r.spread.p10)),
                ("p90_secs", jnum(r.spread.p90)),
                ("delta_pct", jnum((r.spread.mean / best - 1.0) * 100.0)),
                ("reads", r.stats.reads.to_string()),
                ("writes", r.stats.writes.to_string()),
                ("mb", jnum((r.stats.read_bytes + r.stats.write_bytes) as f64 / 1e6)),
            ])
        })
        .collect();
    jarr(items)
}

fn micro_trace(sc: &Scenario) -> Result<Trace> {
    let kind = sc.kind.as_deref().ok_or_else(|| anyhow!("{}: micro scenario without kind", sc.id))?;
    let (Some(bs), Some(qd), Some(bytes)) = (sc.bs, sc.qd, sc.bytes) else {
        bail!("{}: micro scenario needs bs, qd and bytes", sc.id);
    };
    Trace::micro(kind, bs, qd, bytes)
}

fn calibrate_cmd(a: &CalibrateArgs) -> Result<()> {
    let (all, pairs) = match &a.scenarios {
        Some(p) => {
            let text = fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
            workload::parse_scenarios(&text)?
        }
        None => workload::builtin_scenarios(),
    };
    let drives: Vec<&str> = if a.drive == "all" { DRIVES.to_vec() } else { vec![a.drive.as_str()] };
    let measured: Vec<Scenario> =
        all.iter().filter(|s| s.real.is_some() && drives.contains(&s.drive.as_str())).cloned().collect();
    if measured.is_empty() {
        bail!("no measured scenarios for {}", a.drive);
    }

    let jobs = jobs_or_default(a.jobs);
    let wb = Workbench::new(a.work.clone())?;
    let to_record: Vec<Scenario> = measured.iter().filter(|s| s.op != "micro").cloned().collect();
    // Host-bound drives (msi) are recorded one at a time (their host time is what they
    // measure, and parallel recordings would inflate it); the rest in parallel.
    // (On the USB 3 Kingston, pulls take ~1 s and are partly host time too.)
    let host_bound = |s: &Scenario| s.drive != "sandisk";
    let (serial, par): (Vec<usize>, Vec<usize>) = (0..to_record.len()).partition(|&i| host_bound(&to_record[i]));
    eprintln!(
        "[sim] recording {} scenarios ({} in parallel, {jobs} jobs; {} one at a time)",
        to_record.len(),
        par.len(),
        serial.len()
    );
    let pick = |ix: &[usize]| ix.iter().map(|&i| to_record[i].clone()).collect::<Vec<_>>();
    let mut slots: Vec<Option<Result<std::path::PathBuf>>> = (0..to_record.len()).map(|_| None).collect();
    for (ix, j) in [(&par, jobs), (&serial, a.jobs.unwrap_or(1))] {
        for (&i, r) in ix.iter().zip(wb.record_all(&pick(ix), j)) {
            slots[i] = Some(r);
        }
    }
    let mut recorded = slots.into_iter().map(|r| r.expect("every scenario recorded")).collect::<Vec<_>>().into_iter();

    let mut points = Vec::new();
    let mut skipped = Vec::new();
    for sc in &measured {
        let Some(real) = sc.real else { continue };
        let built = if sc.op == "micro" {
            micro_trace(sc)
        } else {
            match recorded.next() {
                Some(r) => r.and_then(|p| Trace::load(&p)),
                None => Err(anyhow!("internal: missing recording for {}", sc.id)),
            }
        };
        match built {
            Ok(trace) => points.push(CalPoint {
                id: sc.id.clone(),
                drive: sc.drive.clone(),
                real,
                lo: sc.lo,
                hi: sc.hi,
                family: family_of(sc),
                trace,
            }),
            Err(e) => {
                eprintln!("[sim] skip {}: {e:#}", sc.id);
                skipped.push(sc.id.clone());
            }
        }
    }
    if points.is_empty() {
        bail!("nothing could be recorded for {}", a.drive);
    }

    let mut md = String::from("# flashcp sim calibration\n\n");
    let mut summaries = Vec::new();
    for &d in &drives {
        let pts: Vec<CalPoint> = points.iter().filter(|p| p.drive == d).cloned().collect();
        if pts.is_empty() {
            eprintln!("[sim] {d}: no point could be recorded, not calibrated");
            continue;
        }
        let name = workload::drive_model_name(d);
        let base = if a.prior == "probe" { profiles::calibration_prior(name) } else { profiles::builtin(name) }
            .ok_or_else(|| anyhow!("no built-in model for drive {d}"))?;
        let ids: HashSet<&str> = all.iter().filter(|s| s.drive == d).map(|s| s.id.as_str()).collect();
        let dpairs: Vec<(String, String)> = pairs
            .iter()
            .filter(|p| ids.contains(p.faster.as_str()) && ids.contains(p.slower.as_str()))
            .map(|p| (p.faster.clone(), p.slower.clone()))
            .collect();
        let free = if a.free.is_empty() { default_free(&base) } else { check_free(&a.free)? };
        let free_refs: Vec<&str> = free.iter().map(String::as_str).collect();
        let t0 = std::time::Instant::now();
        let rep = calib::calibrate_opts(&pts, &dpairs, &base, &free_refs, calib::CalOpts { fast: a.fast })?;
        let table = rep.table();
        let (ok, total) = rep.ranking();
        let (lok, _) = rep.ranking_lofo();
        let summary = format!(
            "{d}: {} points in {} families, median |held-out error| {:.1}% point-wise, {:.1}% \
             family-wise; ranking {ok}/{total} point-wise, {lok}/{total} family-wise ({:.0} s fit{})",
            pts.len(),
            rep.families,
            100.0 * rep.median_abs_err_cv(),
            100.0 * rep.median_abs_err_lofo(),
            t0.elapsed().as_secs_f64(),
            if a.fast { ", --fast" } else { "" }
        );
        println!("{}", table.trim_end());
        let _ = writeln!(md, "## {d}\n\n```text\n{}\n```\n\n{summary}\n", table.trim_end());
        if let Some(dir) = &a.fitted_dir {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            let f = dir.join(format!("{}.model", workload::drive_model_name(d)));
            fs::write(&f, rep.fitted().render()).with_context(|| format!("write {}", f.display()))?;
            println!("fitted model written to {}", f.display());
        }
        summaries.push(summary);
    }
    for s in &summaries {
        println!("{s}");
    }
    if !skipped.is_empty() {
        println!("skipped (could not be recorded): {}", skipped.join(", "));
        let _ = writeln!(md, "Skipped (could not be recorded): {}", skipped.join(", "));
    }
    if let Some(out) = &a.out {
        fs::write(out, md).with_context(|| format!("write {}", out.display()))?;
        println!("tables written to {}", out.display());
    }
    Ok(())
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(pairs: &[(&str, &str)]) -> Combo {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn vary(spec: &str) -> Vary {
        parse_vary(spec).unwrap()
    }

    #[test]
    fn parse_vary_splits_key_and_values() {
        let v = vary("gap_kb=0,4,16");
        assert_eq!(v.key, "gap_kb");
        assert_eq!(v.values, ["0", "4", "16"]);
        assert!(parse_vary("gap_kb").is_err());
        assert!(parse_vary("=1,2").is_err());
        assert!(parse_vary("gap_kb=1,,2").is_err());
    }

    #[test]
    fn cartesian_first_key_slowest() {
        let c = cartesian(&[vary("a=1,2"), vary("b=x,y,z")]);
        assert_eq!(
            c,
            vec![
                combo(&[("a", "1"), ("b", "x")]),
                combo(&[("a", "1"), ("b", "y")]),
                combo(&[("a", "1"), ("b", "z")]),
                combo(&[("a", "2"), ("b", "x")]),
                combo(&[("a", "2"), ("b", "y")]),
                combo(&[("a", "2"), ("b", "z")]),
            ]
        );
        assert_eq!(cartesian(&[]), vec![Combo::new()]);
    }

    #[test]
    fn mapping_covers_every_pull_key() {
        let s = settings_for("pull", &combo(&[("gap_kb", "16")])).unwrap();
        assert_eq!(s.args, ["--gap-kb", "16"]);
        let s = settings_for("pull", &combo(&[("inflight", "4")])).unwrap();
        assert_eq!(s.args, ["--inflight", "4"]);
        let s = settings_for("pull", &combo(&[("chunk_kb", "8192")])).unwrap();
        assert_eq!(s.args, ["--chunk-kb", "8192"]);
        let s = settings_for("pull", &combo(&[("tail", "4096")])).unwrap();
        assert_eq!(s.env, [("FLASHCP_TEST_TAIL_4K".to_string(), "1".to_string())]);
        assert_eq!(settings_for("pull", &combo(&[("tail", "512")])).unwrap(), Settings::default());
        assert!(settings_for("pull", &combo(&[("tail", "1024")])).is_err());
        let s = settings_for("pull", &combo(&[("buffered", "true")])).unwrap();
        assert_eq!(s.args, ["--buffered", "true"]);
        let s = settings_for("pull", &combo(&[("buffered", "false")])).unwrap();
        assert_eq!(s.args, ["--buffered", "false"]);
        assert!(settings_for("pull", &combo(&[("buffered", "yes")])).is_err());
    }

    #[test]
    fn mapping_covers_every_add_key() {
        let env = |v: &str| vec![("FLASHCP_TEST_GAP_POLICY".to_string(), v.to_string())];
        assert_eq!(settings_for("add", &combo(&[("gap", "fill")])).unwrap().env, env("fill:1"));
        assert_eq!(settings_for("add", &combo(&[("qd", "4")])).unwrap().env, env("skip:4"));
        assert_eq!(settings_for("add", &combo(&[("gap", "pad4096"), ("qd", "8")])).unwrap().env, env("pad4096:8"));
        assert!(settings_for("add", &combo(&[("gap", "pad")])).is_err());
        assert!(settings_for("add", &combo(&[("qd", "0")])).is_err());
        let no_build = vec![("FLASHCP_TEST_NO_ADD_BUILD".to_string(), "1".to_string())];
        assert_eq!(settings_for("add", &combo(&[("build", "no")])).unwrap().env, no_build);
        assert_eq!(settings_for("add", &combo(&[("build", "yes")])).unwrap(), Settings::default());
    }

    #[test]
    fn mapping_covers_every_build_key() {
        let s = settings_for("build", &combo(&[("cluster", "512")])).unwrap();
        assert_eq!(s.args, ["--cluster", "512"]);
        let s = settings_for("build", &combo(&[("fat", "full")])).unwrap();
        assert_eq!(s.args, ["--fat", "full"]);
        assert!(settings_for("build", &combo(&[("fat", "half")])).is_err());
    }

    #[test]
    fn unknown_key_is_an_error_listing_the_valid_keys() {
        let err = check_vary("pull", &[vary("cluster=512")]).unwrap_err().to_string();
        assert!(err.contains("unknown --vary key `cluster`"), "{err}");
        assert!(err.contains("gap_kb, inflight, chunk_kb, tail, buffered"), "{err}");
        assert!(settings_for("add", &combo(&[("gap_kb", "4")])).is_err());
        assert!(check_vary("pull", &[vary("gap_kb=1"), vary("gap_kb=2")]).is_err());
        assert!(check_vary("scrub", &[]).is_err());
    }

    #[test]
    fn ids_are_sanitized() {
        assert_eq!(sanitize("gap_kb=0 x/y"), "gap_kb_0_x_y");
        assert_eq!(sanitize("a-b9"), "a-b9");
        assert_eq!(
            scenario_id("pull", "small", &combo(&[("gap_kb", "0"), ("inflight", "1")])),
            "sweep-pull-small-gap_kb_0_inflight_1"
        );
        assert_eq!(scenario_id("build", "many", &Combo::new()), "sweep-build-many");
    }

    #[test]
    fn sweep_scenarios_pick_layouts_and_add_to() {
        let sc = sweep_scenarios("pull", "small", "sandisk", None, &[Combo::new()]).unwrap();
        assert_eq!(sc[0].layout, "flashcp:small");
        assert_eq!(sc[0].src, "sweep");
        let sc = sweep_scenarios("add", "smallB", "sandisk", None, &[Combo::new()]).unwrap();
        assert_eq!(sc[0].layout, "kernel32k:small");
        assert_eq!(sc[0].args, ["--to", "B"]);
        let sc = sweep_scenarios("add", "mixed", "msi", None, &[Combo::new()]).unwrap();
        assert_eq!(sc[0].layout, "kernel128k:small");
        assert!(sc[0].args.is_empty());
        let sc = sweep_scenarios("build", "small", "msi", None, &[combo(&[("cluster", "512")])]).unwrap();
        assert_eq!(sc[0].layout, "blank");
        assert_eq!(sc[0].args, ["--cluster", "512"]);
        assert!(sweep_scenarios("build", "bogus", "msi", None, &[Combo::new()]).is_err());
    }

    #[test]
    fn record_drive_defaults() {
        assert_eq!(record_drive("sandisk-cruzer-usb2", None), "sandisk");
        assert_eq!(record_drive("msi", None), "msi");
        assert_eq!(record_drive("usb3-midrange", None), "sandisk");
        assert_eq!(record_drive("usb3-midrange", Some("msi")), "msi");
    }
}
