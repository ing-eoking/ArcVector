//! Which graph a command may use, and what to do when the answer is "not this
//! one".
//!
//! The Map's `owner` token is the whole test — `docs/내부구조.md` §6.

use std::sync::Arc;

use crate::arcus::element::{Layout, MetaRecord};
use crate::arcus::engine::Store;
use crate::error::{Error, Result};
use crate::registry::{self, MetaState, VectorIndex};

/// Resolve a name to its graph, recovering it when missing or stale.
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let (meta, layout) = usable_metadata(store, name)?;

    let (index, fresh) = match registry::get(name) {
        Some(index) => (index, false),
        None => empty_graph(store, name, &meta, layout)?,
    };

    if fresh || meta.owner != index.owner() {
        registry::ensure_builder();
        registry::take_over(store, &index)?;
        return Ok(index);
    }

    if index.is_rebuilding() && index.is_refilled() {
        // Only a worker has the connection an engine write needs.
        registry::claim_refilled(store, &index)?;
    }
    Ok(index)
}

/// The metadata element, or the reason there is no index to serve.
fn usable_metadata(store: &Store, name: &str) -> Result<(MetaRecord, Layout)> {
    match registry::read_metadata(store, name) {
        MetaState::Usable(meta, layout) => Ok((meta, layout)),
        // A newer node's format. Not damage, so nothing is touched.
        MetaState::Newer => Err(Error::NoSuchIndex),
        MetaState::Damaged(why) => {
            discard_damaged(store, name, &why);
            Err(Error::NoSuchIndex)
        }
    }
}

/// Delete a Map whose metadata cannot be read — the one failure that deletes.
fn discard_damaged(store: &Store, name: &str, why: &str) {
    if !store.probe_map(name).is_ok_and(|p| p.looks_like_index()) {
        return; // Not ours. Somebody else's Map is none of our business.
    }
    if crate::arcus::abi::mismatched() {
        // "Damaged" is the engine's word, and a misaligned vtable makes it
        // unreliable. Deleting on it would destroy an index over a build mistake.
        return;
    }
    eprintln!(
        "ArcVector: index '{name}' has unusable metadata ({why}); deleting the Map, \
         which cannot become an index again without it"
    );
    let _ = store.drop_map(name);
    registry::remove(name);
}

/// Register an empty graph, and say whether this call is the one that did.
fn empty_graph(
    store: &Store,
    name: &str,
    meta: &MetaRecord,
    layout: Layout,
) -> Result<(Arc<VectorIndex>, bool)> {
    let probe = store.probe_map(name)?;
    let ann = registry::build_ann(meta, layout)?;
    Ok(registry::insert_or_get(VectorIndex::new(
        name.to_owned(),
        ann,
        probe.maxcount.saturating_sub(1),
        registry::REBUILDING,
    )))
}

/// The graph, for a command that reads it.
pub(super) fn for_read(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let index = resolve(store, name)?;
    if index.is_rebuilding() {
        return Err(Error::Unreadable);
    }
    Ok(index)
}

/// The graph, for a command that writes it.
pub(super) fn for_write(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    resolve(store, name)
}
