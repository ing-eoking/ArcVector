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

    // Metadata is read on first touch only -- but only in a build that has
    // something to hand the per-command owner comparison to. See
    // `map_was_taken_over` below, which is that comparison and is a constant
    // `false` under `replication` and a real `AV META` read without it.
    let (index, needs_drain) = match registry::get(name) {
        Some(index) if index.state() == registry::BUILDING => return Err(Error::NoSuchIndex),
        Some(index) => {
            let taken = map_was_taken_over(store, name, stamp, &index)?;
            (index, taken)
        }

        None => {
            let (meta, layout) = usable_metadata(store, name, stamp)?;
            // Taking an index over begins by writing the ownership token. On a
            // replica `stamp` sees the cached replication role and returns
            // without writing, so the replica can build the graph and serve
            // reads.
            recovery::stamp(store, name, crate::owner::NOBODY)?;
            let owner = meta.owner.clone();
            let (index, fresh) = empty_graph(store, name, &meta, layout)?;
            let taken = owner != index.stamped_as();
            (index, fresh || taken)
        }
    };

    if needs_drain {
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
    // Unlike the `cfg(recovery)` variant above, this build has neither a
    // replication stream nor a sweeper-driven owner check to hand the
    // per-command comparison to -- there is no other node and nothing
    // survives a restart to disagree with, but the `owned_by` check below is
    // still the only thing in this build that would ever detect a mismatch,
    // so it stays on every command rather than moving to first touch only.
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
    if !index.owned_by(&meta.owner) {
        registry::remove_observed(name, &index);
        return Err(Error::NoSuchIndex);
    }
    Ok(index)
}

/// Whether another node's graph has written this Map since the index this
/// process holds under `name` was registered.
///
/// This is the one job of the four the per-command `AV META` read used to do
/// that had no cheaper home (design section 4.5). Where it lives depends on
/// what the build actually has, and the answer is not the same for both
/// `cfg(recovery)` configurations:
///
/// * **`replication`.** Free, and no engine call at all: a node that was a
///   replica through another node's term has been applying that node's deltas
///   all along, so the stream reports the takeover directly. This is what makes
///   a command against a registered index cost nothing extra. What it does not
///   catch is recorded in the design and unchanged here: `access::sweep` (and,
///   on a replica, `repl::slave::converge`) reconcile by element count and Map
///   presence only, so an A -> B -> A takeover that leaves the count equal --
///   a delete immediately followed by an insert -- is invisible once the index
///   is registered.
///
/// * **`persistence` only.** `cfg(recovery)` is on, so a Map can outlive this
///   process's graph and be rebuilt from -- but there is no stream, and nothing
///   else in this build ever reads `AV META` for a registered index. Handing
///   the comparison to a replacement that does not exist would mean this
///   configuration silently lost a check and gained nothing, so it keeps
///   reading the owner on every command, exactly as it did before the
///   first-touch narrowing. `owned_by` rather than `stamped_as` because this is
///   back on the request hot path, and it answers the same question without
///   allocating a `String` to drop immediately.
#[cfg(all(recovery, feature = "replication"))]
fn map_was_taken_over(_: &Store, _: &str, _: u64, _: &VectorIndex) -> Result<bool> {
    Ok(false)
}

#[cfg(all(recovery, not(feature = "replication")))]
fn map_was_taken_over(store: &Store, name: &str, stamp: u64, index: &VectorIndex) -> Result<bool> {
    let (meta, _) = usable_metadata(store, name, stamp)?;
    Ok(!index.owned_by(&meta.owner))
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

        Ok(probe) if !probe.is_map => {
            if registry::remove_if_stale(name, stamp) {
                eprintln!(
                    "ArcVector: '{name}' is not a Map ({why}); releasing the graph that used to be there"
                );
            }
        }

        Ok(_) => {
            if crate::handler::arcus::abi::mismatched() {
                return;
            }
            eprintln!(
                "ArcVector: '{name}' has unusable metadata ({why}); keeping the Map but releasing the graph"
            );
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
        // A small rebuild is worth waiting out, so the answer is whole. A large
        // one is not: the read goes to the graph as it stands and the reply says
        // so, rather than refusing for the minutes the rebuild takes.
        if index.state() == registry::FILLING
            && index.rebuild_size() <= SMALL_REBUILD
            && index.await_refill(REBUILD_WAIT_CAP, WAITING_WORKERS)
        {
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

#[cfg(test)]
mod resolve_tests {
    // `resolve` needs a live engine to exercise, and this crate has no
    // in-process substitute for one -- `Store` wraps an engine pointer handed
    // in by the memcached process, and every other test in this module tree
    // that touches a registered index goes through the registry directly
    // rather than through `resolve`. Short of adding that harness, the only
    // thing left to check without one is where the read sits in the source:
    // that it is inside the registry-miss arm, not in the `Some(index)` arm,
    // above the match, or in the comparison that follows it. Pinning the
    // shape like this proves nothing about runtime behaviour, and it breaks
    // on a harmless reformatting that keeps the read on the miss path but
    // changes how the source reads around it.

    /// Splits the `cfg(recovery)` `resolve`'s body into the text before the
    /// registry-miss (`None`) arm, the arm's own braced block, and the text
    /// after it -- found by counting braces from the arm's opening `{`
    /// rather than by a fixed byte offset, so the split survives edits
    /// inside the arm as long as its braces stay balanced.
    fn split_around_the_miss_arm(body: &str) -> (&str, &str, &str) {
        let arm_start = body.find("None => {").expect("the miss arm is here");
        let brace_start = arm_start + body[arm_start..].find('{').expect("its opening brace");
        let mut depth = 0usize;
        for (i, ch) in body[brace_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let arm_end = brace_start + i + 1;
                        return (
                            &body[..brace_start],
                            &body[brace_start..arm_end],
                            &body[arm_end..],
                        );
                    }
                }
                _ => {}
            }
        }
        panic!("the miss arm's braces never close");
    }

    #[test]
    fn a_registered_index_resolves_without_reading_metadata() {
        let source = include_str!("mod.rs");
        let function_body = source
            .split_once("pub(super) fn resolve")
            .expect("resolve is here")
            .1
            .split_once("\n#[cfg(not(recovery))]")
            .expect("the not(recovery) variant follows it in the file")
            .0;

        let (before, miss_arm, after) = split_around_the_miss_arm(function_body);

        // Both names read metadata (`usable_metadata` wraps `read_metadata`,
        // and `claim_refilled`'s own read is a separate, already-existing
        // call this task did not move) -- either one appearing outside the
        // miss arm would reintroduce a per-command read, whether by calling
        // `usable_metadata` from the `Some(index)` arm or the post-match
        // comparison, or by reaching for `read_metadata` directly instead.
        for forbidden in ["usable_metadata", "read_metadata"] {
            assert!(
                !before.contains(forbidden) && !after.contains(forbidden),
                "a metadata read outside the registry-miss arm reintroduces the \
                 per-command cost this task removed (found `{forbidden}`)"
            );
        }
        assert!(
            miss_arm.contains("usable_metadata"),
            "the registry-miss arm should still read metadata on first touch"
        );

        // The per-command comparison did not disappear; it moved into
        // `map_was_taken_over`, and which definition a build gets is the whole
        // point. A `persistence`-only build has `cfg(recovery)` but no
        // replication stream, so gating the narrowing on `cfg(recovery)` --
        // which is what this task originally did -- silently took the check
        // away from the one configuration with nothing to replace it. Pinning
        // both definitions here is what makes that regression visible: the
        // second `split_once` simply fails if the two are collapsed back into
        // one.
        let free = source
            .split_once("#[cfg(all(recovery, feature = \"replication\"))]\nfn map_was_taken_over")
            .expect("a build with a stream answers this without an engine call")
            .1;
        assert!(
            !free[..free.find("\n}").expect("the body closes")].contains("usable_metadata"),
            "with a stream to report a takeover, a registered index costs no metadata read"
        );

        let paid = source
            .split_once(
                "#[cfg(all(recovery, not(feature = \"replication\")))]\nfn map_was_taken_over",
            )
            .expect("a recovery build with no stream keeps the per-command comparison");
        assert!(
            paid.1[..paid.1.find("\n}").expect("the body closes")].contains("usable_metadata"),
            "a persistence-only build has no stream, so it must still read the owner itself"
        );
    }
}
