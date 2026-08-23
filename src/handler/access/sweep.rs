//! Releasing the graphs of indexes whose Map is gone, without waiting for a command to ask.
//!
//! Every command already drops the entry for a name it finds missing, but only for the name it
//! was given. An index nobody queries again — expired by `EXPTIME`, or evicted under memory
//! pressure — keeps its usearch graph and the element refcounts that graph holds for the life of
//! the process. Those are the largest things this module has, and nothing else is going to
//! notice.
//!
//! **A thread does the work; a connection lends the one call that needs a cookie.** The engine
//! puts `ACTION_BEFORE_READ` on every keyed API and hands it the cookie: with `ENABLE_MIGRATION`
//! the EE build routes that to `mg_before_check` → `set_not_my_key_info(cookie, …)`, so a
//! connectionless thread calling `getattr` is a null dereference waiting for a migration
//! ([§7.5](../../../docs/내부구조.md)). `getattr` is therefore the *only* part a connection does,
//! and it does one:
//!
//! ```text
//! sweeper thread        picks the BATCH coldest names        ← reads every entry
//!   connection ①        getattr(name₁) → gone, hand back     ← one call, nothing else
//!   connection ②        getattr(name₂) → alive, forget it
//!   …
//! sweeper thread        removes the gone ones, tears them down
//! ```
//!
//! Choosing is what reads the whole registry, and tearing down is what takes real time — a
//! refcount handed back per vector, then the HNSW destructor. Neither belongs in front of an
//! answer that has not gone out yet, and neither needs a cookie.
//!
//! Coldest first because an index a command touched a moment ago is one whose Map was there a
//! moment ago. The sweep looks where something is likely to be gone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry::{self, VectorIndex};

/// How often the sweeper wakes to hand out work and to tick the access clock.
const TICK: Duration = Duration::from_secs(1);

/// Names put up for probing at a time.
///
/// One connection takes one of them, so this is how many commands it takes to get through a
/// round — not how much any of them carries. A round per [`TICK`] walks a registry of five
/// hundred in under a minute, and reclaiming memory nobody is asking about does not need to be
/// faster than that.
const BATCH: usize = 10;

/// A name waiting to be probed, or one that came back gone.
struct Probe {
    name: String,
    /// Taken before the name went up for probing, so a `vcreate` that publishes while it is out
    /// there is out of the verdict's reach — the rule every release here follows.
    stamp: u64,
}

struct Sweeper {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    /// Put up by the thread, taken one at a time by connections.
    offered: Vec<Probe>,
    /// Handed back by connections: probed, and the Map was gone.
    gone: Vec<Probe>,
    /// Left the registry and needs dropping somewhere that is not a connection.
    retired: Vec<Arc<VectorIndex>>,
}

/// `offered.len()`, so a command that has nothing to do finds out in one relaxed load.
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

/// Probe one offered name, if any is offered.
///
/// Called with a command's answer decided but not yet sent, so it does one `getattr` and
/// nothing else — no registry scan, no removal, no teardown. A name that comes back gone goes
/// to the thread; one that answers anything else is dropped, since only `KeyGone` proves
/// something and a transient engine failure must not cost a working index its graph.
pub fn maybe(store: &Store) {
    if OFFERED.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Some(probe) = take_offer() else { return };
    if matches!(store.probe_map(&probe.name), Err(StoreError::KeyGone)) {
        hand_back(probe);
    }
}

fn take_offer() -> Option<Probe> {
    let mut state = state();
    let probe = state.offered.pop();
    OFFERED.store(state.offered.len(), Ordering::Relaxed);
    probe
}

/// Report a probed name whose Map was gone. Dropped if the queue cannot grow — the name will be
/// offered again in a later round.
fn hand_back(probe: Probe) {
    let mut state = state();
    if state.gone.try_reserve(1).is_ok() {
        state.gone.push(probe);
        SWEEPER.wake.notify_one();
    }
}

/// Hand an entry that has left the registry to the sweeper. **Call with no registry guard held.**
///
/// The last `Arc` runs `AnnIndex::drop`, which hands back a refcount per vector —
/// `map_elem_release` takes the engine's cache lock every hundred — and then destroys the HNSW
/// index. On a large graph that is a long time to spend in front of an answer, and longer still
/// to spend holding the registry's write lock that every name lookup needs.
///
/// If it is not the last `Arc` this only moves a reference, and whichever command still holds
/// one pays when it finishes — that thread was using the index anyway. What this covers is the
/// case the sweep makes: nobody is using it, so nobody but the sweep would pay.
pub(in crate::handler) fn retire(evicted: Option<Arc<VectorIndex>>) {
    let Some(index) = evicted else { return };
    let mut state = state();
    if state.retired.try_reserve(1).is_err() {
        // Nowhere to put it. Tearing it down here costs this thread the time, which still beats
        // leaking the graph.
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

        // Everything below is outside both locks, because all of it is the expensive part.
        for probe in gone {
            if registry::remove_if_stale(&probe.name, probe.stamp) {
                let name = &probe.name;
                eprintln!(
                    "ArcVector: index '{name}' has no Map; released the graph it was built from"
                );
            }
        }
        // Whatever the removals just retired comes round on the next turn.
        drop(retired);

        if last_round.elapsed() >= TICK {
            last_round = Instant::now();
            registry::tick();
            offer_round();
        }
    }
}

/// Put the coldest names up for probing, if the last round has been taken.
///
/// A round that connections have not finished is left alone: refilling it would keep re-offering
/// the same coldest names and the rest of the registry would never come up.
fn offer_round() {
    if OFFERED.load(Ordering::Relaxed) != 0 {
        return;
    }
    // Stamped before the names are chosen, let alone probed, so any publish that races the round
    // reads as newer than the verdict — which is what spares it.
    let stamp = registry::now();
    let round: Vec<Probe> = registry::coldest(BATCH)
        .into_iter()
        .map(|name| Probe { name, stamp })
        .collect();
    if round.is_empty() {
        return;
    }

    let mut state = state();
    state.offered = round;
    OFFERED.store(state.offered.len(), Ordering::Relaxed);
}
