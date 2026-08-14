//! Searching with usearch — a cache of the [`crate::arcus`] side.
//!
//! Everything here is rebuildable from Map, which makes restart, eviction, TTL
//! expiry and replication a single case rather than four.

pub mod idmap;
pub mod index;
pub mod metric;
pub mod threads;

pub use index::AnnIndex;
pub use metric::Metric;
pub use threads::THREAD_SLOTS;
