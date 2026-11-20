//! Instant filename search index (see `docs/ARCHITECTURE.md`, M3/M4).

#![warn(missing_docs)]

pub mod arena;
pub mod r#gen;
pub mod query;
pub mod rank;
pub mod snapshot;
#[cfg(windows)]
pub mod win;

pub use arena::{EntryId, Index, Meta};
pub use query::{CancelToken, EntryKind, Query, Results, SearchOptions, parse_query};
pub use snapshot::SnapshotError;
