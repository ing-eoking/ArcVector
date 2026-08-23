use std::fmt::Write as _;

use crate::command::request::Create;
use crate::error::{Error, Reply, Result};
#[cfg(recovery)]
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

    let (registered, previous) = match registry::put(index) {
        Ok(pair) => pair,
        Err(e) => {
            return Err(Error::Index(format!(
                "the index registry could not grow: {e}"
            )));
        }
    };

    let settled = store
        .alloc_elem(name, element::META_FIELD, &meta.encode(layout))
        .and_then(|pending| pending.insert_creating(engine::index_attr(Some(held), spec.exptime)));

    match settled {
        Ok(true) => {
            registered.publish();
            if previous.is_some() {
                eprintln!(
                    "ArcVector: index '{name}' had no Map; released the graph it was built from"
                );

                sweep::retire(previous);
            }
            Ok(Reply::Created)
        }

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

fn already_there(store: &Store, name: &str) -> Result<Reply> {
    #[cfg(recovery)]
    {
        resolve(store, name)?;
        Ok(Reply::Exists)
    }

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

pub fn vstats() -> Result<Reply> {
    let indexes = registry::snapshot();

    let mut vectors = 0usize;
    let mut addr_set_bytes = 0usize;
    let mut held_bytes = 0usize;
    let mut used_bytes = 0usize;
    let mut per_index = String::new();
    for index in &indexes {
        let (count, addrs, held, used) = (
            index.ann.len(),
            index.ann.addr_set_bytes(),
            index.ann.held_bytes(),
            index.ann.used_bytes(),
        );
        vectors += count;
        addr_set_bytes += addrs;
        held_bytes += held;
        used_bytes += used;

        let name = &index.name;
        let _ = write!(
            per_index,
            "STAT {name}:vectors {count}\r\n\
             STAT {name}:addr_set_bytes {addrs}\r\n\
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
         STAT addr_set_bytes {addr_set_bytes}\r\n\
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
