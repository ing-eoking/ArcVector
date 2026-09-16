use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::handler::registry::{self, VectorIndex};

const TICK: Duration = Duration::from_secs(1);

struct Sweeper {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    retired: Vec<Arc<VectorIndex>>,
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
pub(in crate::handler) fn ensure_sweeper() {
    LazyLock::force(&SWEEPER);
}

/// sweeper를 깨운다. 봉인된 배리어를 푼 검색이 부른다.
pub(in crate::handler) fn wake() {
    SWEEPER.wake.notify_one();
}

fn state() -> MutexGuard<'static, State> {
    SWEEPER.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Hands a graph no longer in the registry to the sweeper to drop.
///
/// Freeing one returns every element it holds to the engine, which is work a
/// request should not be made to wait through.
pub(in crate::handler) fn retire(evicted: Option<Arc<VectorIndex>>) {
    let Some(index) = evicted else { return };
    let mut state = state();
    if state.retired.try_reserve(1).is_err() {
        drop(state);
        drop(index);
        return;
    }
    state.retired.push(index);
    SWEEPER.wake.notify_one();
}

fn run() {
    let mut last_round = Instant::now();
    loop {
        let retired = {
            let mut state = state();
            if state.retired.is_empty() {
                state = SWEEPER
                    .wake
                    .wait_timeout(state, TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            std::mem::take(&mut state.retired)
        };

        drop(retired);

        if last_round.elapsed() >= TICK {
            last_round = Instant::now();
            crate::server::tick();
            for index in registry::indexes().unwrap_or_default() {
                index.ann.retry_stuck();
                while index.ann.drain_retired() {}
            }
        }
    }
}
