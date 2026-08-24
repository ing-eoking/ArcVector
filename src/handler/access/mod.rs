pub mod meta;
pub mod sweep;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, MetaRecord};
use crate::handler::arcus::engine::Store;
#[cfg(recovery)]
use crate::handler::arcus::engine::StoreError;
#[cfg(recovery)]
use crate::handler::recovery;
use crate::handler::registry::{self, VectorIndex};
use meta::{MetaState, read_metadata};

#[cfg(recovery)]
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let stamp = registry::now();
    let (meta, layout) = usable_metadata(store, name, stamp)?;

    let (index, fresh) = match registry::get(name) {
        Some(index) if index.state() == registry::BUILDING => return Err(Error::NoSuchIndex),
        Some(index) => (index, false),

        None => empty_graph(store, name, &meta, layout)?,
    };

    if fresh || meta.owner != index.stamped_as() {
        recovery::ensure_builder();
        recovery::drain(store, &index)?;
        return Ok(index);
    }

    if index.state() == registry::FILLING && index.is_refilled() {
        recovery::claim_refilled(store, &index)?;
    }
    Ok(index)
}

#[cfg(not(recovery))]
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let stamp = registry::now();
    let (meta, _) = usable_metadata(store, name, stamp)?;

    let Some(index) = registry::get(name).filter(|i| i.state() != registry::BUILDING) else {
        eprintln!(
            "ArcVector: '{name}' has an index Map with no graph, which this build cannot \
             rebuild; deleting the Map so the name can be used again"
        );
        let _ = store.drop_map(name);
        return Err(Error::NoSuchIndex);
    };
    if meta.owner != index.stamped_as() {
        registry::remove_observed(name, &index);
        return Err(Error::NoSuchIndex);
    }
    Ok(index)
}

fn usable_metadata(store: &Store, name: &str, stamp: u64) -> Result<(MetaRecord, Layout)> {
    match read_metadata(store, name) {
        MetaState::Usable(meta, layout) => Ok((meta, layout)),

        MetaState::NoMap => {
            map_is_gone(name, stamp);
            Err(Error::NoSuchIndex)
        }
        MetaState::Damaged(why) => {
            discard_damaged(store, name, &why, stamp);
            Err(Error::NoSuchIndex)
        }

        MetaState::Unknown(e) => Err(e.into()),
    }
}

#[cfg(recovery)]
fn discard_damaged(store: &Store, name: &str, why: &str, stamp: u64) {
    match store.probe_map(name) {
        Err(StoreError::KeyGone) => map_is_gone(name, stamp),

        Err(_) => {}

        Ok(probe) if !probe.looks_like_index() => {
            if registry::remove_if_stale(name, stamp) {
                eprintln!(
                    "ArcVector: '{name}' is not our Map ({why}); releasing the graph that used to be there"
                );
            }
        }

        Ok(_) => {
            if crate::handler::arcus::abi::mismatched() {
                return;
            }
            eprintln!(
                "ArcVector: index '{name}' has unusable metadata ({why}); deleting the Map, \
                 which cannot become an index again without it"
            );
            let _ = store.drop_map(name);
            registry::remove_if_stale(name, stamp);
        }
    }
}

#[cfg(not(recovery))]
fn discard_damaged(_store: &Store, name: &str, why: &str, stamp: u64) {
    if registry::remove_if_stale(name, stamp) {
        eprintln!(
            "ArcVector: '{name}' is no longer our Map ({why}); releasing the graph it was built from"
        );
    }
}

#[cfg(recovery)]
fn empty_graph(
    store: &Store,
    name: &str,
    meta: &MetaRecord,
    layout: Layout,
) -> Result<(Arc<VectorIndex>, bool)> {
    let probe = store.probe_map(name)?;
    let ann = recovery::build_ann(meta, layout)?;

    registry::insert_or_get(VectorIndex::rebuilding(
        name.to_owned(),
        ann,
        probe.maxcount.saturating_sub(1),
    ))
    .map_err(|e| Error::Index(format!("the index registry could not grow: {e}")))
}

#[cfg(recovery)]
const SMALL_REBUILD: usize = 256;

#[cfg(recovery)]
const REBUILD_WAIT_CAP: std::time::Duration = std::time::Duration::from_millis(200);

#[cfg(recovery)]
const WAITING_WORKERS: usize = 1;

pub(super) fn for_read(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let index = resolve(store, name)?;
    #[cfg(recovery)]
    {
        if index.state() == registry::DRAINING {
            return Err(Error::Rebuilding);
        }
        if index.state() == registry::COLD {
            recovery::fill(store, &index)?;
        }
        if index.state() == registry::FILLING {
            if index.rebuild_size() > SMALL_REBUILD
                || !index.await_refill(REBUILD_WAIT_CAP, WAITING_WORKERS)
            {
                return Err(Error::Unreadable);
            }
            recovery::claim_refilled(store, &index)?;
        }
    }
    Ok(index)
}

pub(super) fn for_write(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let index = resolve(store, name)?;
    #[cfg(recovery)]
    if index.state() == registry::DRAINING {
        return Err(Error::Rebuilding);
    }
    Ok(index)
}

pub(super) fn map_is_gone(name: &str, stamp: u64) {
    if registry::remove_if_stale(name, stamp) {
        eprintln!("ArcVector: index '{name}' has no Map; releasing the graph it was built from");
    }
}
