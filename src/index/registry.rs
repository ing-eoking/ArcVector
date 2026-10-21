//! Registry table mapping index names to vector indices, managing dropped but in-use indices.
//!
//! The table stores [`VectorIndex`] instances wrapped in `Arc`. Commands acquire a [`Reader`]
//! at start and release it on completion. When a new index is created with the same name,
//! only the table entry is replaced: in-flight commands continue using the old index,
//! while new commands pick up the new one.
//!
//! # Deletion
//!
//! When a Map is unlinked (`drop_map`), its entry is removed from the table and moved to [`GONE`].
//! Once marked deleted, no new readers can acquire it. If no active readers remain, it is
//! finalized immediately; otherwise, it is finalized when the last reader drops.
//!
//! Keeping dropped indices in [`GONE`] until finalized is necessary for element events.
//! The engine's collection deletion thread unlinks elements asynchronously; as long as
//! active readers exist, concurrent searches might dereference those elements, requiring
//! removal from the graph and retirement through the queue. Once finalized, no search
//! can access the index, so element unlinks are freed immediately by the callback.
//!
//! After finalization, remaining items in the retirement queue are drained and discarded
//! by the sweeper outside the engine cache lock.

use std::collections::{HashMap, TryReserveError};
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::index::AnnIndex;
use crate::runtime::sweep;

static CLOCK: AtomicU64 = AtomicU64::new(1);

pub fn now() -> u64 {
    CLOCK.load(Ordering::Acquire)
}

const DELETED: usize = 1 << (usize::BITS - 1);
const COUNT: usize = !DELETED;

pub struct VectorIndex {
    pub name: String,
    /// Map item address for this index, matched against `coll_item` in element events
    /// to distinguish old and new Maps sharing the same key.
    pub map: u64,
    pub ann: AnnIndex,

    published: AtomicU64,
    readers: AtomicUsize,
    finalized: AtomicBool,
}

impl VectorIndex {
    pub fn ours(name: String, map: u64, ann: AnnIndex) -> Self {
        Self {
            name,
            map,
            ann,
            published: AtomicU64::new(0),
            readers: AtomicUsize::new(0),
            finalized: AtomicBool::new(false),
        }
    }

    fn publish(&self) {
        self.published
            .store(CLOCK.fetch_add(1, Ordering::AcqRel), Ordering::Release);
    }

    fn published_at(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    /// Acquires a reader for a command. Returns `None` if deleted.
    /// The index will not be finalized while the returned [`Reader`] is alive.
    pub fn read(self: &Arc<Self>) -> Option<Reader> {
        self.hold(|readers| readers & DELETED == 0)
    }

    /// Acquires a reader for an asynchronous graph job from a callback.
    /// Returns `None` if already finalized.
    pub fn pin(self: &Arc<Self>) -> Option<Reader> {
        self.hold(|readers| readers & DELETED == 0 || readers & COUNT > 0)
    }

    /// Atomically increments the reader count only if `may(readers)` is true.
    fn hold(self: &Arc<Self>, may: impl Fn(usize) -> bool) -> Option<Reader> {
        let mut readers = self.readers.load(Ordering::Acquire);
        loop {
            if !may(readers) {
                return None;
            }
            match self.readers.compare_exchange_weak(
                readers,
                readers + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Reader(Arc::clone(self))),
                Err(now) => readers = now,
            }
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.readers.load(Ordering::Acquire) & DELETED != 0
    }

    fn is_finalized(&self) -> bool {
        self.finalized.load(Ordering::Acquire)
    }

    fn mark_deleted(&self) {
        let prev = self.readers.fetch_or(DELETED, Ordering::AcqRel);
        if prev & DELETED == 0 && prev & COUNT == 0 {
            self.finalize();
        }
    }

    fn leave(&self) {
        let prev = self.readers.fetch_sub(1, Ordering::AcqRel);
        if prev == DELETED | 1 {
            self.finalize();
        }
    }

    /// Finalizes a deleted index when active reader count reaches zero (invoked at most once).
    /// Does not call into the engine so it can run under the engine cache lock.
    fn finalize(&self) {
        if self.finalized.swap(true, Ordering::AcqRel) {
            return;
        }
        sweep::wake();
    }
}

/// Scoped reader guard. Decrements read count on drop.
///
/// When the last [`Reader`] of a deleted index drops, that index is finalized.
pub struct Reader(Arc<VectorIndex>);

impl Deref for Reader {
    type Target = Arc<VectorIndex>;

    fn deref(&self) -> &Arc<VectorIndex> {
        &self.0
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.leave();
    }
}

static INDICES: LazyLock<RwLock<HashMap<String, Arc<VectorIndex>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Indices removed from the primary table: either still referenced by active readers
/// or awaiting complete retirement queue drain by the sweeper.
/// Preallocated to avoid allocations under the engine cache lock.
/// Lock order: [`INDICES`] first, then [`GONE`].
static GONE: LazyLock<RwLock<Vec<Arc<VectorIndex>>>> = LazyLock::new(|| RwLock::new(Vec::new()));

fn read() -> RwLockReadGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.read().unwrap_or_else(PoisonError::into_inner)
}

fn write() -> RwLockWriteGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.write().unwrap_or_else(PoisonError::into_inner)
}

fn gone_read() -> RwLockReadGuard<'static, Vec<Arc<VectorIndex>>> {
    GONE.read().unwrap_or_else(PoisonError::into_inner)
}

fn gone_write() -> RwLockWriteGuard<'static, Vec<Arc<VectorIndex>>> {
    GONE.write().unwrap_or_else(PoisonError::into_inner)
}

/// Looks up a live index by name without incrementing reader count.
pub fn get(name: &str) -> Option<Arc<VectorIndex>> {
    read().get(name).cloned()
}

/// Looks up an index by name and Map item address for element callbacks.
/// Matches live indices and non-finalized dropped indices with active readers.
pub fn of_map(name: &str, map: u64) -> Option<Arc<VectorIndex>> {
    let live = read();
    if let Some(index) = live.get(name)
        && index.map == map
    {
        return Some(Arc::clone(index));
    }
    gone_read()
        .iter()
        .find(|index| index.map == map && index.name == name && !index.is_finalized())
        .cloned()
}

/// Returns all live and pending dropped indices for sweeper inspection.
pub fn all() -> Vec<Arc<VectorIndex>> {
    let live = read();
    let gone = gone_read();
    let mut all = Vec::new();
    // Skip round if allocation fails; sweeper will retry on the next tick.
    if all.try_reserve(live.len() + gone.len()).is_err() {
        return all;
    }
    all.extend(live.values().cloned());
    all.extend(gone.iter().cloned());
    all
}

/// Reaps finalized indices whose retirement queues have been completely drained.
/// Dropped outside locks to avoid running destructors under [`GONE`].
pub fn reap_finalized() {
    loop {
        let reaped = {
            let mut gone = gone_write();
            let Some(at) = gone.iter().position(|index| {
                index.is_finalized() && Arc::strong_count(index) == 1 && index.ann.is_drained()
            }) else {
                return;
            };
            gone.swap_remove(at)
        };
        drop(reaped);
    }
}

pub fn contains(name: &str) -> bool {
    read().contains_key(name)
}

/// Inserts a new index into the registry, deleting any existing entry with the same name.
/// Preallocates capacity in [`GONE`] so unlinks need not allocate under the cache lock.
pub fn put(index: VectorIndex) -> Result<Arc<VectorIndex>, TryReserveError> {
    sweep::ensure_sweeper();
    crate::runtime::pool::ensure_pool();
    index.ann.set_bell();

    let mut live = write();
    live.try_reserve(1)?;
    let mut gone = gone_write();
    gone.try_reserve(live.len() + 1)?;
    // Timestamp recorded inside the lock to avoid race conditions with `remove_if_stale`.
    index.publish();
    let index = Arc::new(index);
    let previous = live.insert(index.name.clone(), Arc::clone(&index));
    if let Some(previous) = &previous {
        set_aside(&mut gone, Arc::clone(previous));
    }
    drop(gone);
    drop(live);
    if let Some(previous) = previous {
        previous.mark_deleted();
    }
    Ok(index)
}

/// Moves an unlinked index to [`GONE`]. Capacity was reserved in [`put`].
fn set_aside(gone: &mut Vec<Arc<VectorIndex>>, index: Arc<VectorIndex>) {
    if gone.len() < gone.capacity() {
        gone.push(index);
        return;
    }
    // If capacity was somehow exhausted, attempt reserve or leak to avoid freeing under cache lock.
    if gone.try_reserve(1).is_ok() {
        gone.push(index);
        return;
    }
    eprintln!(
        "ArcVector: no room to set aside dropped index '{}'; leaking its graph",
        index.name
    );
    std::mem::forget(index);
}

/// Removes an index by name from the primary table. Returns `true` if found.
fn take_out(
    mut live: RwLockWriteGuard<'static, HashMap<String, Arc<VectorIndex>>>,
    name: &str,
) -> bool {
    let Some(index) = live.remove(name) else {
        return false;
    };
    set_aside(&mut gone_write(), Arc::clone(&index));
    drop(live);
    index.mark_deleted();
    true
}

/// Unlinks an index matching name and Map address. Called from the unlink callback.
pub fn drop_map(name: &str, map: u64) -> bool {
    let live = write();
    if live.get(name).is_none_or(|index| index.map != map) {
        return false;
    }
    take_out(live, name)
}

/// Removes an index if it was published before `stamp`.
pub fn remove_if_stale(name: &str, stamp: u64) -> bool {
    let live = write();
    if live
        .get(name)
        .is_none_or(|current| current.published_at() >= stamp)
    {
        return false;
    }
    take_out(live, name)
}

/// Removes an index by name. Used in tests.
#[cfg(test)]
pub fn remove(name: &str) -> bool {
    take_out(write(), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::element::Layout;
    use crate::index::Metric;
    use crate::index::quant::Quant;

    fn index(name: &str, map: u64) -> VectorIndex {
        let ann = AnnIndex::new(
            Layout::new(4, Quant::F32),
            Metric::L2,
            0,
            0,
            0,
            Arc::new(crate::engine::MapElements),
        )
        .expect("build the graph");
        VectorIndex::ours(name.to_owned(), map, ann)
    }

    fn in_gone(index: &Arc<VectorIndex>) -> bool {
        gone_read().iter().any(|gone| Arc::ptr_eq(gone, index))
    }

    #[test]
    fn a_verdict_older_than_the_entry_releases_nothing() {
        let name = "registry-test-late-verdict";
        let stamp = now();

        put(index(name, 0x100)).expect("claim the name");

        assert!(
            !remove_if_stale(name, stamp),
            "a verdict from before the entry existed must release nothing"
        );
        assert!(get(name).is_some(), "the new index survives");
        assert!(
            remove_if_stale(name, now()),
            "a verdict taken after the entry releases it"
        );
        assert!(get(name).is_none(), "the entry is gone");
    }

    #[test]
    fn a_drop_with_no_reader_finalizes_at_once() {
        let name = "registry-test-drop-idle";
        let index = put(index(name, 0x200)).expect("register");

        assert!(drop_map(name, 0x200));
        assert!(get(name).is_none(), "nobody can pick it up any more");
        assert!(index.is_deleted());
        assert!(
            index.is_finalized(),
            "nobody held it, so it is finalized now"
        );
        assert!(
            of_map(name, 0x200).is_none(),
            "element events no longer route to a finalized index"
        );
    }

    #[test]
    fn the_last_reader_finalizes_a_dropped_index() {
        let name = "registry-test-drop-busy";
        let index = put(index(name, 0x300)).expect("register");
        let first = index.read().expect("live");
        let second = index.read().expect("live");

        assert!(drop_map(name, 0x300));
        assert!(index.is_deleted());
        assert!(!index.is_finalized(), "two readers still hold it");
        assert!(
            index.read().is_none(),
            "a deleted index cannot be picked up"
        );
        assert!(
            of_map(name, 0x300).is_some_and(|found| Arc::ptr_eq(&found, &index)),
            "element events still reach it while it is read"
        );
        assert!(in_gone(&index));

        drop(first);
        assert!(!index.is_finalized(), "one reader is left");
        drop(second);
        assert!(index.is_finalized(), "the last reader finalizes it");
        assert!(of_map(name, 0x300).is_none());
    }

    #[test]
    fn a_new_index_under_the_same_name_replaces_only_the_entry() {
        let name = "registry-test-recreate";
        let old = put(index(name, 0x400)).expect("register");
        let reader = old.read().expect("live");
        assert!(drop_map(name, 0x400));

        let new = put(index(name, 0x500)).expect("re-create");
        assert!(get(name).is_some_and(|live| Arc::ptr_eq(&live, &new)));
        assert!(
            of_map(name, 0x400).is_some_and(|found| Arc::ptr_eq(&found, &old)),
            "the old Map's elements still find the old graph"
        );
        assert!(
            of_map(name, 0x500).is_some_and(|found| Arc::ptr_eq(&found, &new)),
            "the new Map's elements find the new graph"
        );
        assert!(!new.is_deleted());

        drop(reader);
        assert!(old.is_finalized());
        assert!(!new.is_finalized());
        assert!(remove(name));
    }

    #[test]
    fn a_drop_for_an_older_map_leaves_the_new_index_alone() {
        let name = "registry-test-stale-drop";
        let new = put(index(name, 0x700)).expect("register");
        assert!(!drop_map(name, 0x600), "the unlink of another Map");
        assert!(!new.is_deleted());
        assert!(remove(name));
    }

    #[test]
    fn a_job_can_pin_a_deleted_index_until_it_is_finalized() {
        let name = "registry-test-pin";
        let index = put(index(name, 0x900)).expect("register");
        let reader = index.read().expect("live");
        assert!(drop_map(name, 0x900));

        assert!(index.read().is_none(), "no new command on a deleted index");
        let job = index
            .pin()
            .expect("a job may still pin it while it is read");
        drop(reader);
        assert!(!index.is_finalized(), "the job still holds it");
        drop(job);
        assert!(index.is_finalized());
        assert!(index.pin().is_none(), "nothing pins a finalized index");
    }

    #[test]
    fn a_reader_that_backs_off_a_deleted_index_can_be_the_one_that_finalizes() {
        let name = "registry-test-backoff";
        let index = put(index(name, 0x800)).expect("register");
        let reader = index.read().expect("live");
        assert!(drop_map(name, 0x800));
        // Backed off acquire on deleted index does not finalize while another reader holds it.
        assert!(index.read().is_none());
        assert!(!index.is_finalized());
        drop(reader);
        assert!(index.is_finalized());
    }
}
