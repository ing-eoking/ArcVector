use std::cell::RefCell;
use std::fmt::Write as _;

use super::access::{for_read, map_is_gone};
use super::coords::coord_vectors;
use crate::command::filter::Filter;
use crate::command::request::{Sim, SimKey};
use crate::error::{Error, Reply, Result};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{HeldElem, Store, StoreError};
use crate::handler::quant;
use crate::handler::registry::VectorIndex;
use crate::handler::usearch::Accept;

/// The attributes of a held element, if the `FILTER` held this key.
fn held_attr(
    held: &RefCell<Vec<(u64, HeldElem<'_>)>>,
    key: u64,
    layout: &Layout,
) -> Option<String> {
    let held = held.borrow();
    let (_, elem) = held.iter().find(|(k, _)| *k == key)?;
    let attr = layout.attr_of(elem.value()).ok()?;
    Some(String::from_utf8_lossy(attr).into_owned())
}

#[allow(clippy::too_many_arguments)]
fn similar(
    store: &Store,
    index: &VectorIndex,
    query: &[u8],
    k: usize,
    filter: Option<&Filter>,
    with_attr: bool,
    query_no: usize,
    out: &mut String,
) -> Result<()> {
    let layout = index.ann.layout;
    // After `for_read`, so this search's entry is already published and a release below cannot
    // reach anything registered since.
    let stamp = crate::handler::registry::now();

    // Elements the FILTER read, kept by the engine's own hold rather than copied. A hit that
    // is in here needs no second lookup to render, and what the reply carries is the same
    // bytes the filter judged — reading twice could have straddled a write.
    let held: RefCell<Vec<(u64, HeldElem<'_>)>> = RefCell::new(Vec::new());
    let by_attr = |key: u64, id: &str| -> bool {
        let Some(filter) = filter else {
            return true;
        };
        let Ok(elem) = store.hold_elem(&index.name, id) else {
            // Evicted or vanished mid-search: no longer a candidate.
            return false;
        };
        let Ok(attr) = layout.attr_of(elem.value()) else {
            return false;
        };
        if !filter.matches(attr) {
            return false;
        }
        held.borrow_mut().push((key, elem));
        true
    };

    // No FILTER means no callback: usearch runs its own path and a staged node that slips
    // into the results is dropped below, where `id_of` comes back empty.
    let accept: Option<Accept> = filter.map(|_| &by_attr as Accept);
    // `search` translates its own hits: the whole list under one hold on the mapping, which is
    // the hold a write takes across both of its registrations — so a hit is either fully
    // published or not named at all. Keys nothing names never come back.
    let named = index.ann.search(query, k, accept)?;

    // It may hand back more than `k`, having asked for extras to cover the slots writes in
    // flight take. Take the first `k` that survive rendering.
    let mut rendered = Vec::with_capacity(k);
    for (key, id, distance) in named {
        if rendered.len() == k {
            break;
        }
        // Only `WITHATTR` reads the element. Without it a hit costs no engine call at all,
        // and the reply cannot report an element that was deleted since the search — a
        // `FILTER` still catches that, because it could not have read it either.
        let attr = if !with_attr {
            None
        } else if let Some(attr) = held_attr(&held, key, &layout) {
            // The FILTER already holds this element; read it again from the hold.
            Some(attr)
        } else {
            let stored = match store.get_elem(&index.name, &id) {
                Ok(stored) => stored,
                // Every remaining hit would fail the same way, and the graph should not
                // outlive the Map it was built from.
                Err(StoreError::KeyGone) => {
                    map_is_gone(&index.name, stamp);
                    break;
                }
                Err(_) => continue,
            };
            Some(
                layout
                    .decode(&stored)
                    .map(|e| String::from_utf8_lossy(e.attr).into_owned())
                    .unwrap_or_default(),
            )
        };
        rendered.push((id.to_owned(), distance, attr));
    }

    // Counted after filtering, so the header agrees with the rows.
    let _ = writeln!(out, "QUERY {query_no} {}\r", rendered.len());
    for (id, distance, attr) in rendered {
        match attr {
            Some(attr) => {
                let _ = write!(out, "VALUE {id} {distance} {}\r\n{attr}\r\n", attr.len());
            }
            None => {
                let _ = write!(out, "VALUE {id} {distance}\r\n");
            }
        }
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
        with_attr,
    } = spec;
    let (k, dim, with_attr) = (*k, *dim, *with_attr);
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
        similar(
            store, &index, &quantized, k, filter, with_attr, query_no, &mut out,
        )?;
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
        with_attr,
    } = spec;
    let (k, with_attr, filter) = (*k, *with_attr, filter.as_ref());

    let index = for_read(store, name)?;
    let stamp = crate::handler::registry::now();

    let stored = match store.get_elem(name, key) {
        Ok(v) => v,
        Err(StoreError::ElemGone) => return Ok(Reply::NotFound),
        // The Map is gone, so the graph has nothing left to answer with.
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };
    let query = index.ann.layout.decode(&stored)?.vector.to_vec();

    let mut out = String::new();
    similar(store, &index, &query, k, filter, with_attr, 0, &mut out)?;
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
