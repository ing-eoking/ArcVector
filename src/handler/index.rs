use std::fmt::Write as _;

#[cfg(recovery)]
use super::access::resolve;
use crate::command::request::Create;
use crate::error::{Error, Reply, Result};
use crate::handler::arcus::element::{self, Layout, MetaRecord};
use crate::handler::arcus::engine::{self, Store, StoreError};
use crate::handler::quant::Quant;
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::AnnIndex;

/// `vcreate <index> <dim> [METRIC …] [QUANT …] [MAXCOUNT …] [EXPTIME …] [M …] …`
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

    // Before the Map, so an unsupported metric leaves no empty Map behind.
    let ann = AnnIndex::new(
        layout,
        metric,
        spec.connectivity,
        spec.expansion_add,
        spec.expansion_search,
        std::sync::Arc::new(engine::DetachedElements),
    )?;

    let held = map_size_for(store, spec.maxcount);
    let maxcount = held - 1;
    let owner = element::mint_owner();
    let meta = MetaRecord {
        metric: metric.as_str().to_owned(),
        connectivity: spec.connectivity,
        expansion_add: spec.expansion_add,
        expansion_search: spec.expansion_search,
        owner,
    };
    let index = VectorIndex::new(name.to_owned(), ann, maxcount, owner);

    // Registered first, written to the engine last. The engine write is what carries this
    // create off the node — `map_elem_insert` emits `CLOG_MAP_ELEM_INSERT` — so everything that
    // can fail goes in front of it, the registry's own growth included. Answering for a Map
    // already made and then failing would leave only a delete to take it back.
    //
    // The entry lands unpublished, which is what makes the gap safe without holding a lock
    // across the engine call. A command that reads the metadata in here finds no Map and is
    // right about what it saw — so the entry says "not yet" itself, and nothing releases it.
    let (registered, previous) = match registry::put(index) {
        Ok(pair) => pair,
        Err(e) => {
            return Err(Error::Index(format!(
                "the index registry could not grow: {e}"
            )));
        }
    };

    // One engine call settles the name. Nothing probes it first: a probe answers about a
    // moment that has already passed, and every case it could report comes back from this
    // call anyway — decided under the engine's own cache lock, so there is no window between
    // the look and the act.
    //
    //   Ok(true)         the name was free; the engine made the Map and took the element
    //   Ok(false)        a Map was there without metadata — not an index, and not ours
    //   Err(ElemExists)  a Map was there with metadata — already an index
    //   Err(BadType)     the name holds something that is not a Map
    //
    // A Map without this element is not an index: `resolve` cannot read it and only `vdrop`
    // can clear it. The engine unlinks the Map again if the element cannot go in, all under
    // that same lock, so that state has no window to exist in either.
    let settled = store
        .alloc_elem(name, element::META_FIELD, &meta.encode(layout))
        .and_then(|pending| pending.insert_creating(engine::index_attr(Some(held), spec.exptime)));

    match settled {
        // Ours, both of them. Publishing is the last step and the only one that cannot fail:
        // from here the entry answers, and a release may take it. Whatever the name held before
        // is a graph whose Map had expired or been evicted, and dropping it here frees the
        // usearch graph and the id mapping with it.
        Ok(true) => {
            registered.publish();
            if previous.is_some() {
                eprintln!(
                    "ArcVector: index '{name}' had no Map; released the graph it was built from"
                );
            }
            Ok(Reply::Created)
        }
        // Not ours. Put the registry back — only if the name still holds this call's entry,
        // since a racing `vcreate` displacing it owns what it put — and take the element back
        // out of a Map we did not make: a plain `mop` Map, or one a replication transfer is
        // still filling. The Map is as we found it, since `delete_elem` passes `drop_if_empty`
        // false and emptying it does not drop it.
        Ok(false) => {
            registry::unput(&registered, previous);
            let _ = store.delete_elem(name, element::META_FIELD);
            already_there(store, name)
        }
        Err(e) => {
            registry::unput(&registered, previous);
            match e {
                StoreError::ElemExists => already_there(store, name),
                StoreError::BadType => Err(Error::bad_request(format!(
                    "'{name}' holds an item that is not a Map"
                ))),
                e => Err(e.into()),
            }
        }
    }
}

/// The answer for a name a Map already occupies. Never clears it.
fn already_there(store: &Store, name: &str) -> Result<Reply> {
    #[cfg(recovery)]
    {
        // Reads the metadata: an index gets adopted, an unreadable one gets discarded.
        resolve(store, name)?;
        Ok(Reply::Exists)
    }
    // Nothing can rebuild from it, and guessing would destroy it or serve an empty graph.
    #[cfg(not(recovery))]
    {
        let _ = store;
        Err(Error::bad_request(format!(
            "a Map already exists at '{name}' and this build cannot rebuild an \
             index from it; drop it with 'vdrop {name}' and create it again"
        )))
    }
}

fn check_dimension_fits(store: &Store, layout: Layout, quant: Quant) -> Result<()> {
    let limit = store.max_element_bytes() as usize;
    // Measured with the vector in the element even where this build leaves it out, so the
    // dimension a server accepts does not depend on how the module was compiled.
    if layout.full_stored_len() <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element would be {} bytes, over max_element_bytes {limit} \
         (max dimension is {} for quant {quant})",
        layout.full_stored_len(),
        Layout::max_dim_for(quant, limit),
    )))
}

fn map_size_for(store: &Store, maxcount: Option<u32>) -> u32 {
    let ceiling = store.max_map_size();
    maxcount.unwrap_or(ceiling).saturating_add(1).min(ceiling)
}

pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    // The Map goes first: a real failure has to leave the registry alone, or the index
    // stops serving a Map that is still there and the client hears DROPPED anyway.
    // Attempted regardless of the registry, so vdrop can clear an orphan Map.
    let dropped = match store.drop_map(name) {
        Ok(()) => true,
        Err(StoreError::KeyGone) => false,
        Err(e) => return Err(e.into()),
    };
    let known = registry::remove(name);

    Ok(if known || dropped {
        Reply::Dropped
    } else {
        Reply::NotFound
    })
}

/// `vstats` — module memory, which arcus's own accounting cannot see.
pub fn vstats() -> Result<Reply> {
    let indexes = registry::snapshot();

    let mut vectors = 0usize;
    let mut idmap_bytes = 0usize;
    let mut held_bytes = 0usize;
    let mut used_bytes = 0usize;
    let mut per_index = String::new();
    for index in &indexes {
        let (count, idmap, held, used) = (
            index.ann.len(),
            index.ann.id_map_bytes(),
            index.ann.held_bytes(),
            index.ann.used_bytes(),
        );
        vectors += count;
        idmap_bytes += idmap;
        held_bytes += held;
        used_bytes += used;

        let name = &index.name;
        let _ = write!(
            per_index,
            "STAT {name}:vectors {count}\r\n\
             STAT {name}:idmap_bytes {idmap}\r\n\
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
         STAT idmap_bytes {idmap_bytes}\r\n\
         STAT index_held_bytes {held_bytes}\r\n\
         STAT index_used_bytes {used_bytes}\r\n\
         STAT attr_bytes_per_vector {}\r\n",
        indexes.len(),
        element::ATTR_BYTES,
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
