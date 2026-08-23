//! An entry pairs the Map with the graph built from it and the `owner` token it was built under; [`super::recovery`] judges that token.

use std::collections::{HashMap, TryReserveError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use crate::handler::usearch::AnnIndex;

/// `owner` while a rebuild is in flight: claimed by nobody.
pub const REBUILDING: u64 = 0;

/// Ticks once per publish, and orders publishes against the reads that judge them.
///
/// A release is always decided on something read earlier — a metadata read, an engine call's
/// `KeyGone` — and by the time it reaches the registry the name can hold an index registered
/// since, for a Map that exists. [`now`] taken before the read says what "already there" means
/// for that verdict, and [`remove_if_stale`] refuses anything newer.
static CLOCK: AtomicU64 = AtomicU64::new(1);

/// The reading a verdict carries. One atomic load — not a lookup, and it takes no lock, which
/// is what lets the metadata read stay first.
pub fn now() -> u64 {
    CLOCK.load(Ordering::Acquire)
}

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,
    /// Element-count limit for vectors, already excluding the metadata element.
    pub maxcount: u32,
    owner: AtomicU64,
    /// When this entry became answerable, and `0` until then.
    ///
    /// `vcreate` registers before it writes to the engine, so for a moment the name holds an
    /// index whose Map does not exist yet. A command reading the metadata right then finds no
    /// Map and is right about what it saw — the entry has to say "not yet" itself, because
    /// nothing outside it can tell that verdict from a true one.
    published: AtomicU64,
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
            owner: AtomicU64::new(owner),
            published: AtomicU64::new(0),
            #[cfg(recovery)]
            refilled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn owner(&self) -> u64 {
        self.owner.load(Ordering::Acquire)
    }

    /// Mark the entry answerable: its Map exists now.
    ///
    /// Stamped with the clock *before* the tick, so a verdict that read the same value was
    /// reached no later than this publish and leaves the entry alone. Only a verdict taken
    /// after it — which reads a higher value — can release it.
    pub fn publish(&self) {
        self.published
            .store(CLOCK.fetch_add(1, Ordering::AcqRel), Ordering::Release);
    }

    /// `0` while the Map this entry describes has not been written yet.
    fn published_at(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    pub fn is_rebuilding(&self) -> bool {
        self.owner() == REBUILDING
    }

    /// Whether the graph is full again and only the token write is outstanding.
    #[cfg(recovery)]
    pub fn is_refilled(&self) -> bool {
        self.refilled.load(Ordering::Acquire)
    }

    #[cfg(recovery)]
    pub(super) fn mark_refilled(&self) {
        self.refilled.store(true, Ordering::Release);
    }

    #[cfg(recovery)]
    pub(super) fn set_owner(&self, owner: u64) {
        self.owner.store(owner, Ordering::Release);
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

/// Claim `name` for an index whose Map does not exist yet.
///
/// `vcreate` registers first and writes to the engine last, which is the rule everywhere here:
/// the engine write emits `CLOG_MAP_ELEM_INSERT` and is what carries the create off this node,
/// so everything that can fail belongs in front of it. Growing this map is one of those things.
///
/// The entry lands unpublished, and nothing releases an unpublished entry — that is what covers
/// the stretch where the name is claimed and the Map is not there yet, with no lock held across
/// the engine call. Call [`VectorIndex::publish`] once the Map exists, or [`unput`] if it never
/// does.
///
/// Returns the registered index and whatever the name held before.
pub fn put(
    index: VectorIndex,
) -> Result<(Arc<VectorIndex>, Option<Arc<VectorIndex>>), TryReserveError> {
    let mut reg = write();
    reg.try_reserve(1)?;
    let index = Arc::new(index);
    let previous = reg.insert(index.name.clone(), Arc::clone(&index));
    Ok((index, previous))
}

/// Undo a [`put`] whose Map never got written, restoring what the name held before.
///
/// Only while the name still holds `ours`: a second `vcreate` racing on the same name displaces
/// it, and that one's entry is not this call's to take back. Allocation-free either way — the
/// key is already in the map.
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
            reg.remove(name);
        }
    }
}

/// Release `name` if what it holds was already answerable when `stamp` was taken.
///
/// Two entries are spared. One published after `stamp` belongs to a Map the verdict never
/// looked at. One not published at all is a `vcreate` mid-flight, and its Map is on its way.
pub fn remove_if_stale(name: &str, stamp: u64) -> bool {
    let mut reg = write();
    match reg.get(name) {
        Some(current) => {
            let at = current.published_at();
            if at == 0 || at >= stamp {
                return false;
            }
            reg.remove(name);
            true
        }
        None => false,
    }
}

/// Register `index` unless the name is taken, returning whichever ends up live.
///
/// For adoptions, where the Map is already there: the entry is published as it goes in.
pub fn insert_or_get(index: VectorIndex) -> Result<(Arc<VectorIndex>, bool), TryReserveError> {
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

/// Every registered name, rebuilding ones included.
///
/// For the sweep, which asks about Maps rather than about what is servable: an index whose Map
/// went while it was being rebuilt is exactly one worth releasing.
pub fn names() -> Vec<String> {
    read().keys().cloned().collect()
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

    /// The race the clock guards: a command reads the Map, finds it gone, and only reaches the
    /// registry after a `vcreate` has put a live index under the same name. Without the stamp
    /// the release takes out the new one, leaving a Map nothing serves.
    #[test]
    fn a_verdict_older_than_the_entry_releases_nothing() {
        let name = "registry-test-late-verdict";
        let stamp = now();

        // The create lands after the verdict was reached.
        let (registered, _) = put(index(name)).expect("claim the name");
        registered.publish();

        assert!(
            !remove_if_stale(name, stamp),
            "a verdict from before the entry existed must release nothing"
        );
        assert!(get(name).is_some(), "the new index survives");
        remove(name);
    }

    /// `vcreate` claims the name before it writes the Map, so for a moment the entry describes
    /// an index with no Map. A command reading the metadata right then is right about what it
    /// saw, and must still leave the entry alone.
    #[test]
    fn an_unpublished_entry_is_never_released() {
        let name = "registry-test-unpublished";
        let (registered, _) = put(index(name)).expect("claim the name");

        assert!(
            !remove_if_stale(name, now()),
            "an entry whose Map is still being written must survive any verdict"
        );

        registered.publish();
        assert!(
            remove_if_stale(name, now()),
            "once published it is releasable like any other"
        );
    }

    /// A racing `vcreate` displaced this call's entry, so the undo is not its to make.
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
