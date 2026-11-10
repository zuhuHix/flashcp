//! flashcp as a library: build a whole exFAT volume in one sequential pass ([`build`]), add a
//! tree to an existing exFAT volume ([`add`]), make a directory of one match a tree ([`sync`]),
//! copy files off one ([`pull`]), or summarize one ([`info`]). Long operations report through a
//! callback of [`events::Event`]s.

pub mod add;
pub(crate) mod add_build;
pub mod bench;
pub mod copy;
pub mod dataset;
pub mod devprofile;
pub(crate) mod dirfd;
pub mod events;
pub mod exfat;
pub(crate) mod journal;
pub mod optimize;
pub mod pack;
pub mod probe;
pub mod pull;
pub mod sync;
pub mod util;
pub(crate) mod verify;
pub(crate) mod volume;

pub use add::{AddOpts, AddStats, VolumeInfo, add, add_to, info, info_of};
pub use events::{Event, Op};
pub use exfat::{BuildOpts, BuildStats, FatMode, TargetSpec, build, build_to};
pub use pull::{PullOpts, PullStats, pull, pull_from};
pub use sync::{SyncOpts, SyncStats, sync, sync_to};
pub mod drive;
pub mod sim;
