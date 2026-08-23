//! Releasing the graphs of indexes whose Map is gone, without waiting for a command to ask.
//!
//! Every command already drops the entry for a name it finds missing, but only for the name it
//! was given. An index nobody queries again — expired by `EXPTIME`, or evicted under memory
//! pressure — keeps its usearch graph and id mapping for the life of the process. Those are the
//! largest things this module holds, and nothing else is going to notice.
//!
//! A pass runs before its connection's answer goes out, so what it costs that connection is
//! flat: **one `getattr`**, on the name after the one the last pass took. A registry of five
//! hundred is five hundred passes around, which for memory nobody is asking about is fine.
//!
//! Releasing costs that connection nothing beyond leaving the registry: the graph is torn down
//! by [`registry::retire`]'s thread, because handing back a refcount per vector is the part
//! that actually takes time.
//!
//! **The probing runs on a worker thread, on purpose.** A connectionless thread would be the
//! obvious place, but the engine may hand the cookie on: with `ENABLE_MIGRATION` the EE build routes
//! every read through `mg_before_check`, which during a migration calls
//! `set_not_my_key_info(cookie, …)`. A null cookie there is a null dereference. Worker threads
//! carry a real one, so the sweep borrows the `Store` of whichever command triggered it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, PoisonError};
use std::time::Instant;

use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry;

/// How long a Map may be gone before its graph is released.
///
/// Not a deadline for correctness — a command on the name notices immediately — but for memory
/// nobody is asking about. A pass is one `getattr`, so this is what the whole server spends on
/// sweeping, and a name comes round again every `INTERVAL` times the number of indexes.
const INTERVAL: u64 = 5_000;

/// The name the last pass probed. Empty starts a cycle over.
static CURSOR: LazyLock<std::sync::Mutex<String>> =
    LazyLock::new(|| std::sync::Mutex::new(String::new()));

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

/// Release the indexes whose Map has gone, if it is time.
///
/// Returns without touching the engine on all but one call per [`INTERVAL`], and even then
/// probes no more than [`PER_SWEEP`] names.
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
    if let Some(name) = due() {
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

/// The name to probe this pass, one on from the last. `None` only if nothing is registered.
///
/// The order is by name because the registry is a hash map and has none of its own — a
/// positional cursor would skip names for good as entries come and go.
fn due() -> Option<String> {
    let mut cursor = CURSOR.lock().unwrap_or_else(PoisonError::into_inner);
    let name = registry::name_after(&cursor)?;
    cursor.clear();
    cursor.push_str(&name);
    Some(name)
}
