//! The item trigger: how the graph learns that a vector arrived or left.
//!
//! A vector is one arcus KV item whose key the daemon recognises
//! (`arcus_trig{index}:<id>`), so the item *is* the notification. Replicating
//! it to a replica, replaying it from the persistence log, and an ordinary
//! client write all reach `do_item_link`, which is why one callback covers all
//! three and this crate needs no channel, no queue and no thread of its own.
//!
//! This replaces the delta channel described in
//! `docs/superpowers/specs/2026-09-04-arcvector-index-replication-design.md`.

pub mod event;
pub mod key;
pub mod recover;
pub mod waiting;
