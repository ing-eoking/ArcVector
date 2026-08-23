use std::cell::RefCell;
use std::fmt::Write as _;

use super::coords::coord_vectors;
use crate::command::filter::Filter;
use crate::command::request::{Sim, SimKey};
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, map_is_gone};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry::VectorIndex;
use crate::handler::usearch::Accept;

/// What the `FILTER` read for this key, if it read one.
fn judged_attr(judged: &RefCell<Vec<(u64, String)>>, key: u64) -> Option<String> {
    let judged = judged.borrow();
    judged
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, a)| a.clone())
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

    // What the FILTER read, for the hits that pass it. Copied rather than held: the ATTR is at
    // most `ATTR_BYTES`, so keeping the bytes costs less than keeping the element they came
    // from — no engine call inside the graph's read lock, and no refcount standing for the
    // length of the search. A hit that is in here renders without a second read, and what the
    // reply carries is the same bytes the filter judged.
    //
    // Local to this query, and `RefCell` because usearch's predicate is `Fn`.
    let judged: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
    let by_attr = |key: u64| -> bool {
        let Some(filter) = filter else {
            return true;
        };
        // The key is the element's address, so the attributes are one dereference away — no
        // hash lookup, no refcount pair, on the path that runs once per visited node. Safe
        // because the graph checked the address and holds its read lock for this call.
        let Some(passed) = store.with_attr_at(key, layout, |attr| {
            filter
                .matches(attr)
                .then(|| String::from_utf8_lossy(attr).into_owned())
        }) else {
            // Unreadable mid-search: no longer a candidate.
            return false;
        };
        let Some(attr) = passed else {
            return false;
        };
        let mut judged = judged.borrow_mut();
        // A candidate that cannot be recorded is dropped rather than answered without the
        // bytes it was judged on — and the daemon does not fall over a search it declined.
        if judged.try_reserve(1).is_err() {
            return false;
        }
        judged.push((key, attr));
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
        } else if let Some(attr) = judged_attr(&judged, key) {
            // The FILTER already read this element; answer with the bytes it judged.
            Some(attr)
        } else {
            // The ATTR only, as `vgetattr` answers it — the rest of the stored value is a
            // length header and, where this build keeps one, the vector.
            let stored = match store.get_attr(&index.name, &id, layout) {
                Ok(stored) => stored,
                // Every remaining hit would fail the same way, and the graph should not
                // outlive the Map it was built from.
                Err(StoreError::KeyGone) => {
                    map_is_gone(&index.name, stamp);
                    break;
                }
                // Unreadable description: stop offering the id and fail the command rather
                // than quietly shortening the answer. The Map is left alone.
                Err(StoreError::CorruptElement) => {
                    index.ann.forget_unreadable(key);
                    eprintln!(
                        "ArcVector: element '{id}' of index '{}' is unreadable; dropped from the graph",
                        index.name
                    );
                    return Err(StoreError::CorruptElement.into());
                }
                Err(_) => continue,
            };
            Some(String::from_utf8_lossy(&stored).into_owned())
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

    // The graph is keyed by element address, so the id becomes one here before the graph is
    // asked anything. The Map is the authority on whether the id exists at all.
    let held = match store.hold_addr(name, key) {
        Ok(held) => held,
        Err(StoreError::ElemGone) => return Ok(Reply::NotFound),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };

    // The query comes out of the graph, not the Map. usearch holds the vector in the index's
    // own quantization — the same bytes a stored element carries — so this reads what the Map
    // would have said, and it is the only copy a build without recovery keeps.
    let Some(query) = index.ann.vector_of(held.addr())? else {
        // The element is there and its node is not: a write that has not published yet, or a
        // rebuild that has not reached it. Neither is "no such vector".
        return Err(Error::Unreadable);
    };

    let mut out = String::new();
    similar(store, &index, &query, k, filter, with_attr, 0, &mut out)?;
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
