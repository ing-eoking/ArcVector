use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};

use crate::handler::access::meta::{MetaState, read_metadata};

use super::registry::{COLD, DRAINING, FILLING, VectorIndex, get, remove};
use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord};
use crate::handler::arcus::engine::{HeldMap, Store};
use crate::handler::usearch::{AnnIndex, Metric};
use crate::owner;

/// How long the rebuild thread waits before looking for a cookie again.
const NO_COOKIE_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

pub fn drain(store: &Store, index: &Arc<VectorIndex>) -> Result<()> {
    let was = index.state();
    if was == DRAINING {
        return Ok(());
    }
    index.mark_state(DRAINING);
    index.refilled.store(false, Ordering::Release);

    if let Err(e) = stamp(store, &index.name, owner::NOBODY) {
        index.mark_state(was);
        return Err(e);
    }

    if index.ann.is_empty() {
        index.mark_state(COLD);
        return Ok(());
    }
    DRAINER.enqueue(Arc::clone(index));
    Ok(())
}

/// Marks an index for rebuild and hands the name to the rebuild thread.
///
/// The snapshot is not taken here. `hold_all` walks the whole Map and takes a
/// refcount on every element, which on a large index is far longer than a
/// request should be held for -- and it is the rebuild thread that needs it.
/// It could only be done here while background threads had no cookie to pass a
/// key with; [`Store::background_keyed`] removed that constraint.
pub fn fill(store: &Store, index: &Arc<VectorIndex>) -> Result<()> {
    if !index.enter(COLD, FILLING) {
        return Ok(());
    }
    index.refilled.store(false, Ordering::Release);
    index.ann.begin_fill();

    // How many elements the rebuild has to place is what decides whether the
    // caller waits it out, so it has to be known before this returns. `getattr`
    // answers from the Map's own counter and costs nothing -- unlike the
    // snapshot, which walks every element. The thread corrects this with the
    // count it actually gets.
    let size = store
        .probe_map(&index.name)
        .map_or(0, |map| map.count.saturating_sub(1) as usize);
    index.set_rebuild_size(size);

    if !BUILDER.enqueue(&index.name) {
        abandon(index);
        return Err(Error::Index(format!(
            "could not queue the rebuild of '{}'",
            index.name
        )));
    }
    Ok(())
}

/// Puts an index back where a later read will try the rebuild again.
fn abandon(index: &VectorIndex) {
    index.ann.end_rebuild();
    index.mark_state(COLD);
}

struct Drainer {
    queue: Mutex<Vec<Arc<VectorIndex>>>,
    wake: Condvar,
}

impl Drainer {
    fn enqueue(&self, index: Arc<VectorIndex>) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.try_reserve(1).is_err() {
            drop(queue);
            finish_drain(&index);
            return;
        }
        queue.push(index);
        self.wake.notify_one();
    }

    fn take(&self) -> Arc<VectorIndex> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(index) = queue.pop() {
                return index;
            }
            queue = self
                .wake
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

static DRAINER: LazyLock<Drainer> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("arcvector-drain".to_owned())
        .spawn(run_drainer)
        .expect("spawn the drain thread");
    Drainer {
        queue: Mutex::new(Vec::new()),
        wake: Condvar::new(),
    }
});

fn run_drainer() {
    loop {
        let index = DRAINER.take();
        finish_drain(&index);
    }
}

fn finish_drain(index: &VectorIndex) {
    if let Err(e) = index.ann.drop_all() {
        eprintln!(
            "ArcVector: could not empty the graph of '{}' ({e}); dropping the index",
            index.name
        );
        remove(&index.name);
        return;
    }
    index.mark_state(COLD);
}

struct Builder {
    queue: Mutex<Vec<String>>,
    wake: Condvar,
}

impl Builder {
    /// False only if the name could not be recorded at all, which leaves the
    /// caller to put the index back.
    fn enqueue(&self, name: &str) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.iter().any(|queued| queued == name) {
            return true;
        }
        if queue.try_reserve(1).is_err() {
            return false;
        }
        queue.push(name.to_owned());
        self.wake.notify_one();
        true
    }

    /// Puts a name back after a round that could not run. The index stays
    /// FILLING meanwhile, so no read queues it a second time.
    fn requeue(&self, name: String) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.try_reserve(1).is_err() {
            return false;
        }
        queue.push(name);
        true
    }

    fn take(&self) -> String {
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
    LazyLock::force(&DRAINER);
}

fn run_builder() {
    loop {
        let name = BUILDER.take();
        let Some(index) = get(&name) else { continue };

        // `hold_all` takes a key, so it needs a cookie. Until `attach` has a
        // connection parked there is none, and the name goes back rather than
        // the rebuild being lost.
        let Some(store) = Store::background_keyed() else {
            if !BUILDER.requeue(name) {
                abandon(&index);
            }
            std::thread::sleep(NO_COOKIE_RETRY);
            continue;
        };

        let mut held = match store.hold_all(&name) {
            Ok(held) => held,
            Err(e) => {
                eprintln!("ArcVector: could not read the Map of index '{name}' ({e})");
                abandon(&index);
                continue;
            }
        };
        index.set_rebuild_size(held.len());

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

fn refill(store: &Store, index: &VectorIndex, held: &mut HeldMap) -> Result<usize> {
    index.ann.reserve(held.len())?;

    let layout = index.ann.layout;
    let mut added = 0usize;
    for (addr, field, value) in held.read(store) {
        if field == META_FIELD.as_bytes() {
            continue;
        }
        if index.state() != FILLING {
            return Ok(added);
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
            added += 1;
            held.keep(addr);
        }
    }
    Ok(added)
}

/// What a rebuild that just finished should record as this index's owner,
/// given what the Map currently says -- or `None` when there is nothing this
/// node may claim.
///
/// A master runs a claim protocol: the Map must still say `owner::NOBODY`
/// (what `drain` left there) before this node may write its own token over
/// it, so a Map naming anyone else means the claim was lost while this
/// rebuild ran, and the result has to be discarded rather than served.
///
/// A replica never gets to win or lose that race -- `stamp` skips its write
/// entirely -- so there is no claim to make. It records the Map's value
/// verbatim, master's token or `NOBODY` alike, because that is exactly what
/// `resolve` will keep comparing against on every later request. Recording
/// anything else here -- including always assuming `NOBODY` -- would make
/// that comparison disagree the moment the Map actually names a master, and
/// `resolve` would call `drain` again, forever.
fn owner_to_record(is_replica: bool, map_owner: &str) -> Option<String> {
    if is_replica {
        Some(map_owner.to_owned())
    } else if map_owner == owner::NOBODY {
        Some(owner::ours().to_owned())
    } else {
        None
    }
}

pub fn claim_refilled(store: &Store, index: &VectorIndex) -> Result<()> {
    if index.state() != FILLING || !index.take_refilled() {
        return Ok(());
    }
    let map_owner = match read_metadata(store, &index.name) {
        MetaState::Usable(meta, _) => meta.owner,
        MetaState::NoMap => {
            index.mark_refilled();
            return Err(Error::NoSuchIndex);
        }
        MetaState::Damaged(why) => {
            index.mark_refilled();
            return Err(Error::bad_request(why));
        }
        MetaState::Unknown(e) => {
            index.mark_refilled();
            return Err(e.into());
        }
    };

    #[cfg(feature = "replication")]
    let is_replica = crate::repl::role::is_replica();
    #[cfg(not(feature = "replication"))]
    let is_replica = false;

    let Some(record) = owner_to_record(is_replica, &map_owner) else {
        index.mark_refilled();
        return Err(Error::NoSuchIndex);
    };

    // On a replica this is a no-op: `stamp` returns `Ok(())` without writing,
    // since a replica never gets to hold the ownership token. On a master
    // `record` is `owner::ours()` here (see `owner_to_record`), so this is
    // the same write `claim_refilled` always made.
    if let Err(e) = stamp(store, &index.name, &record) {
        index.mark_refilled();
        return Err(e);
    }
    index.ann.end_rebuild();
    index.mark_owned_by(&record);

    eprintln!(
        "ArcVector: index '{}' is serving again, owner={record}",
        index.name
    );
    Ok(())
}

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

pub(super) fn stamp(store: &Store, name: &str, owner: &str) -> Result<()> {
    #[cfg(feature = "replication")]
    if crate::repl::role::is_replica() {
        return Ok(());
    }
    let (meta, layout) = match read_metadata(store, name) {
        MetaState::Usable(meta, layout) => (meta, layout),
        MetaState::NoMap => return Err(Error::NoSuchIndex),
        MetaState::Damaged(why) => return Err(Error::bad_request(why)),
        MetaState::Unknown(e) => return Err(e.into()),
    };
    let claimed = MetaRecord {
        owner: owner.to_owned(),
        ..meta
    };
    store.put_elem(name, META_FIELD, &claimed.encode(layout))?;
    Ok(())
}

#[cfg(all(test, feature = "replication"))]
mod role_tests {
    use crate::repl::role::{Probe, shared};

    #[test]
    fn a_replica_skips_the_ownership_write() {
        shared().observe(Probe::Refused);
        assert!(
            crate::repl::role::is_replica(),
            "stamp reads this instead of asking the engine, which cannot answer"
        );
    }
}

/// `owner_to_record` is the pure decision `claim_refilled` defers to; these
/// drive it directly, without a `Store`, so a regression in the branching
/// (say, dropping the replica case and always claiming) fails a test instead
/// of only showing up as churn against a live engine.
#[cfg(test)]
mod owner_to_record_tests {
    use super::owner_to_record;
    use crate::owner;

    #[test]
    fn a_replica_records_the_master_s_token_as_is() {
        assert_eq!(
            owner_to_record(true, "some-master-token"),
            Some("some-master-token".to_owned()),
            "a replica observes the Map instead of claiming it"
        );
    }

    #[test]
    fn a_replica_records_nobody_when_the_map_says_so() {
        assert_eq!(
            owner_to_record(true, owner::NOBODY),
            Some(owner::NOBODY.to_owned())
        );
    }

    #[test]
    fn a_master_claims_its_own_token_when_the_map_says_nobody() {
        assert_eq!(
            owner_to_record(false, owner::NOBODY),
            Some(owner::ours().to_owned())
        );
    }

    #[test]
    fn a_master_cannot_claim_a_map_already_naming_someone_else() {
        assert_eq!(
            owner_to_record(false, "some-other-token"),
            None,
            "the claim was lost while the rebuild ran; nothing to record"
        );
    }
}
