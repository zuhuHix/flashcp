//! Pack mode: the whole tree becomes one sequential stream (tar), optionally zstd-compressed.
//! `Compress::Auto` samples files spread across the tree and only compresses when it pays.

use crate::util::{scan, syncfs};
use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Compress {
    None,
    Zstd,
    Auto,
}

pub struct PackStats {
    pub files: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub secs: f64,
    pub compressed: bool,
    pub sample_ratio: Option<f64>,
}

/// Compress up to ~16 MB sampled evenly across the file list; return compressed/original.
pub fn sample_ratio(src: &Path) -> Result<f64> {
    let (_, files) = scan(src)?;
    let mut raw = Vec::new();
    let step = (files.len() / 64).max(1);
    for e in files.iter().step_by(step) {
        let mut f = File::open(src.join(&e.rel))?;
        let mut chunk = vec![0u8; (e.size as usize).min(256 << 10)];
        f.read_exact(&mut chunk)?;
        raw.extend_from_slice(&chunk);
        if raw.len() > 16 << 20 {
            break;
        }
    }
    if raw.is_empty() {
        return Ok(1.0);
    }
    let c = zstd::bulk::compress(&raw, 1)?;
    Ok(c.len() as f64 / raw.len() as f64)
}

pub fn pack(src: &Path, out_path: &Path, mode: Compress) -> Result<PackStats> {
    let t = Instant::now();
    let (sample, compress) = match mode {
        Compress::None => (None, false),
        Compress::Zstd => (None, true),
        Compress::Auto => {
            let r = sample_ratio(src)?;
            // Below ~0.85 the saved device bytes beat the CPU cost on any USB link.
            (Some(r), r < 0.85)
        }
    };
    let (_, files) = scan(src)?;
    let out = BufWriter::with_capacity(8 << 20, File::create(out_path)?);
    let bytes_in: u64 = files.iter().map(|e| e.size).sum();

    fn write_tar<W: Write>(src: &Path, files: &[crate::util::Entry], w: W) -> Result<W> {
        let mut b = tar::Builder::new(w);
        b.mode(tar::HeaderMode::Deterministic);
        for e in files {
            b.append_path_with_name(src.join(&e.rel), &e.rel)?;
        }
        Ok(b.into_inner()?)
    }

    let file = if compress {
        let mut enc = zstd::stream::Encoder::new(out, 1)?;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as u32;
        enc.multithread(threads.min(8))?;
        let enc = write_tar(src, &files, enc)?;
        enc.finish()?.into_inner().map_err(|e| e.into_error())?
    } else {
        write_tar(src, &files, out)?.into_inner().map_err(|e| e.into_error())?
    };
    let bytes_out = file.metadata()?.len();
    drop(file);
    syncfs(out_path.parent().unwrap_or(Path::new(".")))?;
    Ok(PackStats {
        files: files.len(),
        bytes_in,
        bytes_out,
        secs: t.elapsed().as_secs_f64(),
        compressed: compress,
        sample_ratio: sample,
    })
}
