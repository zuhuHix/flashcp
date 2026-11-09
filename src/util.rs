use anyhow::{Context, Result};
use std::ffi::CString;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// One file to transfer, relative to the source root.
#[derive(Clone, Debug)]
pub struct Entry {
    pub rel: PathBuf,
    pub size: u64,
}

/// Walk `root` and return (directories, files), both in a stable, directory-grouped order.
/// Directories come parents-first so they can be created in one pass.
pub fn scan(root: &Path) -> Result<(Vec<PathBuf>, Vec<Entry>)> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in WalkDir::new(root).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(root)?.to_path_buf();
        if e.file_type().is_dir() {
            if !rel.as_os_str().is_empty() {
                dirs.push(rel);
            }
        } else if e.file_type().is_file() {
            files.push(Entry { rel, size: e.metadata()?.len() });
        }
    }
    Ok((dirs, files))
}

/// Flush every dirty page of the filesystem containing `path` to the device.
/// Timing without this measures RAM, not the drive.
pub fn syncfs(path: &Path) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {} for syncfs", path.display()))?;
    let r = unsafe { libc::syncfs(f.as_raw_fd()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("syncfs");
    }
    Ok(())
}

/// Best-effort preallocation so the filesystem can pick one contiguous extent up front.
pub fn preallocate(f: &File, len: u64) {
    if len > 0 {
        unsafe {
            libc::fallocate(f.as_raw_fd(), 0, 0, len as libc::off_t);
        }
    }
}

pub fn fadvise_sequential(f: &File) {
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
    }
}

pub fn statfs_free(path: &Path) -> Result<u64> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

pub fn human_bytes(b: f64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

/// Deterministic, fast PRNG (SplitMix64) so datasets are reproducible.
pub struct Rng(pub u64);
#[allow(clippy::should_implement_trait)]
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

/// Page-aligned buffer for O_DIRECT I/O.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    layout: std::alloc::Layout,
}
#[allow(clippy::should_implement_trait)]
impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        AlignedBuf { ptr, len, layout }
    }
    pub fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
    pub fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}
