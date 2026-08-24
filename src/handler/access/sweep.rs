use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry::{self, VectorIndex};

const TICK: Duration = Duration::from_secs(1);

const BATCH: usize = 10;

struct Probe {
    name: String,

    stamp: u64,
}

struct Sweeper {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    offered: Vec<Probe>,

    gone: Vec<Probe>,

    retired: Vec<Arc<VectorIndex>>,
}

static OFFERED: AtomicUsize = AtomicUsize::new(0);

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

fn state() -> MutexGuard<'static, State> {
    SWEEPER.state.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn maybe(store: &Store) {
    if OFFERED.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Some(probe) = take_offer() else { return };
    let Some(index) = registry::get(&probe.name) else {
        return;
    };
    if index.is_rebuilding() {
        return;
    }

    let name = &probe.name;
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
            registry::remove_observed(name, &index);
        }
        Err(StoreError::KeyGone) => hand_back(probe),
        Err(_) => {}
    }
}

fn take_offer() -> Option<Probe> {
    let mut state = state();
    let probe = state.offered.pop();
    OFFERED.store(state.offered.len(), Ordering::Relaxed);
    probe
}

fn hand_back(probe: Probe) {
    let mut state = state();
    if state.gone.try_reserve(1).is_ok() {
        state.gone.push(probe);
        SWEEPER.wake.notify_one();
    }
}

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
        let (gone, retired) = {
            let mut state = state();
            if state.gone.is_empty() && state.retired.is_empty() {
                state = SWEEPER
                    .wake
                    .wait_timeout(state, TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            (
                std::mem::take(&mut state.gone),
                std::mem::take(&mut state.retired),
            )
        };

        for probe in gone {
            if registry::remove_if_stale(&probe.name, probe.stamp) {
                let name = &probe.name;
                eprintln!(
                    "ArcVector: index '{name}' has no Map; released the graph it was built from"
                );
            }
        }

        drop(retired);

        if last_round.elapsed() >= TICK {
            last_round = Instant::now();
            crate::server::tick();
            for index in registry::indexes() {
                index.ann.retry_stuck();
                index.ann.reclaim();
            }
            offer_round();
        }
    }
}

fn offer_round() {
    if OFFERED.load(Ordering::Relaxed) != 0 {
        return;
    }

    let stamp = registry::now();
    let round: Vec<Probe> = registry::coldest(BATCH)
        .into_iter()
        .rev()
        .map(|name| Probe { name, stamp })
        .collect();
    if round.is_empty() {
        return;
    }

    let mut state = state();
    state.offered = round;
    OFFERED.store(state.offered.len(), Ordering::Relaxed);
}
