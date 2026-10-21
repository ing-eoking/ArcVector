//! Decides, by queue order, when an item reference may be handed back to the engine.
//!
//! Searches and retires go into the same FIFO, so "who came first" is simply queue order. A
//! barrier ahead of a `Release` is a search that started before those addresses left the graph,
//! so it must block them. A barrier behind it started afterwards and cannot reach those addresses
//! (`resolve()` re-checks the graph too), so there is no reason to block.
//!
//! Full argument: `docs/superpowers/specs/2026-09-16-arcvector-retire-barrier-queue-design.md`.

use crate::index::elements::Elements;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Size of the ring holding addresses to release.
///
/// Allocated once at startup and never grown. `retire` is called from the unlink callback while
/// the engine's cache lock is held, and that path must not call `malloc`.
///
/// It only fills up when deletes pile up while the sweeper is blocked on the front barrier. On
/// overflow `retire` returns `false`; the caller locks the index (`Halt`) and lets the engine
/// reclaim the item on the spot.
pub(super) const RING: usize = 4096;

/// Extra event capacity reserved on top of `CAP`.
///
/// `retire` refuses (`false`) unless two slots are free, so the queue never grows past this
/// margin. Overflow is handled by `Halt`.
const EVENT_MARGIN: usize = 256;

/// Addresses passed to `release` per call. Uses a stack buffer, so no heap.
const RELEASE_CHUNK: usize = 64;

/// Base capacity of the event queue.
pub(super) const CAP: usize = 1024;

/// Occupancy (%) below which the lock may be lifted. Locked when full, unlocked here.
const RESUME_AT: usize = 90;

/// Number of barrier slots.
///
/// A slot only advances when its barrier enters the queue, so live barriers never outnumber queue
/// entries. With one extra open slot this size cannot wrap. The `debug_assert` in `retire()` is
/// the tripwire.
pub(super) const POOL: usize = CAP + 2;

/// Top bit: the barrier is sealed and accepts no new searches.
const SEALED: usize = 1 << (usize::BITS - 1);

/// Remaining bits: number of attached searches.
const COUNT: usize = !SEALED;

/// Queue entries.
enum Event {
    /// The sweeper stops here until this slot's barrier clears.
    Barrier(usize),
    /// Addresses to release extend up to this ring position. The start is always `Queue::head`
    /// and entries are contiguous, so the end alone defines the span. Positions increase
    /// monotonically; `% RING` picks the cell.
    Release(usize),
}

#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    /// Start of the not-yet-released ring span. Advanced by `drain_once` after releasing.
    head: usize,
    /// Next ring position to write. Advanced by `retire`.
    tail: usize,
    /// Someone has dropped the mutex and is calling `release`.
    ///
    /// That span's entry is still in the queue; without this flag a second drainer would pick up
    /// the same entry and double-free it.
    draining: bool,
}

/// Guard that always resets `draining`.
///
/// The normal path calls `disarm()`. On panic in between, `Drop` only clears the flag and leaves
/// `head` alone -- having the next drainer re-release an unreleased span is better than skipping
/// it and pinning those references forever.
struct Draining<'a> {
    owner: &'a Retirement,
    armed: bool,
}

impl<'a> Draining<'a> {
    fn new(owner: &'a Retirement) -> Self {
        Self { owner, armed: true }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Draining<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.owner
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .draining = false;
        }
    }
}

/// Attaches one search to the slot. Backs out and returns `false` if it was sealed.
///
/// The key point is that `fetch_add` returns the SEALED bit atomically with the count. With the
/// flag and count in separate words, this check and the one in `seal` could miss each other
/// (Dekker), requiring `SeqCst`.
fn attach(owner: &Retirement, cell: &AtomicUsize) -> bool {
    let prev = cell.fetch_add(1, Ordering::AcqRel);
    if prev & SEALED != 0 {
        let before_backout = cell.fetch_sub(1, Ordering::AcqRel);
        if before_backout & COUNT == 1 && before_backout & SEALED != 0 {
            // Backing out brought this barrier's count to zero. Same check as
            // `Reading::drop` -- the sweeper may be waiting on this barrier, so ring. If nobody
            // is waiting (the barrier is not queued) it is a spurious ring: the next `run()`
            // iteration idles, which is harmless.
            owner.ring();
        }
        return false;
    }
    true
}

/// Seals the slot and returns the number of searches attached at that instant.
fn seal(cell: &AtomicUsize) -> usize {
    cell.fetch_or(SEALED, Ordering::AcqRel) & COUNT
}

/// What a single `drain_once` call did.
pub(super) enum Progress {
    /// Processed one entry. The value is the number of addresses released (0 for a barrier).
    ///
    /// The production path (`AnnIndex::drain_retired`) discards it via `Released(_)` -- only the
    /// fact that something was released matters. Only tests read the value, so it looks dead
    /// outside `cfg(test)`.
    Released(#[cfg_attr(not(test), expect(dead_code))] usize),
    /// The barrier at the head has not cleared yet.
    Blocked,
    /// The queue is empty.
    Empty,
}

pub(super) struct Retirement {
    slots: Box<[AtomicUsize]>,
    /// Ring of addresses to release. Allocated once at startup and never grown.
    ///
    /// `AtomicU64` because `retire` writes to another span while a drainer reads without the
    /// mutex. They never overlap -- the drainer's `[head, to)` cannot be overwritten by `retire`
    /// until `head` is advanced.
    addrs: Box<[AtomicU64]>,
    /// Index of the currently open slot. Written only by retire; searches only read it.
    open: AtomicUsize,
    queue: Mutex<Queue>,
    bell: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl Retirement {
    /// About 61KB per index. Small, but there is one per `vop create` and no cap on the number of
    /// indexes, so the client decides the total. Hence no `collect` or `with_capacity` -- those
    /// abort on failure, and this constructor can run on the replication replay path **while
    /// holding the engine cache lock**.
    pub(super) fn new() -> crate::error::Result<Self> {
        Self::with_cap_inner(CAP)
    }

    fn with_cap_inner(cap: usize) -> crate::error::Result<Self> {
        let mut slots = crate::fallible::vec(POOL)?;
        slots.extend((0..POOL).map(|_| AtomicUsize::new(0)));
        let mut addrs = crate::fallible::vec(RING)?;
        addrs.extend((0..RING).map(|_| AtomicU64::new(0)));

        let mut events = VecDeque::new();
        events
            .try_reserve_exact(cap + EVENT_MARGIN)
            .map_err(|_| crate::error::Error::Index("no room for a retirement ring".into()))?;

        Ok(Self {
            // Sized exactly, so `into_boxed_slice` does not reallocate.
            slots: slots.into_boxed_slice(),
            addrs: addrs.into_boxed_slice(),
            open: AtomicUsize::new(0),
            queue: Mutex::new(Queue {
                events,
                ..Queue::default()
            }),
            bell: OnceLock::new(),
        })
    }

    /// Registers how to wake the sweeper. Ringing is a no-op until then.
    pub(super) fn set_bell(&self, bell: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.bell.set(bell);
    }

    /// Detaches from the slot. Returns `true` if this brought a sealed barrier to zero.
    fn detach(&self, slot: usize) -> bool {
        let prev = self.slots[slot].fetch_sub(1, Ordering::AcqRel);
        prev & COUNT == 1 && prev & SEALED != 0
    }

    /// Rings the bell. Called only by a search that brought a sealed barrier to zero.
    ///
    /// Preventing lost wakeups is the bell's job, not this function's -- the sweeper waits on its
    /// own mutex, not this queue's, so taking the queue lock here would protect nothing.
    /// `sweep::wake` sets the flag under that mutex.
    pub(super) fn ring(&self) {
        let Some(bell) = self.bell.get() else { return };
        bell();
    }

    /// Attaches a search to the currently open barrier.
    ///
    /// If the slot is being sealed, backs out and re-reads `open`. Backing out happens before the
    /// graph is read, so that search has seen no addresses.
    ///
    /// Never waits. If the backlog becomes unmanageable, `Halt` locks the whole index; that
    /// check is done at command entry by `cmd::access::for_read`.
    pub(super) fn enter(&self) -> Reading<'_> {
        loop {
            let slot = self.open.load(Ordering::Acquire);
            if !attach(self, &self.slots[slot]) {
                std::thread::yield_now();
                continue;
            }
            let reading = Reading { owner: self, slot };
            // Between reading `open` and attaching, this slot may have been sealed, its barrier
            // cleared, and the seal lifted (ABA). We are attached, but the slot is no longer the
            // open one, so subsequent retires won't wait for this search. The graph hasn't been
            // read yet, so detach and retry.
            if self.open.load(Ordering::Acquire) == slot {
                return reading;
            }
            drop(reading);
        }
    }

    #[cfg(test)]
    pub(super) fn open_slot(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn slot_count(&self, slot: usize) -> usize {
        self.slots[slot].load(Ordering::Acquire) & COUNT
    }

    /// Seals the open slot and opens the next one, without a retire. Test only.
    ///
    /// The real slot-advance rule lives in `retire()`.
    #[cfg(test)]
    pub(super) fn seal_open(&self) -> usize {
        let slot = self.open.load(Ordering::Acquire);
        let live = seal(&self.slots[slot]);
        self.open.store((slot + 1) % POOL, Ordering::Release);
        live
    }

    /// Whether the queue is empty: no addresses to release and no pending barriers.
    pub(super) fn is_empty(&self) -> bool {
        let q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        q.events.is_empty() && q.head == q.tail
    }

    /// Whether the backlog has drained enough: both occupancies must be below `RESUME_AT`%.
    ///
    /// Hysteresis: unlocking as soon as one slot frees up would re-lock on the next delete and
    /// flap at the boundary.
    pub(super) fn drained_enough(&self) -> bool {
        let q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        (q.tail - q.head) * 100 < RING * RESUME_AT
            && q.events.len() * 100 < q.events.capacity() * RESUME_AT
    }

    /// Processes the head of the queue.
    ///
    /// `release` is called **after dropping the mutex**. `release` takes the engine's cache lock,
    /// and the unlink callback takes this mutex while holding the cache lock, so the reverse
    /// order would deadlock.
    ///
    /// During that window the entry stays in the queue, so `draining` keeps other drainers out.
    /// Otherwise two of them would release the same span, a double free.
    pub(super) fn drain_once(&self, elements: &dyn Elements) -> Progress {
        let (from, to) = {
            let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            if q.draining {
                return Progress::Blocked;
            }
            match q.events.front() {
                None => return Progress::Empty,

                Some(Event::Barrier(slot)) => {
                    let slot = *slot;
                    if self.slots[slot].load(Ordering::Acquire) & COUNT > 0 {
                        return Progress::Blocked;
                    }
                    // Clear only the seal. `store(0)` would overwrite the `+1` of a search that
                    // is backing out, underflowing on its `-1`.
                    self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
                    q.events.pop_front();
                    return Progress::Released(0);
                }

                Some(Event::Release(to)) => {
                    let span = (q.head, *to);
                    q.draining = true;
                    span
                }
            }
        };
        // `draining` is set from here on. `release` crosses FFI and may panic; if the flag were
        // left set, this index could never drain again, the queue would fill, and it would end
        // up locked. The guard resets it.
        let guard = Draining::new(self);

        self.release_span(elements, from, to);

        let more = {
            let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            q.draining = false;
            q.head = to;
            // `retire` may have extended the same entry while we were releasing. If so, keep
            // it -- popping it would lose addresses not yet released.
            match q.events.front() {
                Some(Event::Release(tail_to)) if *tail_to > to => {}
                _ => {
                    q.events.pop_front();
                }
            }
            !q.events.is_empty()
        };
        guard.disarm();

        // A drainer may have backed off because of `draining`. The flag is clear now, so tell it
        // to come back -- otherwise it sleeps until the next `TICK`.
        if more {
            self.ring();
        }

        Progress::Released(to - from)
    }

    /// Releases ring span `[from, to)`. Must be called **without holding any lock**.
    ///
    /// `head` is still `from`, so `retire` cannot overwrite this span. Chunking through a stack
    /// buffer avoids the heap.
    fn release_span(&self, elements: &dyn Elements, from: usize, to: usize) {
        let mut chunk = [0u64; RELEASE_CHUNK];
        let mut pos = from;
        while pos < to {
            let n = RELEASE_CHUNK.min(to - pos);
            for (i, cell) in chunk[..n].iter_mut().enumerate() {
                *cell = self.addrs[(pos + i) % RING].load(Ordering::Acquire);
            }
            elements.release(&chunk[..n]);
            pos += n;
        }
    }

    /// Immediately releases every pending address and empties the queue, discarding `Barrier`s.
    ///
    /// **Only called from `Drop for AnnIndex`.** Searches hold an `Arc<VectorIndex>`, so by the
    /// time that `drop` can run no search exists -- whatever the barriers were guarding is gone,
    /// and releasing directly without going through queue order does not break the ordering
    /// argument of §2. This assumption holds nowhere else.
    ///
    /// `release` is called after dropping the queue mutex (same reason as `drain_once`).
    pub(super) fn release_pending(&self, elements: &dyn Elements) {
        loop {
            // Read without advancing `head`. Another thread's unlink callback may call `retire`
            // on this index meanwhile; advancing first would let it wrap around and overwrite
            // the cells being read.
            let (from, to) = {
                let q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
                (q.head, q.tail)
            };
            if from == to {
                break;
            }
            self.release_span(elements, from, to);
            let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            q.head = to;
        }
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .clear();
    }

    /// Queues addresses for release without touching the engine. Returns `false` if there is no
    /// room.
    ///
    /// **The addresses must already be unlinked from the graph**, so that searches starting after
    /// this call cannot reach them.
    ///
    /// Sealing and pushing happen as one unit so that two overlapping retires cannot reorder.
    /// With `[B(b1), R(y), B(b), R(x)]`, a search attached to b could hold y, yet nothing would
    /// block `R(y)`.
    ///
    /// On `false` the caller has lost the addresses -- from a callback it should refuse so the
    /// engine reclaims them; otherwise all it can do is log.
    pub(super) fn retire(&self, addrs: &[u64]) -> bool {
        if addrs.is_empty() {
            return true;
        }
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);

        // Check for room first; giving up after sealing would mean more to undo.
        //
        // Up to two entries are needed -- one barrier and one new `Release`. Tail coalescing may
        // use fewer, but be conservative here.
        if q.tail - q.head + addrs.len() > RING || q.events.len() + 2 > q.events.capacity() {
            // Report the overflow so the caller can apply back-pressure instead of the batch
            // being silently dropped.
            return false;
        }

        let slot = self.open.load(Ordering::Acquire);
        // A separate `load` keeps `seal()` a single RMW. The TOCTOU gap only makes this tripwire
        // less precise, and only toward missing a case; even then the downstream logic
        // (underflow, duplicate Barrier) still exposes the problem.
        let already_sealed = self.slots[slot].load(Ordering::Acquire) & SEALED != 0;
        let live = seal(&self.slots[slot]);
        debug_assert!(
            !already_sealed,
            "POOL wrapped and resealed slot {slot} whose barrier is still queued \
             -- grow POOL or find out why that barrier was not drained"
        );
        if live == 0 {
            // Nobody attached: clear the seal and keep using the slot.
            //
            // `fetch_and` rather than `store(0)`: a search may be attaching and backing out
            // between the seal and this line, and overwriting 0 between its `fetch_add` and
            // `fetch_sub` would underflow. Clear only the flag.
            self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
        } else {
            // The slot advances only when a barrier is queued, so live barriers equal queued
            // barriers and slots never wrap.
            q.events.push_back(Event::Barrier(slot));
            self.open.store((slot + 1) % POOL, Ordering::Release);
        }

        let from = q.tail;
        for (i, addr) in addrs.iter().enumerate() {
            self.addrs[(from + i) % RING].store(*addr, Ordering::Release);
        }
        let to = from + addrs.len();
        q.tail = to;

        // Tail coalescing. The ring appends at `tail`, so what was just written is contiguous
        // with the previous span -- just bump `to`, no copy and no new entry.
        match q.events.back_mut() {
            Some(Event::Release(tail_to)) => *tail_to = to,
            _ => q.events.push_back(Event::Release(to)),
        }

        drop(q);
        self.ring();
        true
    }

    /// Number of addresses in the ring not yet released.
    #[cfg(test)]
    pub(super) fn pending_addrs(&self) -> usize {
        let q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        q.tail - q.head
    }

    /// Marks a drain as in progress, to check that `draining` actually blocks.
    #[cfg(test)]
    pub(super) fn mark_draining(&self) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .draining = true;
    }

    #[cfg(test)]
    pub(super) fn queue_len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .len()
    }

    #[cfg(test)]
    pub(super) fn queue_shape(&self) -> Vec<char> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .iter()
            .map(|e| match e {
                Event::Barrier(_) => 'B',
                Event::Release(_) => 'R',
            })
            .collect()
    }
}

/// Guard that lives for the duration of a search.
///
/// It carries `slot` because a sealed barrier moves to the queue and a new one opens; detaching
/// from "the currently open barrier" on exit would decrement the wrong slot. A single total
/// won't do either -- searches finish in arbitrary order, so a later barrier's search would pay
/// off an earlier barrier's debt.
#[must_use = "the search is only counted while this is alive"]
pub(super) struct Reading<'a> {
    owner: &'a Retirement,
    slot: usize,
}

impl<'a> Reading<'a> {
    /// Signals that the search is done. Returns `true` if this brought a sealed barrier to zero.
    ///
    /// A caller receiving `true` should drain a bit of the queue -- memory is returned on the spot
    /// without waiting for the sweeper. Hence, unlike `Drop`, this does not ring; the caller can
    /// ring if it hits its drain limit.
    pub(super) fn leave(self) -> bool {
        let cleared = self.owner.detach(self.slot);
        std::mem::forget(self);
        cleared
    }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        if self.owner.detach(self.slot) {
            // We were the last one, and this barrier is holding up the sweeper in the queue.
            self.owner.ring();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::Ordering;

    use crate::index::elements::Elements;

    #[derive(Default)]
    struct FakeElements {
        freed: StdMutex<Vec<u64>>,
    }

    impl Elements for FakeElements {
        fn id_at(&self, _addr: u64) -> Option<Arc<str>> {
            None
        }
        fn release(&self, addrs: &[u64]) {
            self.freed.lock().unwrap().extend_from_slice(addrs);
        }
    }

    impl FakeElements {
        fn freed(&self) -> Vec<u64> {
            self.freed.lock().unwrap().clone()
        }
    }

    #[test]
    fn an_unresolved_barrier_blocks_everything_behind_it() {
        let r = Retirement::new().unwrap();
        let e = FakeElements::default();

        let reading = r.enter();
        r.retire(&[0x10]);

        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        assert!(e.freed().is_empty(), "cannot release while an earlier search is alive");

        drop(reading);
        assert!(matches!(r.drain_once(&e), Progress::Released(0)), "barrier is dropped");
        assert!(matches!(r.drain_once(&e), Progress::Released(1)), "then the release");
        assert_eq!(e.freed(), vec![0x10]);
    }

    #[test]
    fn a_full_address_ring_drops_the_batch_rather_than_growing() {
        let r = Retirement::new().unwrap();
        let full: Vec<u64> = (0..RING as u64).collect();

        r.retire(&full);
        assert_eq!(r.pending_addrs(), RING, "ring filled exactly");
        assert_eq!(r.queue_len(), 1);

        r.retire(&[0xdead_beef]);
        assert_eq!(r.pending_addrs(), RING, "overflowing batch is rejected");
        assert_eq!(r.queue_len(), 1, "no new entry either");
    }

    #[test]
    fn a_second_drainer_backs_off_while_one_is_releasing() {
        let r = Retirement::new().unwrap();
        let e = FakeElements::default();
        r.retire(&[0x10]);

        // The first drainer drops the mutex and calls release. If another thread released the
        // same entry in that window, it would be a double free.
        r.mark_draining();
        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        assert!(e.freed().is_empty(), "must not touch a span another drainer is releasing");
    }

    #[test]
    fn a_panic_in_release_does_not_wedge_the_queue() {
        struct Exploding;
        impl Elements for Exploding {
            fn id_at(&self, _addr: u64) -> Option<Arc<str>> {
                None
            }
            fn release(&self, _addrs: &[u64]) {
                panic!("release blew up");
            }
        }

        let r = Retirement::new().unwrap();
        r.retire(&[0x10]);

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            r.drain_once(&Exploding);
        }));
        std::panic::set_hook(hook);
        assert!(caught.is_err(), "panic propagated");

        // If `draining` stayed set, this index could never drain again.
        let e = FakeElements::default();
        assert!(
            matches!(r.drain_once(&e), Progress::Released(1)),
            "guard reset the flag so draining works again"
        );
        assert_eq!(e.freed(), vec![0x10], "unreleased span was picked up again");
    }

    #[test]
    fn a_full_queue_has_not_drained_enough() {
        let r = Retirement::new().unwrap();
        let full: Vec<u64> = (0..RING as u64).collect();
        r.retire(&full);

        assert!(!r.drained_enough(), "unlocking when full would re-lock immediately");
    }

    #[test]
    fn draining_past_the_mark_says_so() {
        let r = Retirement::new().unwrap();
        let e = FakeElements::default();
        let full: Vec<u64> = (0..RING as u64).collect();
        r.retire(&full);
        assert!(!r.drained_enough());

        while !matches!(r.drain_once(&e), Progress::Empty) {}
        assert!(r.drained_enough(), "fully drained, may accept again");
    }

    #[test]
    fn an_empty_queue_reports_empty() {
        let r = Retirement::new().unwrap();
        let e = FakeElements::default();
        assert!(matches!(r.drain_once(&e), Progress::Empty));
    }

    #[test]
    fn a_resolved_barrier_gives_its_slot_back() {
        let r = Retirement::new().unwrap();
        let e = FakeElements::default();

        let reading = r.enter();
        let slot = r.open_slot();
        r.retire(&[0x10]);
        drop(reading);

        assert!(matches!(r.drain_once(&e), Progress::Released(0)));

        // A zero count is not enough -- slot_count masks SEALED, so this would pass even without
        // the fetch_and. Check that the seal was actually cleared.
        assert!(
            attach(&r, &r.slots[slot]),
            "seal not cleared; enter() would spin forever once this slot reopens"
        );
        r.slots[slot].fetch_sub(1, Ordering::AcqRel);
        assert_eq!(r.slot_count(slot), 0, "slot returned clean");
    }

    #[test]
    fn a_barrier_behind_a_release_does_not_block_it() {
        let r = Retirement::new().unwrap();
        let e = FakeElements::default();

        // The first attached search creates the first barrier.
        let first = r.enter();
        r.retire(&[0x10]);

        // This search started after 0x10 left the graph, so it cannot reach it and attaches to
        // the second barrier -- behind Rel(0x10).
        let late = r.enter();
        r.retire(&[0x20]);
        assert_eq!(r.queue_shape(), vec!['B', 'R', 'B', 'R']);

        drop(first);
        assert!(matches!(r.drain_once(&e), Progress::Released(0)), "first barrier cleared");
        assert!(matches!(r.drain_once(&e), Progress::Released(1)));
        assert_eq!(
            e.freed(),
            vec![0x10],
            "first release passes even though the late search is still running"
        );

        // Everything after that is blocked by `late`.
        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        drop(late);
        assert!(matches!(r.drain_once(&e), Progress::Released(0)));
        assert!(matches!(r.drain_once(&e), Progress::Released(1)));
        assert_eq!(e.freed(), vec![0x10, 0x20]);
    }

    #[test]
    fn a_search_holds_the_open_slot_until_it_ends() {
        let r = Retirement::new().unwrap();
        let slot = r.open_slot();
        assert_eq!(r.slot_count(slot), 0);

        let reading = r.enter();
        assert_eq!(r.slot_count(slot), 1, "search attached");

        drop(reading);
        assert_eq!(r.slot_count(slot), 0, "search detached");
    }

    #[test]
    fn two_searches_share_one_slot() {
        let r = Retirement::new().unwrap();
        let slot = r.open_slot();

        let a = r.enter();
        let b = r.enter();
        assert_eq!(r.slot_count(slot), 2);

        drop(a);
        assert_eq!(r.slot_count(slot), 1);
        drop(b);
        assert_eq!(r.slot_count(slot), 0);
    }

    #[test]
    fn sealing_returns_the_count_at_that_moment() {
        let r = Retirement::new().unwrap();
        let a = r.enter();
        let b = r.enter();

        assert_eq!(r.seal_open(), 2, "returns the exact count at seal time");
        drop(a);
        drop(b);
    }

    #[test]
    fn a_search_that_meets_a_sealed_slot_goes_to_the_next_one() {
        let r = Retirement::new().unwrap();
        let first = r.open_slot();

        let held_open = r.enter(); // keeps the seal from returning 0
        assert_eq!(r.seal_open(), 1);

        let late = r.enter();
        assert_ne!(r.open_slot(), first, "sealed slot is skipped; moved to the next one");
        assert_eq!(r.slot_count(first), 1, "only the earlier search remains on the sealed slot");
        assert_eq!(r.slot_count(r.open_slot()), 1, "late search attached to the new slot");

        drop(late);
        drop(held_open);
    }

    #[test]
    fn attaching_to_a_sealed_slot_backs_out_without_disturbing_the_count() {
        let r = Retirement::new().unwrap();
        let slot = r.open_slot();

        let holder = r.enter();
        assert_eq!(seal(&r.slots[slot]), 1, "count at seal time");

        // A search trying to attach to a sealed slot backs out. enter() can't exercise this --
        // with `open` not advanced it would spin forever.
        assert!(!attach(&r, &r.slots[slot]), "cannot attach while sealed");
        assert_eq!(
            r.slot_count(slot),
            1,
            "backed-out search neither left a count nor erased the existing one"
        );

        drop(holder);
        assert_eq!(r.slot_count(slot), 0, "back to 0 without underflow");
    }

    #[test]
    fn a_retire_with_no_search_attached_queues_no_barrier() {
        let r = Retirement::new().unwrap();
        let slot = r.open_slot();

        r.retire(&[0x10]);
        r.retire(&[0x20]);

        assert_eq!(r.queue_shape(), vec!['R'], "no attached search, no barrier");
        assert_eq!(r.open_slot(), slot, "slot did not advance");
    }

    #[test]
    fn consecutive_releases_coalesce_into_the_tail() {
        let r = Retirement::new().unwrap();
        for addr in 0..50u64 {
            r.retire(&[addr]);
        }
        assert_eq!(r.queue_len(), 1, "addresses extend the tail entry");
    }

    #[test]
    fn slots_do_not_wrap_when_no_search_is_running() {
        let r = Retirement::new().unwrap();
        let slot = r.open_slot();
        for addr in 0..(POOL as u64 * 3) {
            r.retire(&[addr]);
        }
        assert_eq!(r.open_slot(), slot, "slot stays put when no barrier is queued");
    }

    #[test]
    fn a_retire_with_a_search_attached_queues_a_barrier_first() {
        let r = Retirement::new().unwrap();
        let slot = r.open_slot();
        let reading = r.enter();

        r.retire(&[0x10]);

        assert_eq!(r.queue_shape(), vec!['B', 'R'], "barrier precedes the release");
        assert_ne!(r.open_slot(), slot, "barrier was queued, so the slot advanced");
        drop(reading);
    }

    #[test]
    fn concurrent_retires_never_put_two_barriers_side_by_side() {
        // If sealing and pushing were not atomic, barriers could end up adjacent like
        // [B, B, R, R], and the first barrier would not block the release behind the second.
        let r = Arc::new(Retirement::new().unwrap());
        let _reading = r.enter(); // makes every retire create a barrier

        std::thread::scope(|s| {
            for t in 0..4u64 {
                let r = Arc::clone(&r);
                s.spawn(move || {
                    for i in 0..50u64 {
                        let _hold = r.enter();
                        r.retire(&[t * 1000 + i]);
                    }
                });
            }
        });

        let shape = r.queue_shape();
        for pair in shape.windows(2) {
            assert_ne!(pair, ['B', 'B'], "two adjacent barriers: {shape:?}");
        }
    }

    #[test]
    fn a_search_that_starts_after_a_retire_joins_the_next_barrier() {
        let r = Retirement::new().unwrap();
        let first = r.enter();
        r.retire(&[0x10]);
        let second = r.enter();

        r.retire(&[0x20]);

        assert_eq!(
            r.queue_shape(),
            vec!['B', 'R', 'B', 'R'],
            "second search joins the second barrier and does not block the first release"
        );
        drop(first);
        drop(second);
    }

    #[test]
    fn the_last_search_of_a_sealed_barrier_rings_the_bell() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new().unwrap();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        let a = r.enter();
        let b = r.enter();
        r.retire(&[0x10]); // seals (2); retire() itself also rings once (tested separately)

        let after_retire = rings.load(Ordering::Relaxed);
        drop(a);
        assert_eq!(rings.load(Ordering::Relaxed), after_retire, "not the last one yet");
        drop(b);
        assert_eq!(rings.load(Ordering::Relaxed), after_retire + 1, "the last one rings");
    }

    #[test]
    fn retire_itself_rings_the_bell() {
        use std::sync::atomic::AtomicUsize as Counter;

        // If retire() didn't ring on completion, addresses would sit for up to a sweeper TICK.
        // Every retire() must ring, including when no barrier is created (no searches).
        let r = Retirement::new().unwrap();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        r.retire(&[0x10]);

        assert_eq!(
            rings.load(Ordering::Relaxed),
            1,
            "retire() must wake the sweeper whenever it queues something"
        );
    }

    #[test]
    fn an_empty_retire_does_not_ring() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new().unwrap();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        r.retire(&[]);

        assert_eq!(rings.load(Ordering::Relaxed), 0, "nothing queued, no reason to wake");
    }

    #[test]
    fn a_backing_out_search_that_clears_the_last_count_rings_the_bell() {
        use std::sync::atomic::AtomicUsize as Counter;

        // The sealed-slot backout path in `attach` must ring under the same condition as
        // `Reading::drop`. Simulate a sealed slot whose count is already 0 -- the same bit
        // pattern as the moment the last reader detached but the sweeper hasn't cleared the
        // barrier yet.
        let r = Retirement::new().unwrap();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        let slot = r.open_slot();
        r.slots[slot].store(SEALED, Ordering::Release);

        assert!(!attach(&r, &r.slots[slot]), "cannot attach while sealed");
        assert_eq!(
            rings.load(Ordering::Relaxed),
            1,
            "backing out brought the count back to 0, so the sweeper must be woken"
        );
        assert_eq!(r.slot_count(slot), 0, "back to 0 without underflow");
    }

    #[test]
    fn a_backing_out_search_that_is_not_last_does_not_ring() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new().unwrap();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        let slot = r.open_slot();
        let holder = r.enter(); // a real reader holds this barrier
        assert_eq!(seal(&r.slots[slot]), 1);

        assert!(!attach(&r, &r.slots[slot]), "cannot attach while sealed");
        assert_eq!(
            rings.load(Ordering::Relaxed),
            0,
            "a real reader is still attached after the backout, so no ring yet"
        );

        drop(holder);
    }

    #[test]
    fn leaving_reports_only_when_it_cleared_a_sealed_barrier() {
        let r = Retirement::new().unwrap();

        // Unsealed barrier -- nobody is waiting, so nothing to report.
        assert!(!r.enter().leave(), "false before sealing");

        let a = r.enter();
        let b = r.enter();
        r.retire(&[0x10]); // seals (2)

        assert!(!a.leave(), "b is still attached");
        assert!(b.leave(), "last one out -- the caller should drain");
    }

    #[test]
    fn an_unsealed_barrier_does_not_ring() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new().unwrap();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        drop(r.enter());
        assert_eq!(rings.load(Ordering::Relaxed), 0, "nobody waits on an unsealed barrier");
    }
}
