use std::sync::{Condvar, LazyLock, Mutex, PoisonError};

use crate::handler::access::meta::{MetaState, read_metadata};

use super::registry::{REBUILDING, VectorIndex, get, remove};
use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord, mint_owner};
use crate::handler::arcus::engine::{HeldMap, Store};
use crate::handler::usearch::{AnnIndex, Metric};

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
        remove(&index.name);
        return Err(e);
    }

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
    if !index.take_refilled() {
        return Ok(());
    }
    match read_metadata(store, &index.name) {
        MetaState::Usable(meta, _) if meta.owner == REBUILDING => {}
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
    let owner = mint_owner();
    if let Err(e) = stamp(store, &index.name, owner) {
        index.mark_refilled();
        return Err(e);
    }
    index.ann.end_rebuild();
    index.set_owner(owner);
    eprintln!(
        "ArcVector: index '{}' is serving again, owner={owner:016x}",
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
