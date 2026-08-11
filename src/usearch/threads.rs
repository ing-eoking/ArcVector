//! Reserved thread contexts, and the invariant that keeps them from running out.
//!
//! usearch's Rust bindings always call into C++ with `any_thread()`, which pops a
//! context from a **fixed-size pool**. When the pool is empty it does not block —
//! it fails with "Reserve capacity ahead of insertions!". arcus allows more worker
//! threads than any constant could assume (`-t` above 64 only warns), so
//! correctness cannot rest on the worker count.
//!
//! Instead every entry into usearch takes a permit from a semaphore sized to the
//! reserved contexts:
//!
//! ```text
//! concurrent entries <= permits == reserved contexts   =>   the pop cannot fail
//! ```
//!
//! Excess workers wait rather than fail.

use std::sync::{Condvar, Mutex, PoisonError};

/// Thread contexts reserved per index, and therefore the permit count.
///
/// Not a client-facing setting: it follows from how many worker threads the server
/// runs, which the client has no view of. The value is not correctness-critical —
/// the invariant above holds for any of them — so it is a throughput/memory
/// tradeoff. Below the worker count, excess workers queue briefly; above it, the
/// surplus per-thread buffers are wasted (usearch allocates
/// `bytes_per_vector * threads` for casting, so a 4096-dimension `f32` index costs
/// about 1 MB here). 64 covers every worker count `-t` reaches without warning
/// (memcached.c:16107).
///
/// Deriving it from `settings.num_threads` was considered and rejected: the symbol
/// is exported, but reading it needs the `struct settings` layout, whose field
/// offsets shift with build-time `#ifdef`s.
pub const THREAD_SLOTS: usize = 64;

// ---------------------------------------------------------------------------
// Semaphore
// ---------------------------------------------------------------------------

pub struct Semaphore {
    avail: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    pub fn new(n: usize) -> Self {
        Semaphore {
            avail: Mutex::new(n.max(1)),
            cv: Condvar::new(),
        }
    }

    pub fn acquire(&self) -> Permit<'_> {
        let mut avail = self.avail.lock().unwrap_or_else(PoisonError::into_inner);
        while *avail == 0 {
            avail = self.cv.wait(avail).unwrap_or_else(PoisonError::into_inner);
        }
        *avail -= 1;
        Permit { sem: self }
    }
}

pub struct Permit<'a> {
    sem: &'a Semaphore,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut avail = self
            .sem
            .avail
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *avail += 1;
        drop(avail);
        self.sem.cv.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn semaphore_bounds_concurrent_holders() {
        let sem = Arc::new(Semaphore::new(3));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let sem = Arc::clone(&sem);
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let _p = sem.acquire();
                    let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::yield_now();
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // This is the invariant that keeps usearch's context pool from draining.
        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "peak {}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn semaphore_permits_are_returned_on_panic_unwind() {
        let sem = Arc::new(Semaphore::new(1));
        let s2 = Arc::clone(&sem);
        let _ = std::thread::spawn(move || {
            let _p = s2.acquire();
            panic!("boom");
        })
        .join();
        // If Drop had not run, this would deadlock.
        let _p = sem.acquire();
    }
}
