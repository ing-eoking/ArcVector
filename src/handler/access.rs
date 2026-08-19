//! With `cfg(recovery)` the Map's `owner` token decides and a stale graph is rebuilt; without it the registry is the whole answer.

use std::sync::Arc;

use crate::error::{Error, Result};
#[cfg(recovery)]
use crate::handler::arcus::element::{Layout, MetaRecord};
use crate::handler::arcus::engine::Store;
#[cfg(recovery)]
use crate::handler::recovery::{self, metadata::MetaState};
use crate::handler::registry::{self, VectorIndex};

#[cfg(recovery)]
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let (meta, layout) = usable_metadata(store, name)?;

    let (index, fresh) = match registry::get(name) {
        Some(index) => (index, false),
        None => empty_graph(store, name, &meta, layout)?,
    };

    if fresh || meta.owner != index.owner() {
        recovery::ensure_builder();
        recovery::take_over(store, &index)?;
        return Ok(index);
    }

    if index.is_rebuilding() && index.is_refilled() {
        // Only a worker has the connection an engine write needs.
        recovery::claim_refilled(store, &index)?;
    }
    Ok(index)
}

#[cfg(recovery)]
fn usable_metadata(store: &Store, name: &str) -> Result<(MetaRecord, Layout)> {
    match recovery::metadata::read_metadata(store, name) {
        MetaState::Usable(meta, layout) => Ok((meta, layout)),
        MetaState::Damaged(why) => {
            discard_damaged(store, name, &why);
            Err(Error::NoSuchIndex)
        }
    }
}

/// Delete a Map whose metadata cannot be read — the one failure that deletes.
#[cfg(recovery)]
fn discard_damaged(store: &Store, name: &str, why: &str) {
    if !store.probe_map(name).is_ok_and(|p| p.looks_like_index()) {
        return; // Not ours. Somebody else's Map is none of our business.
    }
    if crate::handler::arcus::abi::mismatched() {
        // "Damaged" is unreliable under a misaligned vtable, and deleting on it would destroy an index over a build mistake.
        return;
    }
    eprintln!(
        "ArcVector: index '{name}' has unusable metadata ({why}); deleting the Map, \
         which cannot become an index again without it"
    );
    let _ = store.drop_map(name);
    registry::remove(name);
}

#[cfg(recovery)]
fn empty_graph(
    store: &Store,
    name: &str,
    meta: &MetaRecord,
    layout: Layout,
) -> Result<(Arc<VectorIndex>, bool)> {
    let probe = store.probe_map(name)?;
    let ann = recovery::metadata::build_ann(meta, layout)?;
    Ok(registry::insert_or_get(VectorIndex::new(
        name.to_owned(),
        ann,
        probe.maxcount.saturating_sub(1),
        registry::REBUILDING,
    )))
}

/// Nothing outlives the graph in this build, so an unknown name does not exist; a leftover Map waits for `vdrop`.
#[cfg(not(recovery))]
pub(super) fn resolve(_store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    registry::get(name).ok_or(Error::NoSuchIndex)
}

pub(super) fn for_read(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let index = resolve(store, name)?;
    #[cfg(recovery)]
    if index.is_rebuilding() {
        // usearch has no notion of "not ready": a half-filled index answers without complaint.
        return Err(Error::Unreadable);
    }
    Ok(index)
}

/// Writes go through during a rebuild: Map takes them first, and the refill cannot replay an older value over them.
pub(super) fn for_write(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    resolve(store, name)
}
