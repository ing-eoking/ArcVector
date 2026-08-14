//! Refilling a graph from Map, on a thread that never writes.
//!
//! A worker stamps [`super::REBUILDING`] and queues the name; the thread reads
//! Map and refills; the next worker command stamps a fresh token. The split is
//! forced by the engine — a write from a connectionless thread reaches
//! `do_check_master_switchover_done`, which dereferences the cookie.

use std::sync::{Condvar, LazyLock, Mutex, PoisonError};

use super::metadata::{MetaState, read_metadata, stamp};
use super::{REBUILDING, VectorIndex, get, remove};
use crate::arcus::element::{META_FIELD, mint_owner};
use crate::arcus::engine::Store;
use crate::error::{Error, Result};

/// Empty a graph this node was not serving and queue its refill.
pub fn take_over(store: &Store, index: &VectorIndex) -> Result<()> {
    let previous = index.owner();
    index
        .refilled
        .store(false, std::sync::atomic::Ordering::Release);
    index.set_owner(REBUILDING);

    if let Err(e) = stamp(store, &index.name, REBUILDING) {
        index.set_owner(previous);
        return Err(e);
    }

    if let Err(e) = index.ann.begin_rebuild() {
        // The Map says rebuilding and this node cannot do it. Drop the entry so
        remove(&index.name);
        return Err(e);
    }
    BUILDER.enqueue(&index.name);
    Ok(())
}

// ---------------------------------------------------------------------------

/// Asleep until there is work; no polling.
struct Builder {
    queue: Mutex<Vec<String>>,
    wake: Condvar,
}

impl Builder {
    fn enqueue(&self, name: &str) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if !queue.iter().any(|q| q == name) {
            queue.push(name.to_owned());
        }
        self.wake.notify_one();
    }

    fn take(&self) -> String {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(name) = queue.pop() {
                return name;
            }
            queue = self
                .wake
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

static BUILDER: LazyLock<Builder> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("arcvector-rebuild".to_owned())
        .spawn(run_builder)
        .expect("spawn the rebuild thread");
    Builder {
        queue: Mutex::new(Vec::new()),
        wake: Condvar::new(),
    }
});

/// Start the rebuild thread if it is not already running.
pub fn ensure_builder() {
    LazyLock::force(&BUILDER);
}

fn run_builder() {
    loop {
        let name = BUILDER.take();
        let Some(index) = get(&name) else { continue };
        let Some(store) = Store::detached() else {
            continue;
        };
        match refill(&store, &index) {
            Ok(count) => {
                index.mark_refilled();
                eprintln!("ArcVector: refilled index '{name}' from {count} element(s)");
            }
            Err(e) => {
                eprintln!("ArcVector: could not rebuild index '{name}': {e}");
                remove(&name);
            }
        }
    }
}

/// Replay Map into the graph, leaving anything the live path has touched alone.
fn refill(store: &Store, index: &VectorIndex) -> Result<usize> {
    let probe = store.probe_map(&index.name)?;
    index.ann.reserve(probe.count as usize)?;

    let layout = index.ann.layout;
    let mut added = 0usize;
    for (field, value) in store.get_all(&index.name)? {
        if field == META_FIELD {
            continue;
        }
        let element = layout
            .decode(&value)
            .map_err(|e| Error::bad_request(format!("{e} in element '{field}'")))?;
        if index.ann.add_unless_known(&field, element.vector)? {
            added += 1;
        }
    }
    Ok(added)
}

/// Claim a refilled graph, on a worker thread that has a connection.
pub fn claim_refilled(store: &Store, index: &VectorIndex) -> Result<()> {
    match read_metadata(store, &index.name) {
        MetaState::Usable(meta, _) if meta.owner == REBUILDING => {}
        MetaState::Usable(..) | MetaState::Newer => return Err(Error::NoSuchIndex),
        MetaState::Damaged(why) => return Err(Error::bad_request(why)),
    }
    let owner = mint_owner();
    stamp(store, &index.name, owner)?;
    index.ann.end_rebuild();
    index.set_owner(owner);
    eprintln!(
        "ArcVector: index '{}' is serving again, owner={owner:016x}",
        index.name
    );
    Ok(())
}
