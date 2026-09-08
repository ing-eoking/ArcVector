use std::collections::{HashMap, TryReserveError};
#[cfg(recovery)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};

use crate::handler::access::sweep;
use crate::handler::usearch::AnnIndex;
use crate::owner;

pub const SERVING: u8 = 0;

pub const DRAINING: u8 = 1;

pub const COLD: u8 = 2;

pub const FILLING: u8 = 3;

pub const BUILDING: u8 = 4;

static CLOCK: AtomicU64 = AtomicU64::new(1);

pub fn now() -> u64 {
    CLOCK.load(Ordering::Acquire)
}

#[cfg(recovery)]
struct Seat<'a>(&'a AtomicUsize);

#[cfg(recovery)]
impl Drop for Seat<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,

    pub maxcount: u32,
    state: AtomicU8,

    /// The owner token last recorded for this index, read back by
    /// `stamped_as`. Kept separate from `crate::owner::ours()` because a
    /// replica's `stamp` skips its own write, so its Map keeps whatever
    /// owner was already there -- usually `owner::NOBODY`, the value
    /// `drain` last wrote. Recording that same value here, instead of
    /// always assuming our own token, is what keeps `resolve`'s comparison
    /// against the Map stable instead of drifting into a permanent re-drain.
    owner: Mutex<String>,

    published: AtomicU64,

    last_access: AtomicU64,

    #[cfg(recovery)]
    pub(super) refilled: std::sync::atomic::AtomicBool,
    #[cfg(recovery)]
    done: (std::sync::Mutex<()>, std::sync::Condvar),
    #[cfg(recovery)]
    waiting: AtomicUsize,
    #[cfg(recovery)]
    rebuild_size: AtomicUsize,
}

impl VectorIndex {
    pub fn ours(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        let index = Self::new(name, ann, maxcount, SERVING);
        index.mark_ours();
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
            owner: Mutex::new(String::new()),
            published: AtomicU64::new(0),
            last_access: AtomicU64::new(0),
            #[cfg(recovery)]
            refilled: std::sync::atomic::AtomicBool::new(false),
            #[cfg(recovery)]
            done: (std::sync::Mutex::new(()), std::sync::Condvar::new()),
            #[cfg(recovery)]
            waiting: AtomicUsize::new(0),
            #[cfg(recovery)]
            rebuild_size: AtomicUsize::new(0),
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// The owner token as of this call, copied out.
    ///
    /// This allocates, which used to rule it out for `resolve` -- a clone on
    /// every request, thrown away one comparison later, was exactly the cost
    /// this crate was trying to cut. Now that a recovery build's `resolve`
    /// only reaches this comparison on first touch (a registry miss), the
    /// clone is cheap enough there and `stamped_as` is what it calls; a
    /// build with neither replication nor persistence still runs the
    /// comparison on every request and uses `owned_by` instead, below.
    pub fn stamped_as(&self) -> String {
        if self.state() == SERVING {
            self.owner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        } else {
            owner::NOBODY.to_owned()
        }
    }

    /// Whether this index is currently serving under exactly `token`.
    ///
    /// Same answer as `token == self.stamped_as()`, but without allocating a
    /// `String` to throw away immediately after -- which is why every build
    /// that asks this question on the request path uses it. Only a
    /// `replication` build does not: there the replication stream reports a
    /// takeover directly, so `resolve` reads the owner on first touch alone
    /// (`access::map_was_taken_over`), and by then a clone is cheap enough that
    /// it uses `stamped_as`.
    #[cfg_attr(all(recovery, feature = "replication"), expect(dead_code))]
    pub(in crate::handler) fn owned_by(&self, token: &str) -> bool {
        if self.state() == SERVING {
            *self.owner.lock().unwrap_or_else(PoisonError::into_inner) == token
        } else {
            token == owner::NOBODY
        }
    }

    pub fn publish(&self) {
        self.published
            .store(CLOCK.fetch_add(1, Ordering::AcqRel), Ordering::Release);
    }

    fn touch(&self) {
        let now = crate::server::coarse_now();
        if self.last_access.load(Ordering::Relaxed) != now {
            self.last_access.store(now, Ordering::Relaxed);
        }
    }

    fn accessed_at(&self) -> u64 {
        self.last_access.load(Ordering::Relaxed)
    }

    fn published_at(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    pub fn is_rebuilding(&self) -> bool {
        self.state() != SERVING
    }

    #[cfg(recovery)]
    pub fn is_refilled(&self) -> bool {
        self.refilled.load(Ordering::Acquire)
    }

    #[cfg(recovery)]
    pub(super) fn set_rebuild_size(&self, elements: usize) {
        self.rebuild_size.store(elements, Ordering::Release);
    }

    #[cfg(recovery)]
    pub fn rebuild_size(&self) -> usize {
        self.rebuild_size.load(Ordering::Acquire)
    }

    #[cfg(recovery)]
    pub(super) fn take_refilled(&self) -> bool {
        self.refilled.swap(false, Ordering::AcqRel)
    }

    #[cfg(recovery)]
    pub(super) fn mark_refilled(&self) {
        let _held = self.done.0.lock().unwrap_or_else(PoisonError::into_inner);
        self.refilled.store(true, Ordering::Release);
        self.done.1.notify_all();
    }

    #[cfg(recovery)]
    pub fn await_refill(&self, limit: std::time::Duration, seats: usize) -> bool {
        if self.is_refilled() {
            return true;
        }
        if self
            .waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < seats).then_some(n + 1)
            })
            .is_err()
        {
            return false;
        }
        let seated = Seat(&self.waiting);

        let deadline = std::time::Instant::now() + limit;
        let mut held = self.done.0.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.is_refilled() {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            held = self
                .done
                .1
                .wait_timeout(held, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(held);
        drop(seated);
        self.is_refilled()
    }

    /// How far a rebuild has got, or `None` when the graph answers in full.
    ///
    /// The two counts are read separately, so this is a moment rather than a
    /// transaction — good enough to report, not to decide on.
    #[cfg(recovery)]
    pub fn rebuilding_progress(&self) -> Option<crate::error::Rebuild> {
        (self.state() == FILLING).then(|| crate::error::Rebuild {
            done: self.ann.len(),
            total: self.rebuild_size(),
        })
    }

    /// Without recovery a graph is never rebuilt, so a read is never partial.
    #[cfg(not(recovery))]
    pub fn rebuilding_progress(&self) -> Option<crate::error::Rebuild> {
        None
    }

    pub(in crate::handler) fn mark_ours(&self) {
        self.mark_owned_by(owner::ours());
    }

    /// Stamps this index as serving under a given owner token, rather than
    /// assuming it is always our own. `recovery::claim_refilled` uses this on
    /// a replica, where the token that ends up matching the Map is whatever
    /// the Map already holds -- not `owner::ours()`, since a replica's
    /// `stamp` never gets to write that.
    pub(in crate::handler) fn mark_owned_by(&self, token: &str) {
        *self.owner.lock().unwrap_or_else(PoisonError::into_inner) = token.to_owned();
        self.state.store(SERVING, Ordering::Release);
    }

    #[cfg(recovery)]
    pub(super) fn enter(&self, was: u8, now: u8) -> bool {
        self.state
            .compare_exchange(was, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    // `not(test)`: the unit tests drive state transitions through this
    // directly, so under `--all-targets` it is live in every configuration and
    // an unqualified expectation is itself the warning.
    #[cfg_attr(all(not(recovery), not(test)), expect(dead_code))]
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
    let index = read().get(name).cloned();
    if let Some(index) = &index {
        index.touch();
    }
    index
}

pub fn contains(name: &str) -> bool {
    read().contains_key(name)
}

pub fn put(
    index: VectorIndex,
) -> Result<(Arc<VectorIndex>, Option<Arc<VectorIndex>>), TryReserveError> {
    sweep::ensure_sweeper();
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

/// The least recently used indexes, for the sweeper to look at.
///
/// Handing back the indexes rather than their names is what keeps the rotation
/// honest: a second lookup would go through `get`, which touches what it finds,
/// and a sweep that warms everything it inspects never reaches a cold name.
pub(in crate::handler) fn coldest(limit: usize) -> Vec<Arc<VectorIndex>> {
    let reg = read();
    let names = coldest_of(
        reg.iter()
            .filter(|(_, index)| index.state() != BUILDING)
            .map(|(name, index)| (index.accessed_at(), name.as_str())),
        limit,
    );
    names
        .iter()
        .filter_map(|name| reg.get(name).cloned())
        .collect()
}

fn coldest_of<'a>(entries: impl Iterator<Item = (u64, &'a str)>, limit: usize) -> Vec<String> {
    let mut all: Vec<(u64, &str)> = Vec::new();
    for entry in entries {
        if all.try_reserve(1).is_err() {
            return Vec::new();
        }
        all.push(entry);
    }
    all.sort_unstable();
    all.truncate(limit);
    all.into_iter().map(|(_, name)| name.to_string()).collect()
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
            Arc::new(crate::handler::arcus::engine::DetachedElements),
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
        assert!(
            !coldest(10).iter().any(|index| index.name == name),
            "and it is not offered for sweeping either"
        );

        registered.publish();
        registered.mark_ours();
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

#[cfg(test)]
mod coldest_tests {
    use super::coldest_of;

    const ENTRIES: [(u64, &str); 4] = [(30, "warm"), (10, "cold"), (40, "hot"), (20, "cool")];

    fn coldest(limit: usize) -> Vec<String> {
        coldest_of(ENTRIES.iter().copied(), limit)
    }

    #[test]
    fn the_longest_untouched_names_come_first() {
        assert_eq!(coldest(2), ["cold", "cool"]);
    }

    #[test]
    fn a_registry_smaller_than_the_batch_comes_back_whole() {
        assert_eq!(coldest(8), ["cold", "cool", "warm", "hot"]);
    }

    #[test]
    fn an_empty_registry_gives_nothing() {
        assert!(coldest_of(std::iter::empty(), 8).is_empty());
        assert!(coldest(0).is_empty(), "a zero batch asks for nothing");
    }
}

#[cfg(all(test, recovery))]
mod refill_wait_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    struct NoElements;

    impl crate::handler::usearch::Elements for NoElements {
        fn id_at(&self, _addr: u64) -> Option<Arc<str>> {
            None
        }
        fn release(&self, _addrs: &[u64]) {}
    }

    fn rebuilding(name: &str) -> Arc<VectorIndex> {
        let ann = crate::handler::usearch::AnnIndex::new(
            crate::handler::arcus::element::Layout::new(4, crate::handler::quant::Quant::F32),
            crate::handler::usearch::Metric::L2,
            16,
            64,
            64,
            Arc::new(NoElements),
        )
        .expect("build a graph");
        Arc::new(VectorIndex::rebuilding(name.to_owned(), ann, 10))
    }

    #[test]
    fn only_a_filling_index_reports_progress() {
        let index = rebuilding("half");
        assert_eq!(
            index.rebuilding_progress(),
            None,
            "a cold index has no rebuild in flight"
        );

        assert!(index.enter(COLD, FILLING));
        index.set_rebuild_size(999_000);
        assert_eq!(
            index.rebuilding_progress(),
            Some(crate::error::Rebuild {
                done: 0,
                total: 999_000
            }),
            "filling, with nothing published yet"
        );

        index.mark_ours();
        assert_eq!(
            index.rebuilding_progress(),
            None,
            "a serving index answers in full"
        );
    }

    #[test]
    fn a_read_gives_up_when_the_refill_takes_too_long() {
        let index = rebuilding("slow");
        let began = Instant::now();
        assert!(!index.await_refill(Duration::from_millis(60), 1));
        assert!(began.elapsed() >= Duration::from_millis(55), "it waited");
        assert!(began.elapsed() < Duration::from_secs(2), "and gave up");
    }

    #[test]
    fn a_read_wakes_as_soon_as_the_refill_lands() {
        let index = rebuilding("quick");
        let other = Arc::clone(&index);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            other.mark_refilled();
        });

        let began = Instant::now();
        assert!(index.await_refill(Duration::from_secs(5), 1));
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "it woke on the signal rather than on the timeout"
        );
    }

    #[test]
    fn only_as_many_workers_wait_as_there_are_seats() {
        let index = rebuilding("seats");
        let seated = Arc::new(AtomicUsize::new(0));

        let holder = Arc::clone(&index);
        let counted = Arc::clone(&seated);
        let sleeper = std::thread::spawn(move || {
            counted.fetch_add(1, Ordering::Release);
            holder.await_refill(Duration::from_millis(400), 1)
        });
        while seated.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(40));

        let began = Instant::now();
        assert!(!index.await_refill(Duration::from_millis(400), 1));
        assert!(
            began.elapsed() < Duration::from_millis(30),
            "the seat was taken, so it came back at once instead of sleeping"
        );

        index.mark_refilled();
        assert!(sleeper.join().unwrap());
    }

    #[test]
    fn the_size_to_rebuild_is_what_the_snapshot_held() {
        let index = rebuilding("sized");
        assert_eq!(index.rebuild_size(), 0, "nothing queued yet");

        index.set_rebuild_size(4096);
        assert_eq!(index.rebuild_size(), 4096);
    }

    #[test]
    fn the_finished_refill_is_claimed_once() {
        let index = rebuilding("once");
        index.mark_refilled();
        assert!(index.take_refilled());
        assert!(!index.take_refilled(), "the loser must not claim it again");
    }
}
