use std::fmt::Write as _;

use crate::command::request::Create;
use crate::error::{Error, Reply, Result};
use crate::handler::access::resolve;
use crate::handler::access::sweep;
use crate::handler::arcus::element::{self, Layout, MetaRecord};
use crate::handler::arcus::engine::{self, Store, StoreError};
use crate::handler::quant::Quant;
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::AnnIndex;

pub fn vcreate(store: &Store, spec: &Create) -> Result<Reply> {
    let Create {
        index: name,
        dim,
        metric,
        quant,
        ..
    } = *spec;

    let layout = Layout::new(dim, quant);
    check_dimension_fits(store, layout, quant)?;

    let ann = AnnIndex::new(
        layout,
        metric,
        spec.connectivity,
        spec.expansion_add,
        spec.expansion_search,
        std::sync::Arc::new(engine::ItemElements),
    )?;

    let maxcount = spec.maxcount.unwrap_or(DEFAULT_MAXCOUNT);
    let meta = MetaRecord {
        metric: metric.as_str().to_owned(),
        connectivity: spec.connectivity,
        expansion_add: spec.expansion_add,
        expansion_search: spec.expansion_search,
        maxcount,
    };
    let index = VectorIndex::building(name.to_owned(), ann, maxcount);

    let (registered, previous) = match registry::put(index) {
        Ok(pair) => pair,
        Err(e) => {
            return Err(Error::Index(format!(
                "the index registry could not grow: {e}"
            )));
        }
    };

    match store.add_meta(name, &meta, layout) {
        Ok(()) => {
            // 이 이름으로 먼저 도착해 기다리던 벡터가 있으면 여기서 그래프에
            // 넣는다. 보통은 없어서 곧장 지나간다.
            crate::trigger::recover::adopt(&registered);
            registered.publish();
            registered.mark_serving();
            if previous.is_some() {
                eprintln!(
                    "ArcVector: index '{name}' had no metadata; released the graph it stood on"
                );
                sweep::retire(previous);
            }
            Ok(Reply::Created)
        }

        // `add` refuses a key already there, which is exactly the question
        // `vcreate` is asking.
        Err(StoreError::NotStored) => {
            registry::unput(&registered, previous);
            already_there(store, name)
        }
        Err(e) => {
            registry::unput(&registered, previous);
            Err(e.into())
        }
    }
}

/// What an index accepts when `vcreate` names no limit.
///
/// A Map had `maxcount` as an attribute and the engine enforced it. Nothing
/// enforces a count across separate items, so this is ArcVector's own ceiling
/// and the graph is what counts against it.
const DEFAULT_MAXCOUNT: u32 = 50_000;

fn already_there(store: &Store, name: &str) -> Result<Reply> {
    resolve(store, name)?;
    Ok(Reply::Exists)
}

pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    // Stop serving the name first, so nothing reaches a graph that is being
    // taken apart -- but stay in the registry until the flush is done.
    //
    // Those two are not the same thing, and the difference is what the unlink
    // callback needs. Each vector the flush unlinks fires that callback, and
    // it decides whether this crate is holding the item by asking the graph
    // (`trigger::event`). Removing the entry outright takes the question away:
    // the callback finds no index, reads it as "we never took this one", and
    // every vector of a dropped index stays pinned for the life of the process
    // -- usearch cannot enumerate its keys, so nothing else can find them.
    //
    // The flush is not a promise that every vector is gone. It stamps the
    // prefix and walks part of the LRU, leaving the rest to go when touched;
    // those late ones do arrive after the entry is gone, and those leak. This
    // order keeps that to the stragglers instead of all of them.
    if let Some(index) = registry::get(name) {
        index.mark_draining();
    }
    let flushed = store.flush_prefix(&crate::trigger::key::index_prefix(name));

    let had_meta = match store.delete_kv(&crate::trigger::key::meta_key(name)) {
        Ok(()) => true,
        Err(StoreError::KeyGone) => false,
        Err(e) => return Err(e.into()),
    };
    let known = registry::remove(name);

    if had_meta && let Err(e) = flushed {
        eprintln!("ArcVector: '{name}' was dropped, but its vectors were not flushed ({e})");
    }

    Ok(if known || had_meta {
        Reply::Dropped
    } else {
        Reply::NotFound
    })
}

fn check_dimension_fits(store: &Store, layout: Layout, quant: Quant) -> Result<()> {
    let limit = store.max_item_size() as usize;

    if layout.full_stored_len() <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element would be {} bytes, over max_item_size {limit} \
         (max dimension is {} for quant {quant})",
        layout.full_stored_len(),
        Layout::max_dim_for(quant, limit),
    )))
}

pub fn vstats() -> Result<Reply> {
    let indexes = registry::snapshot();

    let mut vectors = 0usize;
    let mut held_bytes = 0usize;
    let mut used_bytes = 0usize;
    let mut per_index = String::new();
    for index in &indexes {
        let (count, held, used) = (
            index.ann.len(),
            index.ann.held_bytes(),
            index.ann.used_bytes(),
        );
        vectors += count;
        held_bytes += held;
        used_bytes += used;

        let name = &index.name;
        let _ = write!(
            per_index,
            "STAT {name}:vectors {count}\r\n\
             STAT {name}:index_held_bytes {held}\r\n\
             STAT {name}:index_used_bytes {used}\r\n\
             STAT {name}:reserved {}\r\n",
            index.ann.reserved()
        );
    }

    let mut out = String::new();
    let _ = write!(
        out,
        "STAT indexes {}\r\n\
         STAT vectors {vectors}\r\n\
         STAT index_held_bytes {held_bytes}\r\n\
         STAT index_used_bytes {used_bytes}\r\n\
         STAT attr_bytes_per_vector {}\r\n\
         STAT vectors_awaiting_metadata {}\r\n",
        indexes.len(),
        element::ATTR_BYTES,
        crate::trigger::waiting::count(),
    );
    out.push_str(&per_index);
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

pub fn vlist() -> Result<Reply> {
    let mut out = String::new();
    for index in registry::snapshot() {
        let layout = &index.ann.layout;
        let _ = writeln!(
            out,
            "INDEX {} dim={} quant={} metric={} attrbytes={} count={} maxcount={}\r",
            index.name,
            layout.dim,
            layout.quant,
            index.ann.metric,
            element::ATTR_BYTES,
            index.ann.len(),
            index.maxcount,
        );
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
