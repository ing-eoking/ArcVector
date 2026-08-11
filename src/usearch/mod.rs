//! Searching with usearch.
//!
//! - [`index`] wraps the library: capacity growth, and the `String` ↔ `u64` key
//!   mapping it needs because usearch keys are integers.
//! - [`metric`] is the distance measure, and which quantizations it is meaningful
//!   on.
//! - [`threads`] holds the invariant that keeps usearch's fixed context pool from
//!   running dry, which is the one concurrency rule that cannot be got wrong.
//!
//! **This side is a cache.** Everything in it is rebuildable from
//! [`crate::arcus`], which is what makes restart, eviction, TTL expiry and
//! replication slaves a single case rather than four.

pub mod index;
pub mod metric;
pub mod threads;

pub use index::AnnIndex;
pub use metric::Metric;
pub use threads::THREAD_SLOTS;
