//! flashcp as a library: build a whole exFAT volume in one sequential pass ([`build`]), add a
//! tree to an existing exFAT volume ([`add`]), or summarize one ([`info`]). Long operations
//! report through a callback of [`events::Event`]s.

pub mod add;
pub mod bench;
pub mod copy;
pub mod dataset;
pub mod events;
pub mod exfat;
pub mod pack;
pub mod probe;
pub mod util;

pub use add::{AddOpts, AddStats, VolumeInfo, add, add_to, info, info_of};
pub use events::{Event, Op};
pub use exfat::{BuildOpts, BuildStats, FatMode, TargetSpec, build, build_to};
