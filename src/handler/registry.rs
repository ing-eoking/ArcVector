use std::collections::{HashMap, TryReserveError};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use crate::handler::access::sweep;
use crate::handler::usearch::AnnIndex;

pub const SERVING: u8 = 0;

pub const DRAINING: u8 = 1;

pub const COLD: u8 = 2;

pub const FILLING: u8 = 3;

pub const BUILDING: u8 = 4;

static CLOCK: AtomicU64 = AtomicU64::new(1);

pub fn now() -> u64 {
    CLOCK.load(Ordering::Acquire)
}

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,

    pub maxcount: u32,
    state: AtomicU8,

    published: AtomicU64,
}

impl VectorIndex {
    pub fn ours(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        let index = Self::new(name, ann, maxcount, SERVING);
        index.mark_serving();
        index
    }

    pub fn rebuilding(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        Self::new(name, ann, maxcount, COLD)
    }

    pub fn building(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        Self::new(name, ann, maxcount, BUILDING)
    }

    fn new(name: String, ann: AnnIndex, maxcount: u32, state: u8) -> Self {
        Self {
            name,
            ann,
            maxcount,
            state: AtomicU8::new(state),
            published: AtomicU64::new(0),
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub fn publish(&self) {
        self.published
            .store(CLOCK.fetch_add(1, Ordering::AcqRel), Ordering::Release);
    }

    fn published_at(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    pub fn is_rebuilding(&self) -> bool {
        self.state() != SERVING
    }

    /// Always `None`: a graph is never rebuilt any more, so a read is never
    /// partial. Kept so `Reply` still has the shape its callers expect.
    pub fn rebuilding_progress(&self) -> Option<crate::error::Rebuild> {
        None
    }

    /// Marks this index ready to serve.
    ///
    /// There is no ownership token any more: an index is in this registry
    /// because the trigger callback saw its metadata item linked, and no other
    /// process can have put it there.
    pub(in crate::handler) fn mark_serving(&self) {
        self.state.store(SERVING, Ordering::Release);
    }

    // The unit tests drive state transitions through this directly; nothing in
    // the request path does any more, now that an index is either registered by
    // the trigger or absent.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(super) fn mark_state(&self, now: u8) {
        self.state.store(now, Ordering::Release);
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

pub fn get(name: &str) -> Option<Arc<VectorIndex>> {
    read().get(name).cloned()
}

pub fn contains(name: &str) -> bool {
    read().contains_key(name)
}

pub fn put(
    index: VectorIndex,
) -> Result<(Arc<VectorIndex>, Option<Arc<VectorIndex>>), TryReserveError> {
    sweep::ensure_sweeper();
    index.ann.set_bell();
    let mut reg = write();
    reg.try_reserve(1)?;
    let index = Arc::new(index);
    let previous = reg.insert(index.name.clone(), Arc::clone(&index));
    Ok((index, previous))
}

pub fn unput(ours: &VectorIndex, previous: Option<Arc<VectorIndex>>) {
    let mut reg = write();
    let name = ours.name.as_str();
    if !reg
        .get(name)
        .is_some_and(|c| std::ptr::eq(Arc::as_ptr(c), ours))
    {
        return;
    }
    match previous {
        Some(index) => {
            if let Some(slot) = reg.get_mut(name) {
                *slot = index;
            }
        }
        None => {
            let evicted = reg.remove(name);
            drop(reg);
            sweep::retire(evicted);
        }
    }
}

pub fn remove_if_stale(name: &str, stamp: u64) -> bool {
    let mut reg = write();
    match reg.get(name) {
        Some(current) => {
            if current.state() == BUILDING || current.published_at() >= stamp {
                return false;
            }
            let evicted = reg.remove(name);
            drop(reg);
            sweep::retire(evicted);
            true
        }
        None => false,
    }
}

pub fn insert_or_get(index: VectorIndex) -> Result<(Arc<VectorIndex>, bool), TryReserveError> {
    sweep::ensure_sweeper();
    index.ann.set_bell();
    let mut reg = write();
    reg.try_reserve(1)?;
    let mut inserted = false;
    let entry = reg.entry(index.name.clone()).or_insert_with(|| {
        inserted = true;
        index.publish();
        Arc::new(index)
    });
    Ok((Arc::clone(entry), inserted))
}

pub fn remove(name: &str) -> bool {
    let evicted = write().remove(name);
    let had = evicted.is_some();
    sweep::retire(evicted);
    had
}

pub fn remove_observed(name: &str, observed: &VectorIndex) -> bool {
    let mut reg = write();
    match reg.get(name) {
        Some(current) if std::ptr::eq(Arc::as_ptr(current), observed) => {
            let evicted = reg.remove(name);
            drop(reg);
            sweep::retire(evicted);
            true
        }
        _ => false,
    }
}

/// `Err` means the listing could not be built under memory pressure -- not
/// that the registry is empty. Callers that would act on the difference
/// (the replication master, telling a replica what exists) must not collapse
/// the two; callers that treat "nothing to do this round" as harmless either
/// way (the sweeper) may flatten it with `unwrap_or_default`.
pub fn indexes() -> Result<Vec<Arc<VectorIndex>>, TryReserveError> {
    let reg = read();
    let mut all = Vec::new();
    all.try_reserve(reg.len())?;
    all.extend(reg.values().cloned());
    Ok(all)
}

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
        let ann = AnnIndex::new(
            Layout::new(4, Quant::F32),
            Metric::L2,
            0,
            0,
            0,
            Arc::new(crate::handler::arcus::engine::ItemElements),
        )
        .expect("build the graph");
        VectorIndex::ours(name.to_owned(), ann, 8)
    }

    fn building(name: &str) -> VectorIndex {
        let made = index(name);
        made.mark_state(BUILDING);
        made
    }

    #[test]
    fn a_verdict_older_than_the_entry_releases_nothing() {
        let name = "registry-test-late-verdict";
        let stamp = now();

        let (registered, _) = put(index(name)).expect("claim the name");
        registered.publish();

        assert!(
            !remove_if_stale(name, stamp),
            "a verdict from before the entry existed must release nothing"
        );
        assert!(get(name).is_some(), "the new index survives");
        remove(name);
    }

    #[test]
    fn an_entry_still_being_built_is_never_released() {
        let name = "registry-test-unpublished";
        let (registered, _) = put(building(name)).expect("claim the name");

        assert!(
            !remove_if_stale(name, now()),
            "an entry whose Map is still being written must survive any verdict"
        );
        registered.publish();
        registered.mark_serving();
        assert!(
            remove_if_stale(name, now()),
            "once it stands on its Map it is releasable like any other"
        );
    }

    #[test]
    fn unput_leaves_a_displacing_entry_alone() {
        let name = "registry-test-unput-displaced";
        let (mine, _) = put(index(name)).expect("claim the name");
        let (theirs, displaced) = put(index(name)).expect("displace it");
        assert!(
            displaced.is_some_and(|d| Arc::ptr_eq(&d, &mine)),
            "the second put displaces the first"
        );

        unput(&mine, None);

        assert!(
            get(name).is_some_and(|live| Arc::ptr_eq(&live, &theirs)),
            "undoing the displaced entry must not remove the one that displaced it"
        );
        unput(&theirs, None);
        assert!(get(name).is_none(), "the owner of the entry can undo it");
    }

    #[test]
    fn a_stale_release_leaves_a_newly_registered_index_alone() {
        let name = "registry-test-stale-release";
        let (observed, _) = insert_or_get(index(name)).expect("register the first");

        assert!(remove(name));
        let (current, inserted) = insert_or_get(index(name)).expect("register the second");
        assert!(inserted, "the name was free");

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
