//! An entry pairs the Map with the graph built from it and the `owner` token it was built under; [`super::recovery`] judges that token.

use std::collections::{HashMap, TryReserveError};
use std::sync::{Arc, LazyLock, PoisonError, RwLock, RwLockWriteGuard};

use crate::handler::usearch::AnnIndex;

/// `owner` while a rebuild is in flight: claimed by nobody.
pub const REBUILDING: u64 = 0;

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,
    /// Element-count limit for vectors, already excluding the metadata element.
    pub maxcount: u32,
    owner: std::sync::atomic::AtomicU64,
    /// Set by the rebuild thread when the graph is complete.
    #[cfg(recovery)]
    pub(super) refilled: std::sync::atomic::AtomicBool,
}

impl VectorIndex {
    pub fn new(name: String, ann: AnnIndex, maxcount: u32, owner: u64) -> Self {
        Self {
            name,
            ann,
            maxcount,
            owner: std::sync::atomic::AtomicU64::new(owner),
            #[cfg(recovery)]
            refilled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn owner(&self) -> u64 {
        self.owner.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn is_rebuilding(&self) -> bool {
        self.owner() == REBUILDING
    }

    /// Whether the graph is full again and only the token write is outstanding.
    #[cfg(recovery)]
    pub fn is_refilled(&self) -> bool {
        self.refilled.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(recovery)]
    pub(super) fn mark_refilled(&self) {
        self.refilled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(recovery)]
    pub(super) fn set_owner(&self, owner: u64) {
        self.owner
            .store(owner, std::sync::atomic::Ordering::Release);
    }
}

static INDICES: LazyLock<RwLock<HashMap<String, Arc<VectorIndex>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn read() -> std::sync::RwLockReadGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.read().unwrap_or_else(PoisonError::into_inner)
}

fn write() -> std::sync::RwLockWriteGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.write().unwrap_or_else(PoisonError::into_inner)
}

/// Look a name up, releasing the registry lock immediately.
pub fn get(name: &str) -> Option<Arc<VectorIndex>> {
    read().get(name).cloned()
}

pub fn contains(name: &str) -> bool {
    read().contains_key(name)
}

/// The registry held across the engine write that makes a Map.
///
/// `vcreate` registers first and writes to the engine last, which is the module's rule
/// everywhere: the engine write emits `CLOG_MAP_ELEM_INSERT` and is what carries the write off
/// this node, so everything that can fail belongs in front of it. Registering is one of those
/// things — the map has to grow — and doing it after would mean answering for a Map already
/// made, with only a delete to take it back.
///
/// The hold is what makes the pair atomic. Without it the name is registered while its Map does
/// not exist yet, and a command arriving there reads no Map and releases the entry — correctly,
/// on what it can see. Every lookup goes through this lock, so no one sees between the two.
pub struct Held(RwLockWriteGuard<'static, HashMap<String, Arc<VectorIndex>>>);

/// Take the registry for a create. Nothing else may take an engine lock and then this one.
pub fn hold() -> Held {
    Held(write())
}

impl Held {
    /// Room for one entry, taken before anything is written anywhere.
    pub fn reserve(&mut self) -> Result<(), TryReserveError> {
        self.0.try_reserve(1)
    }

    /// Register `index`, handing back whatever the name held before.
    ///
    /// Call [`Self::reserve`] first: the insert grows the map on its own, and a std collection
    /// that cannot grow aborts. The key clone is a single small allocation and has no fallible
    /// form on stable — see `미해결.md §7`.
    pub fn put(&mut self, index: VectorIndex) -> Option<Arc<VectorIndex>> {
        self.0.insert(index.name.clone(), Arc::new(index))
    }

    /// Undo a [`Self::put`], allocation-free: the key is already in the map either way.
    pub fn restore(&mut self, name: &str, previous: Option<Arc<VectorIndex>>) {
        match previous {
            Some(index) => {
                if let Some(slot) = self.0.get_mut(name) {
                    *slot = index;
                }
            }
            None => {
                self.0.remove(name);
            }
        }
    }
}

/// Register `index` unless the name is taken, returning whichever ends up live.
///
/// The room comes first. `entry` grows the map on its own, and a std collection that cannot
/// grow aborts the process instead of returning — the one failure a daemon must never take.
/// Asking for the room up front turns it into a value the caller can answer with, and leaves
/// the registry untouched when the answer is no.
pub fn insert_or_get(index: VectorIndex) -> Result<(Arc<VectorIndex>, bool), TryReserveError> {
    let mut reg = write();
    reg.try_reserve(1)?;
    let mut inserted = false;
    let entry = reg.entry(index.name.clone()).or_insert_with(|| {
        inserted = true;
        Arc::new(index)
    });
    Ok((Arc::clone(entry), inserted))
}

pub fn remove(name: &str) -> bool {
    write().remove(name).is_some()
}

/// Remove `name` only while it still holds `observed`.
///
/// Releasing a graph is always decided on something read earlier — a metadata read, an engine
/// call's `KeyGone` — and by the time the decision arrives the name can hold a different index
/// entirely. Matching on identity is what keeps a stale verdict from taking out a live entry:
/// `vcreate` registering between the read and the release is exactly that case, and by name
/// alone the new index would be dropped with the Map it was just built for still in place.
pub fn remove_observed(name: &str, observed: &VectorIndex) -> bool {
    let mut reg = write();
    match reg.get(name) {
        Some(current) if std::ptr::eq(Arc::as_ptr(current), observed) => {
            reg.remove(name);
            true
        }
        _ => false,
    }
}

/// Every index that is serving, ordered by name for stable `vlist` output.
pub fn snapshot() -> Vec<Arc<VectorIndex>> {
    let mut all: Vec<Arc<VectorIndex>> = read()
        .values()
        .filter(|index| !index.is_rebuilding())
        .cloned()
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::arcus::element::Layout;
    use crate::handler::quant::Quant;
    use crate::handler::usearch::Metric;

    fn index(name: &str) -> VectorIndex {
        let ann = AnnIndex::new(Layout::new(4, Quant::F32), Metric::L2, 0, 0, 0)
            .expect("build the graph");
        VectorIndex::new(name.to_owned(), ann, 8, 1)
    }

    /// The race this guards: a command reads the Map, finds it gone, and only reaches the
    /// registry after a `vcreate` has put a live index under the same name. By name alone the
    /// release takes out the new one, leaving a Map nothing serves.
    #[test]
    fn a_stale_release_leaves_a_newly_registered_index_alone() {
        let name = "registry-test-stale-release";
        let (observed, _) = insert_or_get(index(name)).expect("register the first");

        // The first index goes, a second takes the name — what `vcreate` does.
        assert!(remove(name));
        let (current, inserted) = insert_or_get(index(name)).expect("register the second");
        assert!(inserted, "the name was free");

        // The release finally arrives, carrying a verdict about the index that is already gone.
        assert!(
            !remove_observed(name, &observed),
            "a stale release must report that it removed nothing"
        );
        assert!(
            get(name).is_some_and(|live| Arc::ptr_eq(&live, &current)),
            "the live index must survive a release meant for its predecessor"
        );
        remove(name);
    }

    #[test]
    fn a_release_for_the_live_index_removes_it() {
        let name = "registry-test-live-release";
        let (live, _) = insert_or_get(index(name)).expect("register");

        assert!(
            remove_observed(name, &live),
            "the observed index is the live one"
        );
        assert!(get(name).is_none(), "the entry is gone");
    }
}
