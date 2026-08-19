//! Commands that act on an index as a whole.

use std::fmt::Write as _;

#[cfg(recovery)]
use super::access::resolve;
use crate::command::request::Create;
use crate::error::{Error, Reply, Result};
use crate::handler::arcus::element::{self, Layout, MetaRecord, Quant};
use crate::handler::arcus::engine::Store;
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::{AnnIndex, THREAD_SLOTS};

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

    // Replication or persistence may have delivered this Map. Never clear it.
    if store.probe_map(name).is_ok() {
        // Replication or persistence may have delivered this Map. Never clear it.
        #[cfg(recovery)]
        {
            resolve(store, name)?;
            return Ok(Reply::Exists);
        }
        // Nothing here can rebuild an index from it, and guessing would either
        // destroy it or serve an empty graph. Name the situation instead.
        #[cfg(not(recovery))]
        return Err(Error::bad_request(format!(
            "a Map already exists at '{name}' and this build cannot rebuild an \
             index from it; drop it with 'vdrop {name}' and create it again"
        )));
    }

    // Before the Map, so an unsupported metric leaves no empty Map behind.
    let ann = AnnIndex::new(
        layout,
        metric,
        spec.connectivity,
        spec.expansion_add,
        spec.expansion_search,
        THREAD_SLOTS,
    )?;

    let held = map_size_for(store, spec.maxcount);
    store.create_map(name, Some(held), spec.exptime)?;
    let maxcount = held - 1;
    let owner = element::mint_owner();
    let meta = MetaRecord {
        metric: metric.as_str().to_owned(),
        connectivity: spec.connectivity,
        expansion_add: spec.expansion_add,
        expansion_search: spec.expansion_search,
        owner,
    };
    store.put_elem(name, element::META_FIELD, &meta.encode(layout))?;

    let index = VectorIndex::new(name.to_owned(), ann, maxcount, owner);
    let (_, inserted) = registry::insert_or_get(index);
    Ok(if inserted {
        Reply::Created
    } else {
        Reply::Exists
    })
}

/// Refuse a dimension whose element would exceed the engine's per-element limit.
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

/// How large the Map must be for `maxcount` vectors.
fn map_size_for(store: &Store, maxcount: Option<u32>) -> u32 {
    let ceiling = store.max_map_size();
    maxcount.unwrap_or(ceiling).saturating_add(1).min(ceiling)
}

pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    let known = registry::remove(name);
    // Regardless of the registry, so vdrop can clear an orphan Map.
    let dropped = store.drop_map(name).is_ok();

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
