pub mod sweep;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::handler::arcus::engine::Store;
use crate::handler::registry::{self, VectorIndex};

/// The registry is the whole answer.
///
/// An index exists in this process exactly when its metadata item has been
/// linked and the trigger callback registered it -- by `vcreate` on this node,
/// by arcus replicating the item in, or by the persistence log replaying it at
/// startup. All three reach `do_item_link`, so there is nothing left for a
/// command to read back from the engine: no ownership token to compare against
/// a Map, and no cold graph to rebuild, because what a search sees is what the
/// callback put there.
///
/// That is why this takes a `Store` it does not use. Every caller has one, and
/// keeping it in the signature means the storage layer can come back into this
/// decision without touching them again.
pub(super) fn resolve(_store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    registry::get(name)
        .filter(|index| index.state() != registry::BUILDING)
        .ok_or(Error::NoSuchIndex)
}

pub(super) fn for_read(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    resolve(store, name)
}

pub(super) fn for_write(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    resolve(store, name)
}

/// Drops a registry entry whose backing storage turned out to be gone.
///
/// Reached when a command finds the engine has no item where the index says
/// there is one. `remove_if_stale` is what keeps this from racing a `vcreate`
/// that registered the same name after `stamp` was taken.
pub(super) fn map_is_gone(name: &str, stamp: u64) {
    if registry::remove_if_stale(name, stamp) {
        eprintln!("ArcVector: index '{name}' is no longer in the engine; releasing its graph");
    }
}
