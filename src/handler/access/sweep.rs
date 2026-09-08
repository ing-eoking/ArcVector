use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry::{self, VectorIndex};

const TICK: Duration = Duration::from_secs(1);

const BATCH: usize = 10;

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
                index.ann.reclaim();
            }
            probe_round();
        }
    }
}

/// Asks the engine, for the coldest few indexes, whether the Map each graph was
/// built from is still there and still agrees on how many elements it holds.
///
/// The Map can go without anyone telling us -- it expires, or the engine evicts
/// it -- and until this notices, the graph answers from memory that no longer
/// has a store behind it. Nothing else asks: a name no command touches is
/// exactly the one that goes stale unseen, so the question has to come from
/// here rather than from a request.
fn probe_round() {
    // Keyed calls run the migration gate, which writes the new owner through the
    // cookie when a key has moved. Skip the round until there is one to write
    // to -- these names will still be the coldest a second from now.
    let Some(store) = Store::background_keyed() else {
        return;
    };

    let stamp = registry::now();
    for index in registry::coldest(BATCH) {
        probe(&store, &index, stamp);
    }
}

fn probe(store: &Store, index: &Arc<VectorIndex>, stamp: u64) {
    if index.is_rebuilding() {
        return;
    }

    let name = &index.name;
    let counted = index.ann.reconcile(|| {
        store
            .probe_map(name)
            .map(|map| map.count.saturating_sub(1) as usize)
    });
    match counted {
        Ok(None) => {}
        Ok(Some((in_map, named))) => {
            eprintln!(
                "ArcVector: index '{name}' names {named} element(s) but its Map holds {in_map}; \
                 dropping the graph so the next read rebuilds it"
            );
            registry::remove_observed(name, index);
        }
        // The Map is gone. `stamp` was taken before the round, so an index
        // published since then survives -- the answer is about the old one.
        Err(StoreError::KeyGone) => {
            if registry::remove_if_stale(name, stamp) {
                eprintln!(
                    "ArcVector: index '{name}' has no Map; released the graph it was built from"
                );
            }
        }
        Err(_) => {}
    }
}
