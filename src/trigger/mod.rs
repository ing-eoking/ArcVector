//! The item trigger: how the graph learns that a vector arrived or left.
//!
//! A vector is one arcus KV item whose key the daemon recognises, so the item
//! *is* the notification. Replication, persistence recovery and an ordinary
//! client write all reach `do_item_link`, which is why one callback covers all
//! three and no channel of our own is needed.

pub mod key;
