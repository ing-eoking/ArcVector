//! The element trigger: how the graph learns that a vector arrived or left.
//!
//! An index is one arcus Map, keyed by the index name and created with the
//! event flag, and a vector is one element of it, so the element
//! *is* the notification. Replicating it to a replica, replaying it from the
//! persistence log, and an ordinary client write all reach the Map's element
//! insert, which is why one callback covers all three and this crate needs no
//! channel of its own. The callback only writes the address down; the pool
//! puts it in the graph, outside the engine's cache lock.
//!
//! This replaces the delta channel described in
//! `docs/superpowers/specs/2026-09-04-arcvector-index-replication-design.md`.

pub mod event;
pub mod key;
pub mod recover;
pub mod waiting;
