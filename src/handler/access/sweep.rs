//! Releasing the graphs of indexes whose Map is gone, without waiting for a command to ask.
//!
//! Every command already drops the entry for a name it finds missing, but only for the name it
//! was given. An index nobody queries again — expired by `EXPTIME`, or evicted under memory
//! pressure — keeps its usearch graph and id mapping for the life of the process. Those are the
//! largest things this module holds, and nothing else is going to notice.
//!
//! What the sweep costs the connection that triggers it is bounded twice over. It probes at
//! most [`PER_SWEEP`] names per pass, picking up where the last one stopped, so the bill does
//! not grow with the number of registered indexes. And releasing an index only takes it out of
//! the registry — the graph it holds is torn down by [`registry::retire`]'s thread, not here,
//! because handing back a refcount per vector is the part that actually takes time.
//!
//! **The probing runs on a worker thread, on purpose.** A connectionless thread would be the obvious
//! place, but the engine may hand the cookie on: with `ENABLE_MIGRATION` the EE build routes
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
/// nobody is asking about. Long enough that a busy server does not spend `getattr`s on it, short
/// enough that an expired index is not a leak.
const INTERVAL: u64 = 30_000;

/// Names probed per pass.
///
/// A pass runs before its connection's answer goes out, so it has to end quickly whatever the
/// registry holds. Each probe is one `getattr`, and the cursor below carries the rest to the
/// next pass: with more indexes than this a full cycle just takes more passes, which for memory
/// nobody is asking about costs nothing.
const PER_SWEEP: usize = 8;

/// Where the last pass stopped, in name order. Empty starts a cycle over.
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
    for name in due(PER_SWEEP) {
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

/// The next `limit` names to probe, in name order, resuming after [`CURSOR`].
///
/// Sorting is what makes "after" mean anything: the registry is a hash map, so its own order
/// changes as names come and go and a positional cursor would skip names for good. A name added
/// behind the cursor waits for the next cycle, which is the same wait everything else gets.
fn due(limit: usize) -> Vec<String> {
    let mut cursor = CURSOR.lock().unwrap_or_else(PoisonError::into_inner);
    let mut names = registry::names();
    names.sort_unstable();
    next_batch(names, &mut cursor, limit)
}

/// [`due`] without the registry, so the cursor's wrap can be tested. `names` comes sorted.
fn next_batch(mut names: Vec<String>, cursor: &mut String, limit: usize) -> Vec<String> {
    let start = names.partition_point(|n| n.as_str() <= cursor.as_str());
    let mut batch: Vec<String> = names.drain(start..).take(limit).collect();
    if batch.len() < limit {
        // The cycle ran out; carry on from the top, skipping what this pass already has.
        let wrap = limit - batch.len();
        names.truncate(wrap);
        batch.append(&mut names);
    }

    cursor.clear();
    match batch.last() {
        // A full pass took everything there was, so the next one starts over.
        Some(last) if batch.len() == limit => cursor.push_str(last),
        _ => {}
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::next_batch;

    fn names(all: &[&str]) -> Vec<String> {
        all.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_batch_resumes_where_the_last_one_stopped() {
        let mut cursor = String::new();
        let all = names(&["a", "b", "c", "d", "e"]);

        assert_eq!(next_batch(all.clone(), &mut cursor, 2), names(&["a", "b"]));
        assert_eq!(cursor, "b");
        assert_eq!(next_batch(all.clone(), &mut cursor, 2), names(&["c", "d"]));
        assert_eq!(cursor, "d");
    }

    #[test]
    fn a_short_cycle_wraps_and_a_full_one_carries_the_cursor() {
        let mut cursor = "d".to_string();
        let all = names(&["a", "b", "c", "d", "e"]);

        // One name left after the cursor, so the pass fills up from the top.
        assert_eq!(
            next_batch(all.clone(), &mut cursor, 3),
            names(&["e", "a", "b"])
        );
        assert_eq!(cursor, "b");
    }

    #[test]
    fn a_cycle_shorter_than_the_limit_starts_over() {
        let mut cursor = "y".to_string();
        let all = names(&["a", "b"]);

        assert_eq!(next_batch(all, &mut cursor, 8), names(&["a", "b"]));
        assert_eq!(
            cursor, "",
            "nothing was left unprobed, so the next pass begins a cycle"
        );
    }

    #[test]
    fn an_empty_registry_sweeps_nothing() {
        let mut cursor = "a".to_string();
        assert!(next_batch(Vec::new(), &mut cursor, 8).is_empty());
        assert_eq!(cursor, "");
    }
}
