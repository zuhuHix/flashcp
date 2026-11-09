//! Deterministic test datasets. Half the content is text-like (compresses well),
//! half is random (like JPG/MP4/ZIP, does not compress).

use crate::util::Rng;
use anyhow::{Result, bail};
use std::fs;
use std::io::Write;
use std::path::Path;

const WORDS: &[&str] = &[
    "the", "flash", "block", "erase", "sector", "file", "copy", "fast", "data", "write",
    "cluster", "table", "entry", "usb", "stick", "rust", "fn", "let", "mut", "struct",
    "impl", "return", "config", "value", "index", "{", "}", "(", ")", ";", "\n",
];

fn fill_text(rng: &mut Rng, buf: &mut Vec<u8>, len: usize) {
    buf.clear();
    while buf.len() < len {
        buf.extend_from_slice(WORDS[(rng.next() % WORDS.len() as u64) as usize].as_bytes());
        buf.push(b' ');
    }
    buf.truncate(len);
}

pub struct Spec {
    /// (count, min_size, max_size)
    pub groups: Vec<(usize, u64, u64)>,
    pub files_per_dir: usize,
}

pub fn spec(profile: &str) -> Result<Spec> {
    Ok(match profile {
        // Source tree, node_modules, photo thumbnails...: the classic USB killer.
        "small" => Spec { groups: vec![(20_000, 512, 16 * 1024)], files_per_dir: 100 },
        // A realistic "copy my Documents folder".
        "mixed" => Spec {
            groups: vec![
                (8_000, 512, 32 * 1024),
                (400, 100 * 1024, 4 * 1024 * 1024),
                (4, 64 * 1024 * 1024, 128 * 1024 * 1024),
            ],
            files_per_dir: 80,
        },
        // Control group: big files, where every method should be about equal.
        "large" => Spec { groups: vec![(4, 256 * 1024 * 1024, 256 * 1024 * 1024)], files_per_dir: 4 },
        // node_modules-like: 100k tiny files.
        "many" => Spec { groups: vec![(100_000, 64, 8 * 1024)], files_per_dir: 250 },
        "tiny" => Spec { groups: vec![(300, 100, 8 * 1024), (3, 1 << 20, 3 << 20)], files_per_dir: 50 },
        _ => bail!("unknown profile '{profile}' (small | mixed | large | tiny)"),
    })
}

pub fn generate(dir: &Path, profile: &str, seed: u64) -> Result<()> {
    let spec = spec(profile)?;
    if dir.exists() && fs::read_dir(dir)?.next().is_some() {
        bail!("{} already exists and is not empty", dir.display());
    }
    let mut rng = Rng(seed);
    let mut idx = 0usize;
    let mut total = 0u64;
    let mut buf = Vec::new();
    for (count, lo, hi) in &spec.groups {
        for _ in 0..*count {
            let d = dir.join(format!("d{:04}", idx / spec.files_per_dir));
            if idx % spec.files_per_dir == 0 {
                fs::create_dir_all(&d)?;
            }
            let size = rng.range(*lo, *hi) as usize;
            let compressible = rng.next() % 2 == 0;
            let name = if compressible { format!("f{idx:06}.txt") } else { format!("f{idx:06}.bin") };
            let mut f = fs::File::create(d.join(name))?;
            let mut left = size;
            while left > 0 {
                let n = left.min(8 << 20);
                if compressible {
                    fill_text(&mut rng, &mut buf, n);
                } else {
                    buf.resize(n, 0);
                    rng.fill(&mut buf);
                }
                f.write_all(&buf)?;
                left -= n;
            }
            total += size as u64;
            idx += 1;
        }
    }
    println!("generated {idx} files, {} in {}", crate::util::human_bytes(total as f64), dir.display());
    Ok(())
}
