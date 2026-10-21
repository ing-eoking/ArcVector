//! Temporarily locks an index out of service.
//!
//! If a retirement cannot be pushed onto the sweeper's queue, the graph and the store drift
//! apart: the graph may hold a deleted vector or point at memory we no longer own. Answering
//! from that state would return wrong ids or read freed memory.
//!
//! So the index is locked and answers `SERVER_ERROR` until the sweeper has drained enough of
//! its backlog. **Searches already in flight are cut off too** -- that is what makes forced
//! release by the engine safe in this design.
//!
//! The lock bit alone is not enough to cut them off. A search may already hold an address the
//! engine reclaimed while locked as a traversal candidate, and dereference it after the lock
//! is lifted. So every halt bumps an `epoch`, and a search only dereferences under the epoch
//! it started with. If the epoch has changed, it is refused even after the lock is lifted.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Top bit: the index is halted.
const HALTED: usize = 1 << (usize::BITS - 1);

/// Remaining bits: number of item dereferences in progress.
const COUNT: usize = !HALTED;

/// Packs the halt bit and the in-flight dereference count into **a single word**.
///
/// Keeping them separate would leave a window between the check and the dereference; if the
/// engine freed the item in that window it would be a use-after-free. Since `fetch_add`
/// returns the halt bit along with the count, check and registration are one operation and
/// there is no window. Same trick as the barrier slots.
pub(crate) struct Halt {
    state: AtomicUsize,
    /// Number of halts. `halt()` bumps it after setting the halt bit, before resuming.
    epoch: AtomicUsize,
}

impl Halt {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            epoch: AtomicUsize::new(0),
        }
    }

    /// Current epoch. Read when a search starts and passed to `touch`.
    pub(crate) fn epoch(&self) -> usize {
        self.epoch.load(Ordering::Acquire)
    }

    /// Begins an item dereference. Returns `None` if halted, or if a halt has occurred at any
    /// point since `since`.
    ///
    /// While the returned guard is alive, `halt()` does not return, so the engine will not
    /// free the item while the guard is held.
    ///
    /// The epoch is checked **after** incrementing the count. If `halt()` bumped the epoch
    /// before that, it is seen here; if after, `halt()` waits for this guard.
    pub(crate) fn touch(&self, since: usize) -> Option<Touching<'_>> {
        let prev = self.state.fetch_add(1, Ordering::AcqRel);
        if prev & HALTED != 0 || self.epoch.load(Ordering::Acquire) != since {
            self.state.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Touching(self))
    }

    /// Halts and **waits for in-flight dereferences to finish.**
    ///
    /// The wait covers single dereferences only -- following a pointer and reading 128 bytes,
    /// not a whole search -- so it is short. That makes it safe to call from a callback that
    /// holds the cache lock without stalling the daemon.
    pub(crate) fn halt(&self) {
        self.state.fetch_or(HALTED, Ordering::AcqRel);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        while self.state.load(Ordering::Acquire) & COUNT > 0 {
            std::hint::spin_loop();
        }
    }

    /// Accepts requests again. Called once the sweeper has drained its backlog.
    pub(crate) fn resume(&self) {
        self.state.fetch_and(COUNT, Ordering::AcqRel);
    }

    pub(crate) fn is_halted(&self) -> bool {
        self.state.load(Ordering::Acquire) & HALTED != 0
    }

    #[cfg(test)]
    fn touching(&self) -> usize {
        self.state.load(Ordering::Acquire) & COUNT
    }
}

/// Guard marking one dereference in progress.
#[must_use = "the dereference is only protected while this is alive"]
pub(crate) struct Touching<'a>(&'a Halt);

impl Drop for Touching<'_> {
    fn drop(&mut self) {
        self.0.state.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dereference_is_counted_while_it_runs() {
        let h = Halt::new();
        assert_eq!(h.touching(), 0);

        let t = h.touch(h.epoch()).expect("not halted, so touch succeeds");
        assert_eq!(h.touching(), 1);

        drop(t);
        assert_eq!(h.touching(), 0);
    }

    #[test]
    fn a_halted_index_turns_every_dereference_away() {
        let h = Halt::new();
        h.halt();

        assert!(h.is_halted());
        assert!(h.touch(h.epoch()).is_none(), "a halted index must refuse dereferences");
        assert_eq!(h.touching(), 0, "a refused touch must not leave a count behind");
    }

    #[test]
    fn resuming_lets_dereferences_back_in() {
        let h = Halt::new();
        h.halt();
        h.resume();

        assert!(!h.is_halted());
        assert!(h.touch(h.epoch()).is_some());
    }

    #[test]
    fn a_search_from_before_a_halt_stays_turned_away_after_resume() {
        let h = Halt::new();
        let started = h.epoch();
        h.halt();
        h.resume();

        assert!(
            h.touch(started).is_none(),
            "may hold addresses reclaimed while halted, so must be refused after resume"
        );
        assert_eq!(h.touching(), 0, "a refused touch must not leave a count behind");
    }

    #[test]
    fn halting_waits_for_the_dereferences_already_running() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let h = Arc::new(Halt::new());
        let t = h.touch(h.epoch()).expect("hold one dereference first");

        let returned = Arc::new(AtomicBool::new(false));
        let (h2, seen) = (Arc::clone(&h), Arc::clone(&returned));
        let waiter = std::thread::spawn(move || {
            h2.halt();
            seen.store(true, Ordering::Release);
        });

        // halt must not return while a dereference is in flight. If it did, the engine would
        // believe it may free the item -- a use-after-free.
        for _ in 0..50 {
            assert!(
                !returned.load(Ordering::Acquire),
                "halt returned before the dereference finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        drop(t);
        waiter.join().unwrap();
        assert!(returned.load(Ordering::Acquire));
    }
}
