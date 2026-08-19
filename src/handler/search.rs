use std::cell::RefCell;
use std::fmt::Write as _;

use super::access::for_read;
use super::coords::coord_vectors;
use crate::command::filter::Filter;
use crate::command::request::{Sim, SimKey};
use crate::error::{Error, Reply, Result};
use crate::handler::arcus::element;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry::VectorIndex;

fn similar(
    store: &Store,
    index: &VectorIndex,
    query: &[u8],
    k: usize,
    filter: Option<&Filter>,
    query_no: usize,
    out: &mut String,
) -> Result<()> {
    let layout = index.ann.layout;

    // Reused across predicate calls: the hot path allocates nothing.
    let scratch = RefCell::new(Vec::with_capacity(element::ATTR_BYTES));
    let accept = |key: u64| -> bool {
        let Some(filter) = filter else {
            return true;
        };
        let Some(id) = index.ann.id_of(key) else {
            return false;
        };
        let mut slot = scratch.borrow_mut();
        match store.read_attr_slot(&index.name, &id, &layout, &mut slot) {
            Ok(()) => filter.matches(&slot),
            // Evicted or vanished mid-search: no longer a candidate.
            Err(_) => false,
        }
    };

    let hits = index.ann.search(query, k, accept)?;

    let mut rendered = Vec::with_capacity(hits.len());
    for (key, distance) in hits {
        let Some(id) = index.ann.id_of(key) else {
            continue;
        };
        let Ok(stored) = store.get_elem(&index.name, &id) else {
            continue;
        };
        let attr = layout
            .decode(&stored)
            .map(|e| String::from_utf8_lossy(e.attr).into_owned())
            .unwrap_or_default();
        rendered.push((id.to_owned(), distance, attr));
    }

    // Counted after filtering, so the header agrees with the rows.
    let _ = writeln!(out, "QUERY {query_no} {}\r", rendered.len());
    for (id, distance, attr) in rendered {
        let _ = write!(out, "VALUE {id} {distance} {}\r\n{attr}\r\n", attr.len());
    }
    Ok(())
}

/// `VSIM VECTOR <index> <num> <bytes> <dim> [FILTER <n> <term>...]`
pub fn vsim_vector(store: &Store, spec: &Sim, body: &[u8]) -> Result<Reply> {
    let Sim {
        index: name,
        k,
        dim,
        filter,
    } = spec;
    let (k, dim) = (*k, *dim);
    let filter = filter.as_ref();

    let index = for_read(store, name)?;
    let layout = index.ann.layout;
    if dim != layout.dim {
        return Err(Error::bad_request(format!(
            "index {name} has dimension {}, got {dim}",
            layout.dim
        )));
    }

    let mut out = String::new();
    for (query_no, query) in coord_vectors(body, dim, "query")?.iter().enumerate() {
        let quantized = quant::encode(query, layout.quant);
        similar(store, &index, &quantized, k, filter, query_no, &mut out)?;
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

/// `VSIM KEY <index> <num> <key> [FILTER <n> <term>...]`
pub fn vsim_key(store: &Store, spec: &SimKey) -> Result<Reply> {
    let SimKey {
        index: name,
        key,
        k,
        filter,
    } = spec;
    let (k, filter) = (*k, filter.as_ref());

    let index = for_read(store, name)?;

    let stored = match store.get_elem(name, key) {
        Ok(v) => v,
        Err(StoreError::ElemGone | StoreError::KeyGone) => return Ok(Reply::NotFound),
        Err(e) => return Err(e.into()),
    };
    let query = index.ann.layout.decode(&stored)?.vector.to_vec();

    let mut out = String::new();
    similar(store, &index, &query, k, filter, 0, &mut out)?;
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
