//! Which graph a command may use, and what to do when the answer is "not this
//! one".
//!
//! Two builds of this module. With `cfg(recovery)` — the server replicates or
//! persists, so a Map can outlive the graph — the Map's `owner` token decides,
//! and a graph that is missing or stale is rebuilt. Without it nothing can
//! outlive the graph, so the registry is the whole answer.
//!
//! `docs/내부구조.md` §6.

use std::sync::Arc;

use crate::error::{Error, Result};
#[cfg(recovery)]
use crate::handler::arcus::element::{Layout, MetaRecord};
use crate::handler::arcus::engine::Store;
#[cfg(recovery)]
use crate::handler::registry::MetaState;
use crate::handler::registry::{self, VectorIndex};

#[cfg(recovery)]
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
#[cfg(recovery)]
fn usable_metadata(store: &Store, name: &str) -> Result<(MetaRecord, Layout)> {
    match registry::read_metadata(store, name) {
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
#[cfg(recovery)]
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

/// Resolve a name to its graph.
///
/// Nothing can outlive the graph in this build, so a name the registry does not
/// know is a name that does not exist. The Map may still be there — dropped from
/// the registry by a `vdrop` race, or left by an earlier build that could recover
/// — and it stays untouched: an operator clears it with `vdrop` and creates the
/// index again.
#[cfg(not(recovery))]
pub(super) fn resolve(_store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    registry::get(name).ok_or(Error::NoSuchIndex)
}

/// The graph, for a command that reads it.
pub(super) fn for_read(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let index = resolve(store, name)?;
    #[cfg(recovery)]
    if index.is_rebuilding() {
        // usearch has no notion of "not ready": a search against a half-filled
        // index answers without complaint, from whatever happens to be in.
        return Err(Error::Unreadable);
    }
    Ok(index)
}

/// The graph, for a command that writes it.
///
/// Writes go through while a rebuild runs. Refusing them would be an outage
/// lasting as long as the rebuild, and they are safe: Map takes the write first,
/// and `AnnIndex` keeps the rebuild from replaying an older value over it.
pub(super) fn for_write(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    resolve(store, name)
}
