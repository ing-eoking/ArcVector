//! Releasing the graphs of indexes whose Map is gone, without waiting for a command to ask.
//!
//! Every command already drops the entry for a name it finds missing, but only for the name it
//! was given. An index nobody queries again — expired by `EXPTIME`, or evicted under memory
//! pressure — keeps its usearch graph and id mapping for the life of the process. Those are the
//! largest things this module holds, and nothing else is going to notice.
//!
//! **This runs on a worker thread, on purpose.** A connectionless thread would be the obvious
//! place, but the engine may hand the cookie on: with `ENABLE_MIGRATION` the EE build routes
//! every read through `mg_before_check`, which during a migration calls
//! `set_not_my_key_info(cookie, …)`. A null cookie there is a null dereference. Worker threads
//! carry a real one, so the sweep borrows the `Store` of whichever command triggered it.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry;

/// How long a Map may be gone before its graph is released.
///
/// Not a deadline for correctness — a command on the name notices immediately — but for memory
/// nobody is asking about. Long enough that a busy server does not spend `getattr`s on it, short
/// enough that an expired index is not a leak.
const INTERVAL: u64 = 30_000;

static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds since the first sweep check, at the last completed sweep.
static LAST: AtomicU64 = AtomicU64::new(0);

/// Held across a sweep, so a slow one does not stack up behind itself.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Clears [`RUNNING`] however the sweep ends. A flag left set by a panic would stop every
/// later sweep, and the thing this exists to bound would go unbounded in silence.
struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

/// Release the graphs of any indexes whose Map has gone, if it is time.
///
/// Called after a command has answered, so its cost never lands inside one. Returns without
/// touching the engine on all but one call per [`INTERVAL`].
pub fn maybe(store: &Store) {
    let now = STARTED.elapsed().as_millis() as u64;
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < INTERVAL {
        return;
    }
    // One sweeper at a time, and the loser goes back to serving.
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let _running = Running;
    // Stamped before the probes, so an index registered during the sweep is out of reach — the
    // same rule every other release follows.
    let stamp = registry::now();
    for name in registry::names() {
        // Only `KeyGone` is an answer. Anything else proves nothing about the name, and a
        // transient engine failure must not cost a working index its graph.
        if matches!(store.probe_map(&name), Err(StoreError::KeyGone))
            && registry::remove_if_stale(&name, stamp)
        {
            eprintln!("ArcVector: index '{name}' has no Map; released the graph it was built from");
        }
    }
    LAST.store(STARTED.elapsed().as_millis() as u64, Ordering::Relaxed);
}
