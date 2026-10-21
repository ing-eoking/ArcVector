//! A small thread pool for work outside the request path.
//!
//! Element event callbacks run **while holding the engine's cache lock**; updating the graph
//! there would stall the whole daemon. So the callback only enqueues graph work and returns at
//! once, and threads here do the work. Searches run here too.
//!
//! For the same reason the callback never spawns threads on demand: `pthread_create` is a
//! syscall that can fail and has no place under the cache lock. Threads are started up front.
//!
//! Each thread has its own queue (lane), and jobs on a lane run in submission order. Graph work
//! picks its lane by the element's vkey hash (`submit_keyed`), so work for the same vkey runs on
//! one thread in event order. Searches use the least busy lane.

use std::cell::UnsafeCell;
use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};

/// Number of threads, one lane (queue) each.
///
/// Recovery is one sequential job per index, so this is also the number of indexes that can
/// recover concurrently. It is generous so that a node with many indexes does not recover them
/// all in a single line when starting up or catching up on replication.
///
/// Idle threads sleep on a condvar and use no CPU. The count only matters when that many indexes
/// actually recover at once, competing with memcached workers for CPU -- indexes under recovery
/// do not serve queries yet, so in exchange each one comes online sooner.
const WORKERS: usize = 32;

/// Link for a lane list. Embedded in the job struct and submitted by its address.
///
/// The list allocates no nodes of its own, so submitting never allocates, never fails, and has no
/// length limit. The point is that `submit` is also called under the cache lock (from the link
/// callback), so it must not call `malloc`.
///
/// Commands submit at most one link per connection -- the connection's next command is not read
/// until it gets a reply. Graph work from replication or replay is one per element and can pile
/// up while catching up; its nodes are created by the callback (`offload::enqueue`), and if
/// allocation fails only that job is dropped.
pub(crate) struct Entry {
    /// Next link in the list. Only touched under the lane mutex.
    next: UnsafeCell<*const Entry>,
    /// Called by the lane thread with this link's address; finds the enclosing job and runs it.
    run: unsafe fn(*const Entry),
}

// `next` is only read and written under the lane mutex; the rest is immutable.
unsafe impl Send for Entry {}
unsafe impl Sync for Entry {}

impl Entry {
    pub(crate) const fn new(run: unsafe fn(*const Entry)) -> Self {
        Self {
            next: UnsafeCell::new(ptr::null()),
            run,
        }
    }
}

/// A lane's FIFO queue.
struct List {
    head: *const Entry,
    tail: *const Entry,
}

// Submitters keep the links alive as promised by `submit`.
unsafe impl Send for List {}

impl List {
    const fn new() -> Self {
        Self {
            head: ptr::null(),
            tail: ptr::null(),
        }
    }

    /// # Safety
    ///
    /// `entry` must be alive and not linked into any list.
    unsafe fn push_back(&mut self, entry: *const Entry) {
        unsafe { *(*entry).next.get() = ptr::null() };
        if self.tail.is_null() {
            self.head = entry;
        } else {
            unsafe { *(*self.tail).next.get() = entry };
        }
        self.tail = entry;
    }

    fn pop_front(&mut self) -> Option<*const Entry> {
        if self.head.is_null() {
            return None;
        }
        let entry = self.head;
        self.head = unsafe { *(*entry).next.get() };
        if self.head.is_null() {
            self.tail = ptr::null();
        }
        Some(entry)
    }
}

/// A thread and its queue.
///
/// A lane's jobs are run by its single thread in submission order. Routing graph work with the
/// same key to one lane is how that ordering is used (`order`).
struct Lane {
    queue: Mutex<List>,
    wake: Condvar,
    /// Queued plus running jobs. Read without the lock to pick the least busy lane; a snapshot
    /// that need not be exact.
    load: AtomicUsize,
    /// Whether the thread started. A lane without a thread would never run its jobs, so it
    /// accepts none.
    alive: AtomicBool,
}

struct Pool {
    lanes: Box<[Lane]>,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| {
    let lanes: Box<[Lane]> = (0..WORKERS)
        .map(|_| Lane {
            queue: Mutex::new(List::new()),
            wake: Condvar::new(),
            load: AtomicUsize::new(0),
            alive: AtomicBool::new(false),
        })
        .collect();
    // Threads block in `lane()` until POOL is fully initialized.
    for (n, lane) in lanes.iter().enumerate() {
        let spawned = std::thread::Builder::new()
            .name(format!("arcvector-work-{n}"))
            .spawn(move || run(n));
        match spawned {
            Ok(_) => lane.alive.store(true, Ordering::Release),
            Err(e) => eprintln!("ArcVector: could not spawn a worker thread: {e}"),
        }
    }
    Pool { lanes }
});

/// Starts the pool if it is not running.
///
/// As with `ensure_sweeper`, this happens when an index enters the registry rather than at load
/// time: `-d` forks **after** loading the extension, so threads created earlier do not survive.
pub(crate) fn ensure_pool() {
    LazyLock::force(&POOL);
}

/// Number of lanes. Keyed work goes to lane `hash % lanes()`.
#[cfg(test)]
pub(crate) const fn lanes() -> usize {
    WORKERS
}

fn lane(n: usize) -> &'static Lane {
    &POOL.lanes[n]
}

fn queue(lane: &Lane) -> MutexGuard<'_, List> {
    lane.queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Submits a job to the least busy lane.
///
/// Returns `false` if no lane accepts it (no thread could be started); the link then remains
/// the submitter's.
///
/// # Safety
///
/// `entry` must stay alive until the lane thread calls its `run`, and must not be linked into
/// another list meanwhile.
pub(crate) unsafe fn submit(entry: *const Entry) -> bool {
    let lanes = &POOL.lanes;
    let idlest = (0..lanes.len())
        .filter(|&n| lanes[n].alive.load(Ordering::Acquire))
        .min_by_key(|&n| lanes[n].load.load(Ordering::Relaxed));
    match idlest {
        Some(n) => unsafe { submit_to(n, entry) },
        None => false,
    }
}

/// Submits to the lane chosen by the key hash. The same hash always goes to the same lane and
/// runs in submission order.
///
/// If the chosen lane has no thread, the next live lane is used. Which lanes are alive is fixed
/// at startup, so the same hash still always maps to the same lane. Returns `false` if no lane is
/// alive.
///
/// # Safety
///
/// Same as [`submit`].
pub(crate) unsafe fn submit_keyed(hash: u64, entry: *const Entry) -> bool {
    let lanes = &POOL.lanes;
    let first = (hash % lanes.len() as u64) as usize;
    let alive = (0..lanes.len())
        .map(|step| (first + step) % lanes.len())
        .find(|&n| lanes[n].alive.load(Ordering::Acquire));
    match alive {
        Some(n) => unsafe { submit_to(n, entry) },
        None => false,
    }
}

/// Submits to lane `lane_no`. Returns `false` if that lane has no thread.
///
/// Jobs on one lane run in submission order.
///
/// # Safety
///
/// Same as [`submit`].
pub(crate) unsafe fn submit_to(lane_no: usize, entry: *const Entry) -> bool {
    let Some(lane) = POOL.lanes.get(lane_no) else {
        return false;
    };
    if !lane.alive.load(Ordering::Acquire) {
        return false;
    }
    let mut queue = queue(lane);
    unsafe { queue.push_back(entry) };
    lane.load.fetch_add(1, Ordering::Relaxed);
    drop(queue);
    lane.wake.notify_one();
    true
}

fn run(n: usize) {
    let lane = lane(n);
    loop {
        let entry = {
            let mut queue = queue(lane);
            loop {
                if let Some(entry) = queue.pop_front() {
                    break entry;
                }
                queue = lane
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // If one panicking job killed the lane thread, jobs on that lane would never run again.
        // Drop the job, keep the thread.
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| unsafe { ((*entry).run)(entry) }));
        if ran.is_err() {
            eprintln!("ArcVector: a job on worker lane {n} panicked; the lane keeps running");
        }
        lane.load.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;

    /// A job for observing lane ordering. The link is the first field, so its address is the
    /// probe's address.
    #[repr(C)]
    struct Probe {
        entry: Entry,
        f: Box<dyn FnOnce() + Send>,
    }

    unsafe fn run_probe(entry: *const Entry) {
        // `entry` is the first field of a `Probe` handed over by `probe` via `Box::into_raw`.
        let probe = unsafe { Box::from_raw(entry.cast_mut().cast::<Probe>()) };
        (probe.f)();
    }

    /// Submits `f` to `lane`.
    fn probe(lane: usize, f: impl FnOnce() + Send + 'static) -> bool {
        let raw = Box::into_raw(Box::new(Probe {
            entry: Entry::new(run_probe),
            f: Box::new(f),
        }));
        let entry = unsafe { &raw const (*raw).entry };
        if unsafe { submit_to(lane, entry) } {
            return true;
        }
        drop(unsafe { Box::from_raw(raw) });
        false
    }

    #[test]
    fn jobs_on_one_lane_run_in_submission_order() {
        ensure_pool();
        let (tx, rx) = channel();
        for n in 0..100 {
            let tx = tx.clone();
            let queued = probe(3, move || {
                // Leave room for reordering: a job taken by another lane would overtake here.
                if n % 7 == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let _ = tx.send(n);
            });
            assert!(queued, "lane 3 is alive");
        }
        let got: Vec<i32> = (0..100)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        assert_eq!(got, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn a_busy_lane_keeps_taking_jobs_without_a_bound() {
        ensure_pool();
        let (hold_tx, hold_rx) = channel::<()>();
        let (ran_tx, ran_rx) = channel::<()>();
        // Block the lane thread so the queue does not drain.
        assert!(probe(5, move || {
            let _ = ran_tx.send(());
            let _ = hold_rx.recv();
        }));
        ran_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        // Far beyond the old bound (256), everything is accepted and runs once released.
        let (done_tx, done_rx) = channel::<()>();
        for _ in 0..1000 {
            let done_tx = done_tx.clone();
            assert!(probe(5, move || {
                let _ = done_tx.send(());
            }));
        }
        drop(hold_tx);
        for _ in 0..1000 {
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    }

    #[test]
    fn there_are_as_many_lanes_as_workers() {
        assert_eq!(lanes(), WORKERS);
        assert!(!probe(lanes(), || {}), "a lane past the end takes nothing");
    }

    #[test]
    fn the_list_pops_in_push_order_and_empties() {
        unsafe fn nothing(_: *const Entry) {}
        let entries = [
            Entry::new(nothing),
            Entry::new(nothing),
            Entry::new(nothing),
        ];
        let mut list = List::new();
        for entry in &entries {
            unsafe { list.push_back(entry) };
        }
        for entry in &entries {
            assert_eq!(list.pop_front(), Some(entry as *const Entry));
        }
        assert_eq!(list.pop_front(), None);
        // Reusable after being emptied.
        unsafe { list.push_back(&entries[1]) };
        assert_eq!(list.pop_front(), Some(&entries[1] as *const Entry));
    }
}
