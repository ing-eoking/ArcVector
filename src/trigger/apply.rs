//! Applies the graph work queued by the callback, on a lane thread.
//!
//! Each element event is one job. The callback picks a lane by the element's vkey hash and queues
//! the job (`offload::enqueue`); the lane runs jobs in queue order. So inserts, replaces and
//! deletes of one vkey reach the graph in the order the engine sent the events, and one element's
//! graph work always runs on a single thread -- it can never be removed before it is inserted, or
//! re-inserted after removal.
//!
//! Reference ownership (see the `event` module docs):
//!
//! * The new element of an insert or replace is always held by the callback. If it cannot be
//!   inserted, it stays held outside the graph.
//! * The old element of a delete or replace was answered "held" by the callback and is released
//!   here: via the retire queue by the sweeper (after in-flight searches end) if it was in the
//!   graph, immediately otherwise.
//!
//! Lanes run outside the engine's cache lock, so they may call engine functions that read
//! elements and release references. Elements cannot be freed meanwhile because we hold a
//! reference.

use crate::engine::{Store, StoreError};
use crate::error::{Error, Result};
use crate::index::registry::{Reader, VectorIndex};

/// What one element event asks of the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// An element arrived. Insert it into the graph.
    Insert(u64),
    /// An element left. Remove it from the graph and release the reference.
    Delete(u64),
    /// An element was replaced. If the vector is unchanged only the node is moved; otherwise old
    /// is removed and new is inserted.
    Replace { old: u64, new: u64 },
}

/// Graph work queued on a lane. It pins the index, so the index is not cleaned up until the job
/// finishes.
pub(crate) struct Graph {
    pub(crate) index: Reader,
    pub(crate) op: Op,
}

/// Runs one job. **Runs on a lane thread.**
///
/// An error means the element could not be inserted. It stays held outside the graph; when it
/// leaves, the delete sees there is no node in the graph and releases it.
pub(crate) fn apply(graph: Graph) -> Result<()> {
    let Graph { index, op } = graph;
    let store = Store::background();
    match op {
        Op::Insert(addr) => insert(store.as_ref(), &index, addr),
        Op::Delete(addr) => {
            remove(&index, addr);
            Ok(())
        }
        Op::Replace { old, new } => {
            if let Some(store) = &store
                && index.ann.holds(old)
                && same_vector(store, &index, old, new)
                && index.ann.rename_node(old, new)
            {
                // Only attrs changed (`vop setmeta`). The node now points at new, so there is no
                // need to re-insert and the graph never loses this vector. Old is no longer in
                // the graph.
                if !index.ann.retire_one(old) {
                    index.ann.release_after_halt(old);
                }
                return Ok(());
            }
            // The vector changed. `rename_node` alone would only change the address while the
            // graph kept the old coordinates.
            remove(&index, old);
            insert(store.as_ref(), &index, new)
        }
    }
}

/// Reads the element's vector and inserts it into the graph.
fn insert(store: Option<&Store>, index: &VectorIndex, addr: u64) -> Result<()> {
    let store = store.ok_or(Error::Store(StoreError::Unavailable))?;
    let layout = index.ann.layout;
    store
        .with_elem_at(addr, |_field, value| {
            layout
                .split(value)
                .ok()
                .map(|(_, vector)| index.ann.link_node(addr, vector))
        })
        .flatten()
        .unwrap_or(Err(Error::Store(StoreError::CorruptElement)))
}

/// Removes the element from the graph and releases its reference.
fn remove(index: &VectorIndex, addr: u64) {
    if !index.ann.holds(addr) {
        // It was never inserted, so no search can dereference it.
        index.ann.release_now(addr);
        return;
    }
    if !index.ann.unlink_at(addr) {
        // The node is still in the graph. The sweeper (`retry_stuck`) retries the unlink and
        // release.
        return;
    }
    // The sweeper releases it once searches holding the address as a candidate have finished.
    if !index.ann.retire_one(addr) {
        index.ann.release_after_halt(addr);
    }
}

/// Whether two elements hold the same vector. Detects writes that changed only attrs.
fn same_vector(store: &Store, index: &VectorIndex, old: u64, new: u64) -> bool {
    let layout = index.ann.layout;
    store
        .with_elem_at(old, |_field, old_value| {
            store.with_elem_at(new, |_field, new_value| {
                match (layout.split(old_value), layout.split(new_value)) {
                    (Ok((_, a)), Ok((_, b))) => a == b,
                    _ => false,
                }
            })
        })
        .flatten()
        .unwrap_or(false)
}
