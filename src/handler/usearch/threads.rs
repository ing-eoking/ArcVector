//! Reserved thread contexts, and the invariant that keeps them from running out.
//!
//! ```text
//! concurrent entries <= permits == reserved contexts   =>   the pop cannot fail
//! ```
//!
//! `docs/내부구조.md` §9.

use std::sync::{Condvar, Mutex, PoisonError};

/// Thread contexts reserved per index, and therefore the permit count.
pub const THREAD_SLOTS: usize = 64;

pub struct Semaphore {
    avail: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    pub fn new(n: usize) -> Self {
        Self {
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
