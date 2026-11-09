mod bench;
mod copy;
mod dataset;
mod exfat;
mod pack;
mod probe;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "flashcp", about = "Fast file transfer to USB sticks and external drives")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a reproducible test dataset (small | mixed | large | tiny)
    Gen {
        dir: PathBuf,
        #[arg(long, default_value = "small")]
        profile: String,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Measure a drive through a temporary file in DIR (no raw writes, no root)
    Probe {
        dir: PathBuf,
        /// Probe file size in MB
        #[arg(long, default_value_t = 512)]
        size_mb: u64,
    },
    /// Copy a tree as loose files with the pipelined, ordered copier
    Copy {
        src: PathBuf,
        dst: PathBuf,
        #[arg(long)]
        readers: Option<usize>,
        #[arg(long, default_value_t = 1)]
        writers: usize,
        #[arg(long)]
        verify: bool,
    },
    /// Pack a tree into one sequential tar stream (optionally zstd)
    Pack {
        src: PathBuf,
        out: PathBuf,
        #[arg(long, value_enum, default_value = "auto")]
        compress: pack::Compress,
    },
    /// Build a complete exFAT volume from SRC onto TARGET (block device or image file) in one
    /// sequential pass. ERASES TARGET.
    Build {
        src: PathBuf,
        target: PathBuf,
        /// Cluster size in bytes (default: optimizer picks)
        #[arg(long)]
        cluster: Option<u32>,
        #[arg(long, default_value = "FLASHCP")]
        label: String,
        #[arg(long, value_enum, default_value = "prefix")]
        fat: exfat::FatMode,
        #[arg(long, default_value_t = 12)]
        readers: usize,
        /// Only print the plan
        #[arg(long)]
        dry_run: bool,
        /// Required unless --dry-run: confirms TARGET may be erased
        #[arg(long)]
        yes_erase: bool,
    },
    /// Benchmark every method against DEST
    Bench {
        src: PathBuf,
        dest: PathBuf,
        /// Comma-separated: naive,cp,smart,smart-w4,pack,pack-auto
        #[arg(long, default_value = "naive,cp,smart,smart-w4,pack,pack-auto")]
        methods: String,
        #[arg(long, default_value_t = 1)]
        runs: usize,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Gen { dir, profile, seed } => dataset::generate(&dir, &profile, seed),
        Cmd::Probe { dir, size_mb } => {
            let r = probe::probe(&dir, size_mb << 20)?;
            probe::report(&r);
            Ok(())
        }
        Cmd::Copy { src, dst, readers, writers, verify } => {
            let mut o = copy::SmartOpts::default();
            if let Some(r) = readers {
                o.readers = r;
            }
            o.writers = writers;
            let s = copy::smart_copy(&src, &dst, o)?;
            println!(
                "{} files, {} in {:.2}s ({:.0} files/s, {:.1} MB/s)",
                s.files,
                util::human_bytes(s.bytes as f64),
                s.secs,
                s.files as f64 / s.secs,
                s.bytes as f64 / 1e6 / s.secs
            );
            if verify {
                copy::verify(&src, &dst)?;
                println!("verified ✓");
            }
            Ok(())
        }
        Cmd::Pack { src, out, compress } => {
            let s = pack::pack(&src, &out, compress)?;
            println!(
                "{} files, {} → {} in {:.2}s ({:.1} MB/s input){}",
                s.files,
                util::human_bytes(s.bytes_in as f64),
                util::human_bytes(s.bytes_out as f64),
                s.secs,
                s.bytes_in as f64 / 1e6 / s.secs,
                if s.compressed { ", zstd" } else { "" }
            );
            Ok(())
        }
        Cmd::Build { src, target, cluster, label, fat, readers, dry_run, yes_erase } => {
            anyhow::ensure!(dry_run || yes_erase, "this ERASES {}; pass --yes-erase to confirm", target.display());
            let o = exfat::BuildOpts { cluster, label, fat, readers, dry_run };
            let s = exfat::build(&src, &target, &o)?;
            if !dry_run {
                println!(
                    "built: {} files, {} dirs, {} data, {} written, cluster {} in {:.2}s ({:.0} files/s, {:.1} MB/s of data)",
                    s.files,
                    s.dirs,
                    util::human_bytes(s.data_bytes as f64),
                    util::human_bytes(s.written as f64),
                    util::human_bytes(s.cluster as f64),
                    s.secs,
                    s.files as f64 / s.secs,
                    s.data_bytes as f64 / 1e6 / s.secs
                );
            }
            Ok(())
        }
        Cmd::Bench { src, dest, methods, runs } => {
            let ms: Vec<String> = methods.split(',').map(|s| s.trim().to_string()).collect();
            std::fs::create_dir_all(&dest)?;
            let free = util::statfs_free(&dest)?;
            let (_, files) = util::scan(&src)?;
            let need: u64 = files.iter().map(|e| e.size).sum::<u64>() * 2;
            anyhow::ensure!(free > need, "not enough free space on {}", dest.display());
            bench::bench(&src, &dest, &ms, runs.max(1))?;
            Ok(())
        }
    }
}
