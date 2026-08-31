use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};

use crate::handler::access::meta::{MetaState, read_metadata};

use super::registry::{COLD, DRAINING, FILLING, VectorIndex, get, remove};
use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord};
use crate::handler::arcus::engine::{HeldMap, Store};
use crate::handler::usearch::{AnnIndex, Metric};
use crate::owner;

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

pub fn fill(store: &Store, index: &Arc<VectorIndex>) -> Result<()> {
    if !index.enter(COLD, FILLING) {
        return Ok(());
    }
    index.refilled.store(false, Ordering::Release);
    index.ann.begin_fill();

    let held = match store.hold_all(&index.name) {
        Ok(held) => held,
        Err(e) => {
            index.ann.end_rebuild();
            index.mark_state(COLD);
            return Err(e.into());
        }
    };
    index.set_rebuild_size(held.len());
    BUILDER.enqueue(&index.name, held);
    Ok(())
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
    queue: Mutex<Vec<(String, HeldMap)>>,
    wake: Condvar,
}

impl Builder {
    fn enqueue(&self, name: &str, held: HeldMap) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);

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
    LazyLock::force(&DRAINER);
}

fn run_builder() {
    loop {
        let (name, mut held) = BUILDER.take();
        let Some(index) = get(&name) else { continue };

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

pub fn claim_refilled(store: &Store, index: &VectorIndex) -> Result<()> {
    if index.state() != FILLING || !index.take_refilled() {
        return Ok(());
    }
    match read_metadata(store, &index.name) {
        MetaState::Usable(meta, _) if meta.owner == owner::NOBODY => {}
        MetaState::Usable(..) | MetaState::NoMap => {
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
    }
    let token = owner::ours();
    if let Err(e) = stamp(store, &index.name, token) {
        index.mark_refilled();
        return Err(e);
    }
    index.ann.end_rebuild();
    index.mark_ours();
    eprintln!(
        "ArcVector: index '{}' is serving again, owner={token}",
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
    if store.ping_slave() {
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
