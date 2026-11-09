use anyhow::Result;
use clap::{Parser, Subcommand};
use flashcp::TargetSpec;
use flashcp::events::{Event, error_json};
use flashcp::{add, bench, copy, dataset, exfat, pack, probe, util};
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
        /// Block device or image file (or use --target-fd)
        #[arg(required_unless_present = "target_fd", conflicts_with = "target_fd")]
        target: Option<PathBuf>,
        /// Use this inherited, already-open block-device fd (O_RDWR|O_EXCL; O_RDONLY for info)
        /// instead of opening TARGET, e.g. from a privileged helper
        #[arg(long, value_name = "N")]
        target_fd: Option<i32>,
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
        /// Print progress as JSON lines on stdout
        #[arg(long)]
        json: bool,
    },
    /// Add SRC's contents to an EXISTING exFAT volume on TARGET (unmounted block device or
    /// image file) without erasing anything
    Add {
        src: PathBuf,
        /// Block device or image file (or use --target-fd)
        #[arg(required_unless_present = "target_fd", conflicts_with = "target_fd")]
        target: Option<PathBuf>,
        /// Use this inherited, already-open block-device fd (O_RDWR|O_EXCL; O_RDONLY for info)
        /// instead of opening TARGET, e.g. from a privileged helper
        #[arg(long, value_name = "N")]
        target_fd: Option<i32>,
        /// Directory inside the volume to add into, e.g. "Backups/2026" (created if missing)
        #[arg(long)]
        to: Option<String>,
        /// Skip entries whose name already exists (case-insensitive) instead of failing
        #[arg(long)]
        skip_existing: bool,
        #[arg(long, default_value_t = 12)]
        readers: usize,
        /// Only print the plan
        #[arg(long)]
        dry_run: bool,
        /// Print progress as JSON lines on stdout
        #[arg(long)]
        json: bool,
    },
    /// Show an exFAT volume's label, size, free space and file count (read-only)
    Info {
        /// Block device or image file (or use --target-fd)
        #[arg(required_unless_present = "target_fd", conflicts_with = "target_fd")]
        target: Option<PathBuf>,
        /// Use this inherited, already-open block-device fd (O_RDWR|O_EXCL; O_RDONLY for info)
        /// instead of opening TARGET, e.g. from a privileged helper
        #[arg(long, value_name = "N")]
        target_fd: Option<i32>,
        #[arg(long)]
        json: bool,
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

/// Event sink: JSON lines on stdout, or the human-readable plan line.
fn sink(json: bool) -> impl FnMut(&Event) {
    move |e: &Event| {
        if json {
            use std::io::Write;
            let mut o = std::io::stdout().lock();
            let _ = writeln!(o, "{}", e.to_json());
            let _ = o.flush();
        } else if let Event::Plan { detail, .. } = e {
            println!("{detail}");
        }
    }
}

/// clap guarantees exactly one of TARGET / --target-fd.
fn spec(target: Option<PathBuf>, fd: Option<i32>) -> TargetSpec {
    match (target, fd) {
        (_, Some(fd)) => TargetSpec::Fd(fd),
        (Some(p), None) => TargetSpec::Path(p),
        (None, None) => unreachable!("clap requires TARGET or --target-fd"),
    }
}

fn main() {
    let cli = Cli::parse();
    let json =
        matches!(cli.cmd, Cmd::Build { json: true, .. } | Cmd::Add { json: true, .. } | Cmd::Info { json: true, .. });
    if let Err(e) = run(cli.cmd) {
        if json {
            println!("{}", error_json(&format!("{e:#}")));
        } else {
            eprintln!("Error: {e:#}");
        }
        std::process::exit(1);
    }
}

fn run(cmd: Cmd) -> Result<()> {
    match cmd {
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
        Cmd::Build { src, target, target_fd, cluster, label, fat, readers, dry_run, yes_erase, json } => {
            let target = spec(target, target_fd);
            anyhow::ensure!(dry_run || yes_erase, "this ERASES {}; pass --yes-erase to confirm", target.describe());
            let o = exfat::BuildOpts { cluster, label, fat, readers, dry_run };
            let s = exfat::build_to(&src, &target, &o, &mut sink(json))?;
            if !dry_run && !json {
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
        Cmd::Add { src, target, target_fd, to, skip_existing, readers, dry_run, json } => {
            let target = spec(target, target_fd);
            let o = add::AddOpts { to, skip_existing, readers, dry_run };
            let s = add::add_to(&src, &target, &o, &mut sink(json))?;
            if !dry_run && !json {
                println!(
                    "added: {} files, {} dirs, {} data{}{}, cluster {} in {:.2}s ({:.0} files/s, {:.1} MB/s of data)",
                    s.files,
                    s.dirs,
                    util::human_bytes(s.data_bytes as f64),
                    if s.skipped > 0 { format!(", skipped {} existing", s.skipped) } else { String::new() },
                    if s.fragmented > 0 { format!(", {} fragmented", s.fragmented) } else { String::new() },
                    util::human_bytes(s.cluster as f64),
                    s.secs,
                    s.files as f64 / s.secs,
                    s.data_bytes as f64 / 1e6 / s.secs
                );
            }
            Ok(())
        }
        Cmd::Info { target, target_fd, json } => {
            let i = add::info_of(&spec(target, target_fd))?;
            if json {
                println!("{}", i.to_json());
            } else {
                println!(
                    "label {:?} │ cluster {} │ size {} │ free {} │ {} files, {} dirs{}",
                    i.label,
                    util::human_bytes(i.cluster as f64),
                    util::human_bytes(i.size as f64),
                    util::human_bytes(i.free as f64),
                    i.files,
                    i.dirs,
                    if i.dirty { " │ DIRTY" } else { "" }
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
