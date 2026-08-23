//! Every command starts with the Map's metadata element; with `cfg(recovery)` its `owner` token also decides whether this node's graph is the one to serve.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, MetaRecord};
use crate::handler::arcus::engine::Store;
#[cfg(recovery)]
use crate::handler::arcus::engine::StoreError;
use crate::handler::meta::{MetaState, read_metadata};
#[cfg(recovery)]
use crate::handler::recovery;
use crate::handler::registry::{self, VectorIndex};

#[cfg(recovery)]
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    // One lookup, and it comes first — not because it decides anything, but because it is the
    // note of which entry this call may end up releasing. See `map_is_gone`. Deciding is the
    // metadata's job, below.
    let held = registry::get(name);
    let (meta, layout) = usable_metadata(store, name, held.as_deref())?;

    let (index, fresh) = match held {
        Some(index) => (index, false),
        // Registered while the metadata was being read, if anyone did: `empty_graph` inserts
        // through `insert_or_get`, so the winner is whichever landed first and `fresh` follows.
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

/// Nothing outlives the graph in this build, so the registry holds every index there is — but
/// the Map under the name can still expire, be evicted, or be replaced by a Map that is not
/// ours, and the registry cannot see any of that. The metadata read is what does.
#[cfg(not(recovery))]
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    // First, and once: this is the note of what a release below may take, and it is also the
    // index this call resolves to. What it is *not* is the judgement — that is the metadata's,
    // on the next line. See `map_is_gone` for why the note has to precede the read.
    let held = registry::get(name);
    let (meta, _) = usable_metadata(store, name, held.as_deref())?;

    let index = match held {
        Some(index) => index,
        // The note above was taken before the metadata read, and a `vcreate` can have finished
        // in between — registry and Map both, under one hold. So look again before concluding
        // anything: a different question from the note, and only asked when the note was empty.
        // The read lock is the same one `vcreate` holds across its engine write, so this sees
        // either the whole create or none of it.
        None => match registry::get(name) {
            Some(index) => index,
            // A Map that is an index, with no graph serving it, in a build that cannot build
            // one. It answers nothing and `vcreate` will not take the name while its metadata
            // element stands, so the name is bricked until somebody runs `vdrop` by hand.
            // Nothing reachable produces this — the two races that used to are closed — so it
            // means a bug, and leaving a dead name behind is the worse half of that. Delete
            // it, loudly, and let the name be usable again.
            None => {
                eprintln!(
                    "ArcVector: '{name}' has an index Map with no graph, which this build \
                     cannot rebuild; deleting the Map so the name can be used again"
                );
                let _ = store.drop_map(name);
                return Err(Error::NoSuchIndex);
            }
        },
    };
    if meta.owner != index.owner() {
        // Identity, not name: see `map_is_gone`.
        // Nothing in this build can produce a second index at one name — no transfer, no
        // restore, and `vcreate` releases a stale graph before it registers. So this is the
        // branch that should never run, kept because the guarantee it rests on is a build
        // configuration rather than anything the code enforces. There is nothing to rebuild
        // from, so let the graph go and answer as if the name were free.
        registry::remove_observed(name, &index);
        return Err(Error::NoSuchIndex);
    }
    Ok(index)
}

/// The metadata element, or the reason there is none — and, where the reason is definitive,
/// the release of a graph that has nothing left to serve.
///
/// `held` is what the registry had before this read: the only entry a verdict from it may
/// release. One that appears afterwards describes a Map this read never saw.
fn usable_metadata(
    store: &Store,
    name: &str,
    held: Option<&VectorIndex>,
) -> Result<(MetaRecord, Layout)> {
    match read_metadata(store, name) {
        MetaState::Usable(meta, layout) => Ok((meta, layout)),
        // Expired, evicted, or dropped by another node.
        MetaState::NoMap => {
            if let Some(index) = held {
                map_is_gone(name, index);
            }
            Err(Error::NoSuchIndex)
        }
        MetaState::Damaged(why) => {
            discard_damaged(store, name, &why, held);
            Err(Error::NoSuchIndex)
        }
        // Proves nothing about the name, so nothing is released on it — and the client hears
        // what actually happened. Answering NOT_FOUND here would report a working index as
        // missing every time the engine hiccups, and an ABI mismatch would say the same.
        MetaState::Unknown(e) => Err(e.into()),
    }
}

/// What to do about a Map whose metadata will not read.
///
/// The Map is there — `NoMap` was already ruled out — so the question is only whose it is.
#[cfg(recovery)]
fn discard_damaged(store: &Store, name: &str, why: &str, observed: Option<&VectorIndex>) {
    match store.probe_map(name) {
        // Gone between the metadata read and this probe. Nothing to delete.
        Err(StoreError::KeyGone) => {
            if let Some(index) = observed {
                map_is_gone(name, index);
            }
        }
        // Any other engine trouble proves nothing about whether the Map is there.
        Err(_) => {}
        // Somebody else's Map is none of our business — but the graph under that name is
        // ours, and it has nothing left to serve.
        Ok(probe) if !probe.looks_like_index() => {
            if observed.is_some_and(|index| registry::remove_observed(name, index)) {
                eprintln!(
                    "ArcVector: '{name}' is not our Map ({why}); releasing the graph that used to be there"
                );
            }
        }
        // Ours, present, and unreadable — the one failure that deletes.
        Ok(_) => {
            if crate::handler::arcus::abi::mismatched() {
                // "Damaged" is unreliable under a misaligned vtable, and deleting on it would destroy an index over a build mistake.
                return;
            }
            eprintln!(
                "ArcVector: index '{name}' has unusable metadata ({why}); deleting the Map, \
                 which cannot become an index again without it"
            );
            let _ = store.drop_map(name);
            if let Some(index) = observed {
                registry::remove_observed(name, index);
            }
        }
    }
}

/// A Map that is not an index. Never deleted here: no build deletes a Map it did not make, and
/// this one cannot even tell whether it made it — that is what the metadata would have said.
#[cfg(not(recovery))]
fn discard_damaged(_store: &Store, name: &str, why: &str, observed: Option<&VectorIndex>) {
    if observed.is_some_and(|index| registry::remove_observed(name, index)) {
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
    let ann = recovery::metadata::build_ann(meta, layout)?;
    // Nothing to undo if the registry cannot take it: the Map is not ours to remove — we are
    // adopting one that was already there — and the graph goes out of scope unregistered.
    registry::insert_or_get(VectorIndex::new(
        name.to_owned(),
        ann,
        probe.maxcount.saturating_sub(1),
        registry::REBUILDING,
    ))
    .map_err(|e| Error::Index(format!("the index registry could not grow: {e}")))
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

/// The Map is gone, so the graph built from it has nothing left to serve.
///
/// Dropping the registry entry drops the last `Arc`, and with it usearch's graph and the id
/// mapping. A command already holding one frees it when it finishes.
///
/// Not gated on `recovery`: a Map can expire or be evicted in any build, and the graph must not
/// outlive it.
///
/// Only ever called off the back of an engine call that already proved the Map is not there —
/// `resolve`'s metadata read, a `KeyGone` from a command's own element call, or a `vcreate`
/// whose insert reports it created the Map. No build pays an extra probe to find this out, and
/// nothing here runs on a guess: a probe that merely failed could have failed for a transient
/// reason, and releasing the graph on that would throw away a working index.
///
/// `observed` is the entry the caller was working with, and the only one this releases. The
/// verdict was reached before the lock, so by now the name can hold an index some `vcreate`
/// registered in between — one built for a Map that exists. Releasing by name would take that
/// one out, leaving a Map nothing serves and, in a build that cannot rebuild, no way back.
pub(super) fn map_is_gone(name: &str, observed: &VectorIndex) {
    if registry::remove_observed(name, observed) {
        eprintln!("ArcVector: index '{name}' has no Map; releasing the graph it was built from");
    }
}
