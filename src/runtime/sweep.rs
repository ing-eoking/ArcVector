use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::index::registry;

const TICK: Duration = Duration::from_secs(1);

/// Maximum number of events an index drains in a single sweep round.
/// Prevents starvation of other indices when one index faces heavy delete pressure.
const RECLAIM_BATCH: usize = 64;

struct Sweeper {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    /// Indicates work is pending to sweep. Setting inside the mutex avoids missed wakeups.
    pending: bool,
}

static SWEEPER: LazyLock<Sweeper> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("arcvector-sweep".to_owned())
        .spawn(run)
        .expect("spawn the sweeper thread");
    Sweeper {
        state: Mutex::new(State::default()),
        wake: Condvar::new(),
    }
});

/// Starts the sweeper if it is not running. Called when an index enters the
/// registry, which is the first moment there is anything to sweep -- and it has
/// to be then rather than at load time, because `-d` forks after the extensions
/// load and no thread survives that.
pub(crate) fn ensure_sweeper() {
    LazyLock::force(&SWEEPER);
}

/// Wakes the sweeper thread when a sealed barrier has been released by a query.
pub(crate) fn wake() {
    state().pending = true;
    SWEEPER.wake.notify_one();
}

fn state() -> MutexGuard<'static, State> {
    SWEEPER.state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run() {
    let mut last_round = Instant::now();
    loop {
        {
            let mut state = state();
            if !state.pending {
                state = SWEEPER
                    .wake
                    .wait_timeout(state, TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            // Reset before sweeping. Signals during sweep will re-flag for the next loop.
            state.pending = false;
        }

        // Sweep dropped indices as well to release accumulated addresses.
        let indexes = registry::all();

        // Drain retired addresses up to RECLAIM_BATCH per index.
        // If the batch bound was reached, mark pending so the next round runs immediately.
        for index in &indexes {
            if drain_bounded(|| index.ann.drain_retired()) {
                state().pending = true;
            }
            index.ann.resume_if_drained();
        }

        if last_round.elapsed() >= TICK {
            last_round = Instant::now();
            for index in &indexes {
                index.ann.retry_stuck();
            }
        }

        // Drop held list before reaping finalized indices to avoid retaining references.
        drop(indexes);
        registry::reap_finalized();
    }
}

fn drain_bounded(mut drain_once: impl FnMut() -> bool) -> bool {
    let mut n = 0;
    while n < RECLAIM_BATCH && drain_once() {
        n += 1;
    }
    n == RECLAIM_BATCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_stops_at_the_batch_bound_even_if_work_keeps_reappearing() {
        let calls = std::cell::Cell::new(0usize);
        let hit_bound = drain_bounded(|| {
            calls.set(calls.get() + 1);
            true
        });

        assert!(
            hit_bound,
            "must report hitting batch bound to immediately schedule next round"
        );
        assert_eq!(calls.get(), RECLAIM_BATCH, "stops exactly at batch limit");
    }

    #[test]
    fn draining_stops_early_when_the_queue_runs_out_before_the_bound() {
        let calls = std::cell::Cell::new(0usize);
        let hit_bound = drain_bounded(|| {
            let n = calls.get() + 1;
            calls.set(n);
            n < 5
        });

        assert!(!hit_bound, "queue exhausted before bound indicates no pending work");
        assert_eq!(calls.get(), 5);
    }
}
