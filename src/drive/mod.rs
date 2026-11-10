//! Drive mode: `flashcp mount`, a crash-ordered userspace exFAT driver behind FUSE.
//!
//! - [`core`]: the inode tree and every POSIX operation, in memory;
//! - [`stage`]: staged data (RAM, then a spill file);
//! - [`commit`]: planning a commit (copy-on-write data, relocated entry sets) and its phases;
//! - [`djournal`]: the permanent on-volume journal and recovery;
//! - [`fs`]: the thread-safe driver with the commit thread and the write window;
//! - [`dev`]: the device, with a crash-injecting one for tests.
//!
//! See `docs/drive-mode-interface.md` for the command's contract and DESIGN.md for the design.

pub(crate) mod commit;
pub mod core;
pub(crate) mod dev;
pub(crate) mod djournal;
pub mod fs;
pub(crate) mod set;
pub(crate) mod stage;
#[cfg(test)]
mod tests;

pub use self::core::{Attr, Errno, Ino, ROOT, RenameFlags, SetAttr, Statfs};
pub use fs::{Fs, FsOpts, Note, Status};

/// Why a volume goes to the kernel driver instead (exit code 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallback {
    NotExfat,
    Dirty,
    Invalid(String),
    Unsupported(String),
    TooSmall,
}

impl Fallback {
    pub fn reason(&self) -> &'static str {
        match self {
            Fallback::NotExfat => "not_exfat",
            Fallback::Dirty => "dirty",
            Fallback::Invalid(_) => "invalid",
            Fallback::Unsupported(_) => "unsupported",
            Fallback::TooSmall => "too_small",
        }
    }
}

impl std::fmt::Display for Fallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fallback::NotExfat => f.write_str("not an exFAT volume"),
            Fallback::Dirty => f.write_str("the volume is marked dirty (not cleanly unmounted by another system)"),
            Fallback::Invalid(m) => write!(f, "the volume failed validation: {m}"),
            Fallback::Unsupported(m) => write!(f, "the volume uses something flashcp does not support: {m}"),
            Fallback::TooSmall => f.write_str("the volume is too small"),
        }
    }
}

impl std::error::Error for Fallback {}
