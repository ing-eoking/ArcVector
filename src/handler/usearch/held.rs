//! The elements this graph holds a refcount on.
//!
//! A node's usearch key **is the address of its Map element**. That is what replaced the id
//! mapping: the reverse direction is a dereference, not a lookup — the element carries its own
//! field bytes, and those are the id.
//!
//! The refcount is what makes the address safe to dereference. The engine frees an element only
//! once no refcount stands, so one unlinked by a delete or an overwrite stays readable, at the
//! same address, until this set lets it go.
//!
//! **The lock is the rest of the safety.** A lookup takes the read lock, checks membership, and
//! dereferences while still holding it; a release takes the write lock. So an address cannot be
//! handed back between the check and the dereference — the writer waits. Nothing else is needed:
//! no deferred release list, and no counter of searches in flight.
//!
//! `tombstones` is the one piece of bookkeeping left. A refill replays a snapshot taken at
//! takeover; an element deleted since is still in that snapshot and its node is gone from the
//! graph, so nothing else would stop the refill from putting it back.

use std::collections::{HashSet, TryReserveError};
use std::sync::Arc;

/// Marks a key as a staged node's placeholder rather than an element address.
///
/// The high bit, which no user-space address uses. A staged node is in the graph and searches
/// visit it, but its element is not linked yet — its address would dereference to a field that
/// is in no Map, so the row must be dropped. The tag is what lets a lookup drop it without
/// asking anything: one bit test.
pub(super) const STAGED_TAG: u64 = 1 << 63;

pub(super) const fn is_staged(key: u64) -> bool {
    key & STAGED_TAG != 0
}

#[derive(Default)]
pub(super) struct HeldSet {
    live: HashSet<u64>,
    /// Addresses deleted while a rebuild runs.
    tombstones: HashSet<u64>,
}

impl HeldSet {
    pub(super) fn len(&self) -> usize {
        self.live.len()
    }

    pub(super) fn contains(&self, addr: u64) -> bool {
        self.live.contains(&addr)
    }

    /// Whether a refill should skip this element: already in the graph, or deleted since the
    /// snapshot was taken.
    pub(super) fn is_known(&self, addr: u64) -> bool {
        self.live.contains(&addr) || self.tombstones.contains(&addr)
    }

    /// Room for one more address, taken before anything is written.
    ///
    /// The insert grows the set on its own, and a std collection that cannot grow aborts the
    /// process rather than returning.
    pub(super) fn reserve(&mut self) -> Result<(), TryReserveError> {
        self.live.try_reserve(1)
    }

    /// Room to remember one deletion, taken before the engine delete it pairs with.
    pub(super) fn reserve_tombstone(&mut self) -> Result<(), TryReserveError> {
        self.tombstones.try_reserve(1)
    }

    /// Record that this graph holds `addr`. Call [`Self::reserve`] first.
    pub(super) fn publish(&mut self, addr: u64) {
        debug_assert!(
            self.live.capacity() > self.live.len() || self.live.contains(&addr),
            "publish without reserve: the insert below can abort"
        );
        self.live.insert(addr);
        self.tombstones.remove(&addr);
    }

    /// Stop holding `addr`, reporting whether this graph held it.
    ///
    /// The caller hands the refcount back, and must do so **before releasing the write lock** —
    /// that is what keeps a lookup from dereferencing an address on its way out. A second
    /// removal reports `false` rather than releasing a refcount twice.
    pub(super) fn take(&mut self, addr: u64) -> bool {
        self.live.remove(&addr)
    }

    /// Same, and remembered so a refill in flight cannot replay it.
    pub(super) fn take_tombstoned(&mut self, addr: u64) -> bool {
        let held = self.take(addr);
        self.tombstones.insert(addr);
        held
    }

    /// Everything the graph holds, for the caller to release. Used by `clear` and by dropping
    /// the index — the two moments every hold goes at once.
    pub(super) fn take_all(&mut self) -> Vec<u64> {
        self.tombstones.clear();
        self.live.drain().collect()
    }

    pub(super) fn forget_tombstones(&mut self) {
        self.tombstones.clear();
    }

    /// Every address the graph holds. For assertions at rest.
    #[cfg(test)]
    pub(super) fn live_addrs(&self) -> Vec<u64> {
        self.live.iter().copied().collect()
    }

    /// Module memory this set costs, which arcus cannot see.
    pub(super) fn bytes(&self) -> usize {
        (self.live.capacity() + self.tombstones.capacity()) * std::mem::size_of::<u64>()
    }
}

/// What the graph needs from the store that owns its elements.
///
/// The graph keys nodes by element address, so it has to dereference one to learn an id and
/// hand one back when a node goes. Both are the store's business, and stating them as a trait
/// is what keeps `usearch` from depending on `arcus` — the direction stays store → graph.
pub trait Elements: Send + Sync {
    /// The field bytes of the element at `addr`, or `None` if they cannot be read.
    ///
    /// Only ever called with an address the held set still holds, under its read lock.
    fn id_at(&self, addr: u64) -> Option<Arc<str>>;

    /// Hand back the refcounts on these addresses. Called with the write lock held, which is
    /// what stops a lookup from dereferencing one on its way out.
    fn release(&self, addrs: &[u64]);
}
