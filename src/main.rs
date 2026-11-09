use anyhow::Result;
use clap::{Parser, Subcommand};
use flashcp::TargetSpec;
use flashcp::events::{Event, error_json};
use flashcp::{add, bench, copy, dataset, exfat, optimize, pack, probe, pull, util};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "flashcp", version, about = "Fast file transfer to USB sticks and external drives")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Record every device op to FILE as JSON lines, for `flashcp sim` (same as
    /// FLASHCP_TRACE=FILE; see SIMULATION.md)
    #[arg(id = "trace_out", long = "trace", global = true, value_name = "FILE", hide = true)]
    trace_out: Option<PathBuf>,
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
    /// Make a directory of an EXISTING exFAT volume on TARGET (unmounted block device or image
    /// file) match SRC: copies new and changed files (into free space; a changed file is switched
    /// over only once its new version is on the device), leaves unchanged ones alone.
    ///
    /// A file is taken as unchanged when its size and modification time (at exFAT's 10 ms
    /// precision) match: a change that keeps both (`cp -p`, `rsync -t`, some editors and tools
    /// restore the time) is skipped unless --checksum. Every metadata write is journaled, under
    /// $XDG_STATE_HOME/flashcp/sync-journal first and in the hidden file /.flashcp-sync-journal
    /// on the volume while the run commits: an interrupted sync is completed by the next sync of
    /// that volume (or by --recover), on any machine
    Sync {
        /// SRC TARGET: the local tree, then the block device or image file. With --target-fd,
        /// only SRC; with --recover, only TARGET (or nothing with --target-fd)
        #[arg(value_name = "SRC TARGET", num_args = 0..=2)]
        paths: Vec<PathBuf>,
        /// Use this inherited, already-open block-device fd (O_RDWR|O_EXCL) instead of opening
        /// TARGET, e.g. from a privileged helper
        #[arg(long, value_name = "N")]
        target_fd: Option<i32>,
        /// Only complete (replay the journal of) an interrupted sync of TARGET, then exit
        #[arg(long)]
        recover: bool,
        /// Directory inside the volume to sync, e.g. "Backups/laptop" (created if missing)
        #[arg(long)]
        to: Option<String>,
        /// Delete files and directories that are not in SRC (and re-link moved files instead of
        /// copying them again). Needs --yes when anything would be deleted
        #[arg(long)]
        delete: bool,
        /// Confirm the deletions the plan lists (--dry-run shows them without it)
        #[arg(long)]
        yes: bool,
        /// Confirm deleting more than 1000 entries (files and directories, counted recursively)
        #[arg(long)]
        yes_many: bool,
        /// Compare contents instead of the quick check (size and modification time at 10 ms):
        /// catches changes that kept both
        #[arg(long)]
        checksum: bool,
        /// Take a move (same size, time and sampled fingerprint) without comparing the full
        /// contents. RISKY: a different file with equal size, time and samples would be taken
        /// as a move of the old one
        #[arg(long)]
        fast_moves: bool,
        #[arg(long, default_value_t = 12)]
        readers: usize,
        /// Only print the plan
        #[arg(long)]
        dry_run: bool,
        /// Print progress as JSON lines on stdout
        #[arg(long)]
        json: bool,
    },
    /// Copy the files of an exFAT volume on TARGET (block device or image file, opened
    /// read-only; preferably unmounted) into DEST, reading the volume in large sequential chunks
    Pull {
        /// TARGET DEST: block device or image file, then the local destination directory
        /// (created if missing). With --target-fd, only DEST.
        #[arg(value_name = "TARGET DEST", num_args = 1..=2, required = true)]
        paths: Vec<PathBuf>,
        /// Use this inherited, already-open block-device fd (O_RDONLY is enough) instead of
        /// opening TARGET, e.g. from a privileged helper
        #[arg(long, value_name = "N")]
        target_fd: Option<i32>,
        /// Directory inside the volume to copy from, e.g. "Photos/2026"; its contents land in DEST
        #[arg(long)]
        from: Option<String>,
        /// Leave files that already exist in DEST alone
        #[arg(long, conflicts_with_all = ["overwrite", "update"])]
        skip_existing: bool,
        /// Replace files that already exist in DEST
        #[arg(long, conflicts_with = "update")]
        overwrite: bool,
        /// Copy only files that are new or whose size or modification time differ; replace changed ones safely.
        ///
        /// Quick check: a file in DEST is up to date when its size and modification time match the
        /// volume's (mtime at exFAT's 10 ms precision). A same-size change whose mtime was preserved
        /// (for example by `cp -p` or `rsync -t`) is therefore skipped, not copied.
        #[arg(long, conflicts_with_all = ["skip_existing", "overwrite"])]
        update: bool,
        /// With --update: delete files and directories in DEST that are not on the volume.
        ///
        /// The deletions are listed before anything is written. Nothing is deleted without --yes, and
        /// DEST is never pruned when it is `/`, a direct child of `/`, your home directory, an ancestor
        /// of it, or another user's home (a direct child of /home).
        #[arg(long, requires = "update")]
        delete_extraneous: bool,
        /// Confirm the deletions of --delete-extraneous
        #[arg(long, requires = "delete_extraneous")]
        yes: bool,
        /// Also confirm more than 1000 deletions (with --yes)
        #[arg(long, requires = "yes")]
        yes_many: bool,
        /// Largest device read in KB (default: chosen per device)
        #[arg(long, hide = true)]
        chunk_kb: Option<usize>,
        /// Read through gaps between files up to this many KB (default: chosen per device)
        #[arg(long, hide = true)]
        gap_kb: Option<u64>,
        /// Device reads in flight (default: chosen per device)
        #[arg(long, hide = true)]
        inflight: Option<usize>,
        /// Buffered reads instead of O_DIRECT (default: chosen per device)
        #[arg(long, hide = true)]
        buffered: Option<bool>,
        /// Destination writer threads
        #[arg(long, default_value_t = 4, hide = true)]
        writers: usize,
        /// Print progress as JSON lines on stdout
        #[arg(long)]
        json: bool,
    },
    /// Rewrite an EXISTING exFAT volume on TARGET into the flashcp layout (every file
    /// contiguous, all metadata together, the optimizer's cluster size) WITHOUT losing data:
    /// copies everything to a local temp dir, verifies it, rebuilds the volume from it, verifies
    /// the result, deletes the copy. Re-running after an interruption resumes.
    ///
    /// If it is interrupted (power loss, unplugged, killed) after the rebuild started, the drive
    /// does not open anywhere until optimize finishes, and other systems (notably Windows) may
    /// offer to FORMAT it: DECLINE (formatting erases the half-built volume while the only other
    /// copy is the temporary one), plug it back into this Linux computer and re-run `flashcp
    /// optimize <device>` (with the same --temp, if one was given) to resume.
    Optimize {
        /// Block device or image file (or use --target-fd)
        #[arg(required_unless_present = "target_fd", conflicts_with = "target_fd")]
        target: Option<PathBuf>,
        /// Use this inherited, already-open block-device fd (O_RDWR|O_EXCL) instead of opening
        /// TARGET, e.g. from a privileged helper
        #[arg(long, value_name = "N")]
        target_fd: Option<i32>,
        /// Directory for the temporary copy (default: the first disk-backed one with room of
        /// $FLASHCP_TMPDIR, /var/tmp, $TMPDIR or /tmp, $XDG_CACHE_HOME or ~/.cache). It must be
        /// on a disk filesystem: tmpfs, ramfs, overlayfs and unknown filesystems are refused
        /// (see --temp-volatile-i-accept-data-loss). The copy is made read-only once pulled
        #[arg(long, value_name = "DIR")]
        temp: Option<PathBuf>,
        /// Allow --temp on tmpfs, ramfs, overlayfs or a filesystem flashcp does not know to be
        /// disk-backed. DANGER: the drive is erased while its only other copy is there, so a power
        /// loss or crash during the rebuild LOSES THE STICK'S DATA
        #[arg(long = "temp-volatile-i-accept-data-loss", requires = "temp")]
        temp_volatile: bool,
        /// Only print the plan
        #[arg(long)]
        dry_run: bool,
        /// Required unless --dry-run: confirms TARGET may be rebuilt
        #[arg(long)]
        yes: bool,
        /// Rebuild even if the volume is already optimized
        #[arg(long)]
        force: bool,
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
    /// Trace-driven device simulator: replay, sweep and calibrate (see SIMULATION.md)
    Sim {
        #[command(subcommand)]
        cmd: flashcp::sim::cli::SimCmd,
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
    let mut args = std::env::args().skip(1);
    let mut cmd_name = String::new();
    while let Some(a) = args.next() {
        if a == "--trace" {
            args.next();
        } else if !a.starts_with('-') {
            cmd_name = a;
            break;
        }
    }
    // `flashcp sim` replays traces and never touches a device: it is not traced itself (its
    // recordings run flashcp in child processes, each with its own trace file).
    let traced = match (&cli.trace_out, &cli.cmd) {
        (_, Cmd::Sim { .. }) => Ok(()),
        (Some(p), _) => flashcp::sim::trace::init(p, &cmd_name),
        (None, _) => flashcp::sim::trace::init_from_env(&cmd_name),
    };
    if let Err(e) = traced {
        eprintln!("Error: {e:#}");
        std::process::exit(2);
    }
    let json = matches!(
        cli.cmd,
        Cmd::Build { json: true, .. }
            | Cmd::Add { json: true, .. }
            | Cmd::Sync { json: true, .. }
            | Cmd::Pull { json: true, .. }
            | Cmd::Optimize { json: true, .. }
            | Cmd::Info { json: true, .. }
    );
    let r = run(cli.cmd);
    flashcp::sim::trace::finish();
    if let Err(e) = r {
        if json {
            println!("{}", error_json(&format!("{e:#}"), optimize::recovery_of(&e)));
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
            let o = exfat::BuildOpts { cluster, label, fat, readers, dry_run, serial: None, job: None };
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
        Cmd::Sync {
            paths,
            target_fd,
            recover,
            to,
            delete,
            yes,
            yes_many,
            checksum,
            fast_moves,
            readers,
            dry_run,
            json,
        } => {
            let mut paths = paths.into_iter();
            if recover {
                let target = match (target_fd, paths.next(), paths.next()) {
                    (Some(fd), None, _) => TargetSpec::Fd(fd),
                    (None, Some(t), None) => TargetSpec::Path(t),
                    _ => anyhow::bail!("sync --recover takes TARGET (or --target-fd) and nothing else"),
                };
                let r = flashcp::sync::recover(&target)?;
                match flashcp::sync::recovery_note(&r) {
                    Some(n) => println!("{}", n.trim_start_matches("note: ")),
                    None => println!("no interrupted sync to complete for this volume"),
                }
                return Ok(());
            }
            let (src, target) = match (target_fd, paths.next(), paths.next(), paths.next()) {
                (Some(fd), Some(s), None, _) => (s, TargetSpec::Fd(fd)),
                (None, Some(s), Some(t), None) => (s, TargetSpec::Path(t)),
                _ => anyhow::bail!("sync takes SRC TARGET (or SRC with --target-fd)"),
            };
            let o = flashcp::SyncOpts { to, delete, checksum, readers, dry_run, yes, yes_many, fast_moves };
            let s = flashcp::sync_to(&src, &target, &o, &mut sink(json))?;
            if !dry_run && !json {
                println!(
                    "synced: {} unchanged, {} new, {} changed, {} moved, {} deleted ({} dirs); wrote {} of file data \
                     ({} in all), skipped {} in {:.2} s",
                    s.unchanged,
                    s.new,
                    s.changed,
                    s.moved,
                    s.deleted,
                    s.deleted_dirs,
                    util::human_bytes(s.data_written as f64),
                    util::human_bytes(s.written as f64),
                    util::human_bytes(s.data_skipped as f64),
                    s.secs
                );
            }
            Ok(())
        }
        Cmd::Pull {
            paths,
            target_fd,
            from,
            skip_existing,
            overwrite,
            update,
            delete_extraneous,
            yes,
            yes_many,
            chunk_kb,
            gap_kb,
            inflight,
            buffered,
            writers,
            json,
        } => {
            let (target, dest) = match (target_fd, <[PathBuf; 1]>::try_from(paths)) {
                (Some(fd), Ok([dest])) => (TargetSpec::Fd(fd), dest),
                (None, Err(p)) if p.len() == 2 => {
                    let [t, d] = <[PathBuf; 2]>::try_from(p).unwrap();
                    (TargetSpec::Path(t), d)
                }
                (Some(_), _) => anyhow::bail!("with --target-fd, give only DEST"),
                (None, _) => anyhow::bail!("usage: flashcp pull TARGET DEST (or --target-fd N DEST)"),
            };
            let o = pull::PullOpts {
                from,
                skip_existing,
                overwrite,
                update,
                delete_extraneous,
                yes,
                yes_many,
                chunk: chunk_kb.map(|c| c << 10),
                gap: gap_kb.map(|g| g << 10),
                buffered,
                inflight,
                writers,
            };
            let s = pull::pull_from(&target, &dest, &o, &mut sink(json))?;
            if !json {
                let kept = if update {
                    format!(", {} up to date", s.skipped)
                } else if s.skipped > 0 {
                    format!(", skipped {} existing", s.skipped)
                } else {
                    String::new()
                };
                let deleted = if s.deleted > 0 { format!(", deleted {} extraneous", s.deleted) } else { String::new() };
                let skipped_del = if s.skipped_deletions > 0 {
                    format!(", {} planned deletions skipped (DEST changed since planning)", s.skipped_deletions)
                } else {
                    String::new()
                };
                println!(
                    "pulled: {} files, {} dirs, {} data{kept}{deleted}{skipped_del} in {:.2}s ({:.0} files/s, {:.1} MB/s of data; read {} in {} requests)",
                    s.files,
                    s.dirs,
                    util::human_bytes(s.data_bytes as f64),
                    s.secs,
                    s.files as f64 / s.secs,
                    s.data_bytes as f64 / 1e6 / s.secs,
                    util::human_bytes(s.read_bytes as f64),
                    s.reads
                );
            }
            Ok(())
        }
        Cmd::Optimize { target, target_fd, temp, temp_volatile, dry_run, yes, force, json } => {
            let target = spec(target, target_fd);
            anyhow::ensure!(
                dry_run || yes,
                "this rebuilds the volume on {} (its files are copied off, verified and written back); nothing was \
                 changed. Pass --yes to confirm, or --dry-run to see the plan. If it is interrupted: {}",
                target.describe(),
                optimize::FORMAT_WARNING
            );
            let o = optimize::OptimizeOpts { temp, dry_run, force, temp_volatile };
            let mut human = |e: &Event| match e {
                Event::Plan { detail, .. } => println!("{detail}"),
                Event::Phase { phase, step, steps, .. } => println!("[{step}/{steps}] {phase}"),
                _ => {}
            };
            let mut js = sink(true);
            let s = optimize::optimize(&target, &o, if json { &mut js } else { &mut human })?;
            if !json && !dry_run && !s.already {
                let phases: Vec<String> = s.phases.iter().map(|(n, t)| format!("{n} {t:.2}s")).collect();
                println!(
                    "optimized{}: {} files, {} dirs, {} data, cluster {} → {}, {} written in {:.2}s ({})",
                    if s.resumed { " (resumed)" } else { "" },
                    s.files,
                    s.dirs,
                    util::human_bytes(s.data_bytes as f64),
                    util::human_bytes(s.old_cluster as f64),
                    util::human_bytes(s.cluster as f64),
                    util::human_bytes(s.written as f64),
                    s.secs,
                    phases.join(", ")
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
        Cmd::Sim { cmd } => flashcp::sim::cli::run(cmd),
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
