//! répertoire's platform-neutral core: the scanner, the in-memory tree,
//! applying change events to it, queries, access control and snapshots.
//!
//! Nothing here knows about fanotify: event sources hand in [`RawEvent`]s
//! and the tree re-stats what they point at.

pub mod access;
pub mod apply;
pub mod debounce;
pub mod mounts;
pub mod query;
pub mod scan;
pub mod snapshot;
pub mod sys;
pub mod tree;

pub use access::{Viewer, Visibility};
pub use apply::{ByPath, Ctx, DirAccess, Event, Outcome, RawEvent};
pub use mounts::Filesystem;
pub use query::{Pruned, Rank, Sort};
pub use scan::{Excludes, ScanDir, Scanner};
pub use tree::{Id, Kind, Tree, NONE, ROOT};
