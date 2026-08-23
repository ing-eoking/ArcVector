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

    let stamp = crate::handler::registry::now();

    let judged: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
    let by_attr = |key: u64| -> bool {
        let Some(filter) = filter else {
            return true;
        };

        let Some(passed) = store.with_attr_at(key, layout, |attr| {
            filter
                .matches(attr)
                .then(|| String::from_utf8_lossy(attr).into_owned())
        }) else {
            return false;
        };
        let Some(attr) = passed else {
            return false;
        };
        let mut judged = judged.borrow_mut();

        if judged.try_reserve(1).is_err() {
            return false;
        }
        judged.push((key, attr));
        true
    };

    let accept: Option<Accept> = filter.map(|_| &by_attr as Accept);

    let named = index.ann.search(query, k, accept)?;

    let mut rendered = Vec::with_capacity(k);
    for (key, id, distance) in named {
        if rendered.len() == k {
            break;
        }

        let attr = if !with_attr {
            None
        } else if let Some(attr) = judged_attr(&judged, key) {
            Some(attr)
        } else {
            let stored = match store.get_attr(&index.name, &id, layout) {
                Ok(stored) => stored,

                Err(StoreError::KeyGone) => {
                    map_is_gone(&index.name, stamp);
                    break;
                }

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

    let held = match store.hold_addr(name, key) {
        Ok(held) => held,
        Err(StoreError::ElemGone) => return Ok(Reply::NotFound),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };

    let Some(query) = index.ann.vector_of(held.addr())? else {
        return Err(Error::Unreadable);
    };

    let mut out = String::new();
    similar(store, &index, &query, k, filter, with_attr, 0, &mut out)?;
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
