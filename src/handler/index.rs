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
    // Read before the insert, because that is what a release below can be about: an entry
    // registered after this point was built from the Map this call is about to make, and is
    // the opposite of stale.
    let stale = registry::get(name);
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
    let created = match store
        .alloc_elem(name, element::META_FIELD, &meta.encode(layout))
        .and_then(|pending| pending.insert_creating(engine::index_attr(Some(held), spec.exptime)))
    {
        Ok(created) => created,
        Err(StoreError::ElemExists) => return already_there(store, name),
        Err(StoreError::BadType) => {
            return Err(Error::bad_request(format!(
                "'{name}' holds an item that is not a Map"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    if !created {
        // A Map was already there, and one that already had metadata would have refused the
        // element above. This one had none, so it is not an index and the element went into a
        // Map we did not make — a plain `mop` Map, or one a replication transfer is still
        // filling. Take it back out and answer for the Map that was there. Nothing else was
        // touched: the Map is as we found it (`delete_elem` passes `drop_if_empty` false, so
        // emptying it does not drop it).
        let _ = store.delete_elem(name, element::META_FIELD);
        return already_there(store, name);
    }

    // The name was free until this call, so the entry read above is a graph whose Map expired
    // or was evicted. Release it here, or the insert below finds it and answers `EXISTS` —
    // that reply would leave the fresh empty Map paired with the stale graph, and in a build
    // that cannot rebuild, nothing would ever notice.
    if let Some(index) = &stale {
        super::access::map_is_gone(name, index);
    }

    let index = VectorIndex::new(name.to_owned(), ann, maxcount, owner);
    let (_, inserted) = match registry::insert_or_get(index) {
        Ok(pair) => pair,
        // The Map exists and nothing serves it. This is the one place in `vcreate` with
        // something to undo: the insert above is what created it, so taking it back leaves the
        // name as free as we found it. `vdrop` is otherwise the only path that deletes a Map,
        // and this stays inside that rule — we are deleting the Map we made, in the call that
        // made it, having never answered for it.
        Err(e) => {
            let _ = store.drop_map(name);
            return Err(Error::Index(format!(
                "the index registry could not grow: {e}"
            )));
        }
    };
    Ok(if inserted {
        Reply::Created
    } else {
        // We hold the name in the engine, so no racing `vcreate` can be the one registered
        // here: the loser of that race is refused at `insert_creating` with `ELEM_EEXISTS`
        // and never gets this far. This is a graph a concurrent `resolve` built from the Map
        // we just made, between the release above and now — answer that it is already there.
        Reply::Exists
    })
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
    if layout.stored_len() <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element would be {} bytes, over max_element_bytes {limit} \
         (max dimension is {} for quant {quant})",
        layout.stored_len(),
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
