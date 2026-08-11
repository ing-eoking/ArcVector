//! Command handlers.
//!
//! Every handler returns `Result<Reply>`; turning that into an ASCII response is
//! [`crate::protocol::tokens::Responder::reply`]'s job, so nothing here formats errors.

use std::cell::RefCell;
use std::fmt::Write as _;

use crate::error::{Error, Reply, Result};
use crate::filter::Filter;
use crate::protocol::request::{Add, Create, Sim, SimKey};
use crate::registry::{self, VectorIndex};
use crate::search::{AnnIndex, THREAD_SLOTS};
use crate::storage::element::{self, Layout};
use crate::storage::quantize;
use crate::storage::{Store, StoreError};

/// Parse whitespace-separated decimal coordinates.
///
/// Vectors travel as text, so `<veclen>` is the byte length of that text and
/// carries no information about how many coordinates it holds — which is exactly
/// why the dimension is supplied alongside it.
fn numbers(text: &[u8], what: &str) -> Result<Vec<f32>> {
    let text = std::str::from_utf8(text)
        .map_err(|_| Error::bad_request(format!("{what} is not valid UTF-8")))?;
    text.split_ascii_whitespace()
        .map(|token| {
            token.parse::<f32>().map_err(|_| {
                Error::bad_request(format!("{what} coordinate '{token}' is not a number"))
            })
        })
        .collect()
}

/// Split `text` into whole `dim`-dimension vectors.
fn coord_vectors(text: &[u8], dim: usize, what: &str) -> Result<Vec<Vec<f32>>> {
    if dim == 0 {
        return Err(Error::bad_request("dimension must be at least 1"));
    }
    let all = numbers(text, what)?;
    if all.is_empty() || all.len() % dim != 0 {
        return Err(Error::bad_request(format!(
            "{} coordinates is not a whole number of {dim}-dimension vectors",
            all.len()
        )));
    }
    if !all.iter().all(|x| x.is_finite()) {
        return Err(Error::bad_request(format!(
            "{what} contains NaN or infinity"
        )));
    }
    Ok(all.chunks(dim).map(<[f32]>::to_vec).collect())
}

/// Parse exactly one `dim`-dimension vector.
fn coords(text: &[u8], dim: usize, what: &str) -> Result<Vec<f32>> {
    let mut all = coord_vectors(text, dim, what)?;
    if all.len() != 1 {
        return Err(Error::bad_request(format!(
            "expected {dim} coordinates, got {}",
            all.len() * dim
        )));
    }
    Ok(all.remove(0))
}

// ---------------------------------------------------------------------------
// vcreate
// ---------------------------------------------------------------------------

pub fn vcreate(store: &Store, spec: &Create) -> Result<Reply> {
    let Create {
        index: name,
        dim,
        metric,
        quant,
        ..
    } = *spec;

    // The Map element size limit is the index's real dimension ceiling. The engine
    // does not enforce it on the API path and an oversized element has been
    // observed to abort the daemon, so it is checked here.
    let limit = store.max_element_bytes() as usize;
    let layout = Layout::new(dim, quant);
    if layout.element_len() > limit {
        return Err(Error::bad_request(format!(
            "element would be {} bytes, over max_element_bytes {limit} \
             (max dimension is {} for quant {quant})",
            layout.element_len(),
            Layout::max_dim_for(quant, limit),
        )));
    }

    if registry::contains(name) {
        return Ok(Reply::Exists);
    }

    let ann = AnnIndex::new(
        layout,
        metric,
        spec.connectivity,
        spec.expansion_add,
        spec.expansion_search,
        THREAD_SLOTS,
    )?;

    if store.create_map(name, spec.maxcount, spec.exptime).is_err() {
        // The engine may still hold an orphan Map from before a restart. Clear it
        // and retry once so module and engine resynchronize.
        store.drop_map(name)?;
        store.create_map(name, spec.maxcount, spec.exptime)?;
    }

    let maxcount = spec.maxcount.unwrap_or_else(|| store.max_map_size());
    // Freshly created, so there is nothing in Map to rebuild from.
    let created = registry::insert(VectorIndex::new(name.to_owned(), ann, maxcount, true));
    Ok(if created {
        Reply::Created
    } else {
        Reply::Exists
    })
}

// ---------------------------------------------------------------------------
// vadd
// ---------------------------------------------------------------------------

/// `vadd <index> <id> <veclen> <dim> [ATTR <attrlen> <attr JSON>]`
///
/// Both the byte count and the dimension are supplied, so a client that
/// miscounts is told which of the two disagrees instead of having its bytes
/// silently reinterpreted.
pub fn vadd(store: &Store, spec: &Add, body: &[u8]) -> Result<Reply> {
    let Add {
        index: name,
        id,
        dim,
        attr,
    } = spec;
    let attr = attr.as_slice();

    // veclen only sized the body; whether the text really holds `dim`
    // coordinates is decided here.
    let vector = coords(body, *dim, "vector")?;

    let index = registry::require(name)?;
    if *dim != index.ann.layout.dim {
        return Err(Error::bad_request(format!(
            "index {name} has dimension {}, got {dim}",
            index.ann.layout.dim
        )));
    }
    index.ensure_built(store)?;
    let layout = index.ann.layout;

    // ATTR must be a JSON object: it is stored as one and queried by field.
    if !attr.is_empty() {
        match serde_json::from_slice::<serde_json::Value>(attr) {
            Ok(serde_json::Value::Object(_)) => {}
            Ok(_) => return Err(Error::bad_request("ATTR must be a JSON object")),
            Err(e) => return Err(Error::bad_request(format!("ATTR is not valid JSON: {e}"))),
        }
    }

    // max_element_bytes can be lowered at runtime, so an index that fit when it
    // was created may not fit now.
    let limit = store.max_element_bytes() as usize;
    if layout.element_len() > limit {
        return Err(Error::bad_request(format!(
            "element is {} bytes ({} header+ATTR + {} vector), over max_element_bytes {limit}",
            layout.element_len(),
            Layout::VECTOR_OFFSET,
            layout.vector_bytes(),
        )));
    }

    if index.ann.key_of(id).is_none() && index.ann.len() >= index.maxcount as usize {
        return Ok(Reply::Overflowed);
    }

    let quantized = quantize::encode(&vector, layout.quant);
    let value = layout.encode(&quantized, attr)?;

    // Map first: it is the source of truth. If the usearch insert below fails,
    // the vector is merely invisible to search until the next rebuild.
    match store.put_elem(name, id, &value) {
        Ok(()) => {}
        Err(StoreError::Overflow) => return Ok(Reply::Overflowed),
        Err(StoreError::KeyGone) => {
            registry::remove(name);
            return Err(Error::IndexEvicted);
        }
        Err(e) => return Err(e.into()),
    }

    index.ann.add(id, &quantized)?;
    Ok(Reply::Stored)
}

// ---------------------------------------------------------------------------
// vsim
// ---------------------------------------------------------------------------

/// One similarity search, appended to `out` as a `QUERY` group.
///
/// Shared by both `VSIM` forms: the only difference between them is where the
/// query coordinates come from.
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

    // Reused across predicate calls so the hot path allocates nothing after the
    // first visited node.
    let scratch = RefCell::new(Vec::with_capacity(element::ATTR_BYTES));
    let accept = |key: u64| -> bool {
        let Some(filter) = filter else {
            return true;
        };
        let Some(id) = index.ann.id_of(key) else {
            return false;
        };
        let mut slot = scratch.borrow_mut();
        match store.read_attr_slot(&index.name, id, &layout, &mut slot) {
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
        let Ok(stored) = store.get_elem(&index.name, id) else {
            continue;
        };
        let attr = layout
            .decode(&stored)
            .map(|e| String::from_utf8_lossy(e.attr).into_owned())
            .unwrap_or_default();
        rendered.push((id.to_owned(), distance, attr));
    }

    // The count goes on the group header, so it has to be known before the rows
    // are written — hits that vanished mid-search are already excluded.
    let _ = writeln!(out, "QUERY {query_no} {}\r", rendered.len());
    for (id, distance, attr) in rendered {
        let _ = write!(out, "VALUE {id} {distance} {}\r\n{attr}\r\n", attr.len());
    }
    Ok(())
}

/// `VSIM VECTOR <index> <num> <bytes> <dim> [FILTER <n> <term>...]`
///
/// The body holds `bytes` of little-endian `f32`, which is `bytes / (dim * 4)`
/// query vectors searched in one round trip. Each contributes one `QUERY` group
/// of up to `num` neighbours.
pub fn vsim_vector(store: &Store, spec: &Sim, body: &[u8]) -> Result<Reply> {
    let Sim {
        index: name,
        k,
        dim,
        filter,
    } = spec;
    let (k, dim) = (*k, *dim);
    let filter = filter.as_ref();

    let index = registry::require(name)?;
    let layout = index.ann.layout;
    if dim != layout.dim {
        return Err(Error::bad_request(format!(
            "index {name} has dimension {}, got {dim}",
            layout.dim
        )));
    }
    index.ensure_built(store)?;

    let mut out = String::new();
    for (query_no, query) in coord_vectors(body, dim, "query")?.iter().enumerate() {
        let quantized = quantize::encode(query, layout.quant);
        similar(store, &index, &quantized, k, filter, query_no, &mut out)?;
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

/// `VSIM KEY <index> <num> <key> [FILTER <n> <term>...]`
///
/// Searches using a vector already in the index, so no body is transferred. The
/// stored bytes are handed to usearch as they are — already quantized, so there
/// is no re-encoding step.
pub fn vsim_key(store: &Store, spec: &SimKey) -> Result<Reply> {
    let SimKey {
        index: name,
        key,
        k,
        filter,
    } = spec;
    let (k, filter) = (*k, filter.as_ref());

    let index = registry::require(name)?;
    index.ensure_built(store)?;

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

// ---------------------------------------------------------------------------
// vget / vdel / vdrop / vlist
// ---------------------------------------------------------------------------

pub fn vget(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = registry::require(name)?;

    match store.get_elem(name, id) {
        Ok(stored) => {
            let element = index.ann.layout.decode(&stored)?;
            let json = String::from_utf8_lossy(element.attr);
            Ok(Reply::Body(format!(
                "VALUE {id} {}\r\n{json}\r\nEND\r\n",
                json.len()
            )))
        }
        Err(StoreError::ElemGone | StoreError::KeyGone) => Ok(Reply::NotFound),
        Err(e) => Err(e.into()),
    }
}

pub fn vdel(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = registry::require(name)?;

    // Map first, then the cache. A failure to drop the graph node only leaves a
    // ghost, which the predicate filters out and a rebuild clears.
    let existed = match store.delete_elem(name, id) {
        Ok(()) => true,
        Err(StoreError::ElemGone | StoreError::KeyGone) => false,
        Err(e) => return Err(e.into()),
    };
    index.ann.remove(id)?;

    Ok(if existed {
        Reply::Deleted
    } else {
        Reply::NotFound
    })
}

pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    let known = registry::remove(name);
    // Try the engine delete regardless, so an orphan Map left by a restart can
    // still be cleaned up through vdrop.
    let dropped = store.drop_map(name).is_ok();

    Ok(if known || dropped {
        Reply::Dropped
    } else {
        Reply::NotFound
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_are_parsed_from_text() {
        // The reported line: "0.1 0.2" is 7 bytes of text holding 2 coordinates.
        assert_eq!(coords(b"0.1 0.2", 2, "vector").unwrap(), vec![0.1, 0.2]);
        assert_eq!(b"0.1 0.2".len(), 7);

        // Any run of whitespace separates coordinates.
        assert_eq!(
            coords(b"1  -2.5\t3e2", 3, "vector").unwrap(),
            vec![1.0, -2.5, 300.0]
        );
    }

    #[test]
    fn a_coordinate_count_that_disagrees_with_the_dimension_is_named() {
        let msg = coords(b"0.1 0.2 0.3", 2, "vector").unwrap_err().to_string();
        assert!(msg.contains("whole number of 2-dimension"), "{msg}");

        let msg = coords(b"0.1", 2, "vector").unwrap_err().to_string();
        assert!(msg.contains("whole number of 2-dimension"), "{msg}");

        assert!(coords(b"", 2, "vector").is_err());
    }

    #[test]
    fn a_non_numeric_coordinate_is_quoted_back() {
        let msg = coords(b"0.1 abc", 2, "vector").unwrap_err().to_string();
        assert!(msg.contains("'abc' is not a number"), "{msg}");
    }

    #[test]
    fn non_finite_coordinates_are_rejected() {
        for bad in ["NaN", "inf", "-inf"] {
            let text = format!("0.1 {bad}");
            let msg = coords(text.as_bytes(), 2, "query").unwrap_err().to_string();
            assert!(msg.contains("NaN or infinity"), "{bad}: {msg}");
        }
    }

    #[test]
    fn a_batch_splits_into_whole_vectors() {
        let batch = coord_vectors(b"1 2 3 4 5 6", 3, "query").unwrap();
        assert_eq!(batch, vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]]);

        // A partial trailing vector is refused rather than truncated.
        let msg = coord_vectors(b"1 2 3 4", 3, "query")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("whole number of 3-dimension"), "{msg}");
    }

    #[test]
    fn a_zero_dimension_does_not_divide_by_zero() {
        assert!(coord_vectors(b"1 2", 0, "query").is_err());
    }
}
