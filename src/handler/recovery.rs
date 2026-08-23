//! A worker queues the name, a connectionless thread refills, the next command stamps a fresh token — that thread must never write, since the engine derefs the cookie.

use std::sync::{Condvar, LazyLock, Mutex, PoisonError};

use crate::handler::access::meta::{MetaState, read_metadata};

use super::registry::{REBUILDING, VectorIndex, get, remove};
use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord, mint_owner};
use crate::handler::arcus::engine::{HeldMap, Store};
use crate::handler::usearch::{AnnIndex, Metric};

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
        // The Map says rebuilding and this node cannot do it, so drop the entry.
        remove(&index.name);
        return Err(e);
    }

    // The snapshot is taken here, on this thread, because this is the one with a connection.
    // `map_elem_get` is the refill's only call the engine may route through
    // `ACTION_BEFORE_READ`, and with `ENABLE_MIGRATION` that path passes the cookie on — the
    // rebuild thread has none. What it gets instead is the hold, which needs neither.
    //
    // After `begin_rebuild`, so every delete from here on is tombstoned and the refill cannot
    // put one back.
    let held = match store.hold_all(&index.name) {
        Ok(held) => held,
        Err(e) => {
            remove(&index.name);
            return Err(e.into());
        }
    };
    BUILDER.enqueue(&index.name, held);
    Ok(())
}

/// Asleep until there is work; no polling.
struct Builder {
    queue: Mutex<Vec<(String, HeldMap)>>,
    wake: Condvar,
}

impl Builder {
    fn enqueue(&self, name: &str, held: HeldMap) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        // A second takeover of the same name supersedes the first: its snapshot is the newer
        // one, and dropping the older releases those holds now rather than after a refill that
        // is no longer wanted.
        queue.retain(|(q, _)| q != name);
        queue.push((name.to_owned(), held));
        self.wake.notify_one();
    }

    fn take(&self) -> (String, HeldMap) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(work) = queue.pop() {
                return work;
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

pub fn ensure_builder() {
    LazyLock::force(&BUILDER);
}

fn run_builder() {
    loop {
        let (name, mut held) = BUILDER.take();
        let Some(index) = get(&name) else { continue };
        // Only for the engine handle. Every call this thread makes from here — `get_elem_info`
        // and, when `held` drops, `map_elem_release` — ignores the cookie, which is why a
        // connectionless thread may make them and why the fetch happened on a worker.
        let Some(store) = Store::detached() else {
            continue;
        };
        match refill(&store, &index, &mut held) {
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
///
/// `held` is the snapshot the worker took at takeover, still held. Reading it needs no engine
/// call that wants a connection, and the hold is what keeps the bytes readable: an element
/// unlinked since is not freed while the refcount stands.
///
/// A held value can be *stale* — a live write may have replaced its element — but never
/// replayed. `add_unless_known` decides under the mapping's write lock, the same one a `vadd`
/// holds across both of its registrations and a `vdel` across its tombstone, so any id the live
/// path has touched is already known here and skipped.
fn refill(store: &Store, index: &VectorIndex, held: &mut HeldMap) -> Result<usize> {
    let mut kept: Vec<u64> = Vec::new();
    index.ann.reserve(held.len())?;

    let layout = index.ann.layout;
    let mut added = 0usize;
    for (addr, field, value) in held.read(store) {
        if field == META_FIELD.as_bytes() {
            continue;
        }
        let element = layout.decode(&value).map_err(|e| {
            Error::bad_request(format!(
                "{e} in element {}",
                String::from_utf8_lossy(&field)
            ))
        })?;
        let vector = element.vector.to_vec();
        let replayed = index
            .ann
            .add_unless_known(addr, || Ok(Some(vector.clone())))?;
        if replayed {
            // The graph now keys a node by this address, so its refcount has to outlive the
            // snapshot. `keep` is what stops the release below from freeing it.
            added += 1;
            kept.push(addr);
        }
    }
    for addr in kept {
        held.keep(addr);
    }
    Ok(added)
}

/// Claim a refilled graph, on a worker thread that has a connection.
pub fn claim_refilled(store: &Store, index: &VectorIndex) -> Result<()> {
    match read_metadata(store, &index.name) {
        MetaState::Usable(meta, _) if meta.owner == REBUILDING => {}
        MetaState::Usable(..) | MetaState::NoMap => return Err(Error::NoSuchIndex),
        MetaState::Damaged(why) => return Err(Error::bad_request(why)),
        MetaState::Unknown(e) => return Err(e.into()),
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

/// Build the empty graph an adoption starts from, out of what the metadata recorded.
pub fn build_ann(meta: &MetaRecord, layout: Layout) -> Result<AnnIndex> {
    let metric = Metric::parse(&meta.metric)
        .ok_or_else(|| Error::bad_request(format!("unknown metric '{}'", meta.metric)))?;
    AnnIndex::new(
        layout,
        metric,
        meta.connectivity,
        meta.expansion_add,
        meta.expansion_search,
        std::sync::Arc::new(crate::handler::arcus::engine::DetachedElements),
    )
}

/// Write `owner` into the Map's metadata element, keeping everything else.
pub(super) fn stamp(store: &Store, name: &str, owner: u64) -> Result<()> {
    let (meta, layout) = match read_metadata(store, name) {
        MetaState::Usable(meta, layout) => (meta, layout),
        MetaState::NoMap => return Err(Error::NoSuchIndex),
        MetaState::Damaged(why) => return Err(Error::bad_request(why)),
        MetaState::Unknown(e) => return Err(e.into()),
    };
    let claimed = MetaRecord { owner, ..meta };
    store.put_elem(name, META_FIELD, &claimed.encode(layout))?;
    Ok(())
}
