use std::collections::{HashMap, TryReserveError};
#[cfg(recovery)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use crate::handler::access::sweep;
use crate::handler::usearch::AnnIndex;

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
    stamped: AtomicBool,

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
        Self::new(name, ann, maxcount, true)
    }

    pub fn rebuilding(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        Self::new(name, ann, maxcount, false)
    }

    fn new(name: String, ann: AnnIndex, maxcount: u32, stamped: bool) -> Self {
        Self {
            name,
            ann,
            maxcount,
            stamped: AtomicBool::new(stamped),
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

    pub fn stamped_as(&self) -> &'static str {
        if self.stamped.load(Ordering::Acquire) {
            crate::owner::ours()
        } else {
            crate::owner::NOBODY
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
        !self.stamped.load(Ordering::Acquire)
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

    #[cfg(recovery)]
    pub(super) fn mark_ours(&self) {
        self.stamped.store(true, Ordering::Release);
    }

    #[cfg(recovery)]
    pub(super) fn mark_rebuilding(&self) {
        self.stamped.store(false, Ordering::Release);
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
            let at = current.published_at();
            if at == 0 || at >= stamp {
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

pub(in crate::handler) fn indexes() -> Vec<Arc<VectorIndex>> {
    let reg = read();
    let mut all = Vec::new();
    if all.try_reserve(reg.len()).is_err() {
        return Vec::new();
    }
    all.extend(reg.values().cloned());
    all
}

pub(in crate::handler) fn coldest(limit: usize) -> Vec<String> {
    let reg = read();
    coldest_of(
        reg.iter()
            .filter(|(_, index)| index.published_at() != 0)
            .map(|(name, index)| (index.accessed_at(), name.as_str())),
        limit,
    )
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
