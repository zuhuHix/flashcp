//! Benchmark harness. Every method writes the same dataset to the same drive and is timed
//! up to and including the final flush to the device. Loose-file results are verified
//! byte for byte. The source is pre-read so every method starts from the same warm cache.

use crate::copy::{SmartOpts, naive_copy, smart_copy, verify};
use crate::pack::{Compress, pack};
use crate::util::{human_bytes, scan, syncfs};
use anyhow::{Result, bail};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

pub const ALL: &[&str] = &["naive", "cp", "smart", "smart-w4", "pack", "pack-auto"];

pub struct Row {
    pub method: String,
    pub secs: f64,
    pub files: usize,
    pub bytes: u64,
    pub note: String,
}

fn prewarm(src: &Path) -> Result<()> {
    let (_, files) = scan(src)?;
    let mut buf = vec![0u8; 1 << 20];
    for e in files {
        let mut f = fs::File::open(src.join(&e.rel))?;
        while f.read(&mut buf)? > 0 {}
    }
    Ok(())
}

fn run_one(method: &str, src: &Path, out: &Path) -> Result<Row> {
    let mut note = String::new();
    let (secs, files, bytes) = match method {
        "naive" => {
            let s = naive_copy(src, out)?;
            (s.secs, s.files, s.bytes)
        }
        "cp" => {
            let (_, f) = scan(src)?;
            let b = f.iter().map(|e| e.size).sum();
            let t = Instant::now();
            let st = Command::new("cp").arg("-r").arg(src).arg(out).status()?;
            if !st.success() {
                bail!("cp failed");
            }
            syncfs(out)?;
            (t.elapsed().as_secs_f64(), f.len(), b)
        }
        "smart" | "smart-w4" => {
            let mut o = SmartOpts::default();
            if method == "smart-w4" {
                o.writers = 4;
            }
            let s = smart_copy(src, out, o)?;
            (s.secs, s.files, s.bytes)
        }
        "pack" | "pack-auto" => {
            fs::create_dir_all(out)?;
            let mode = if method == "pack" { Compress::None } else { Compress::Auto };
            let s = pack(src, &out.join("data.tar"), mode)?;
            note = format!(
                "→ {} on disk{}",
                human_bytes(s.bytes_out as f64),
                match s.sample_ratio {
                    Some(r) => format!(", sample ratio {r:.2} → {}", if s.compressed { "zstd on" } else { "zstd off" }),
                    None => String::new(),
                }
            );
            (s.secs, s.files, s.bytes_in)
        }
        _ => bail!("unknown method '{method}' (have: {})", ALL.join(", ")),
    };
    if !method.starts_with("pack") {
        // `cp -r src out` with a missing `out` copies the tree's contents into `out`.
        verify(src, out)?;
        note = "verified ✓".into();
    }
    Ok(Row { method: method.into(), secs, files, bytes, note })
}

pub fn bench(src: &Path, dest: &Path, methods: &[String], runs: usize) -> Result<Vec<Row>> {
    let src = fs::canonicalize(src)?;
    fs::create_dir_all(dest)?;
    let work = dest.join(".flashcp-bench");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    println!("pre-reading source so every method starts warm…");
    prewarm(&src)?;

    let mut rows = Vec::new();
    for m in methods {
        let mut times = Vec::new();
        let mut last = None;
        for r in 0..runs {
            let out = work.join(format!("{m}-{r}"));
            syncfs(&work)?;
            let row = run_one(m, &src, &out)?;
            println!("  {:<10} run {}  {:>7.2} s  {}", m, r + 1, row.secs, row.note);
            times.push(row.secs);
            last = Some(row);
            fs::remove_dir_all(&out)?;
            syncfs(&work)?;
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut row = last.unwrap();
        row.secs = times[times.len() / 2];
        rows.push(row);
    }
    let _ = fs::remove_dir_all(&work);
    print_table(&rows);
    Ok(rows)
}

pub fn print_table(rows: &[Row]) {
    let base = rows.iter().find(|r| r.method == "naive").map(|r| r.secs);
    println!();
    println!("{:<10} {:>9} {:>11} {:>11} {:>8}  notes", "method", "time", "files/s", "MB/s", "speedup");
    println!("{}", "─".repeat(78));
    for r in rows {
        let sp = base.map(|b| format!("{:.2}×", b / r.secs)).unwrap_or_default();
        println!(
            "{:<10} {:>8.2}s {:>11.0} {:>11.1} {:>8}  {}",
            r.method,
            r.secs,
            r.files as f64 / r.secs,
            r.bytes as f64 / 1e6 / r.secs,
            sp,
            r.note
        );
    }
}
