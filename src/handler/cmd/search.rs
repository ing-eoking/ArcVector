use std::cell::RefCell;
use std::fmt::Write as _;

use super::coords::coord_vectors;
use crate::command::filter::Filter;
use crate::command::request::{Sim, SimKey};
use crate::error::{Error, Reply, Result};
use crate::handler::access::for_read;
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

/// The line that opens one query's hits.
///
/// A graph still being rebuilt from its Map answers from the part of itself that
/// exists, which is not the nearest neighbours — it is the nearest of what has
/// been added so far. `PARTIAL_QUERY` names that, and carries the counts so the
/// caller can judge how far off the answer may be.
fn query_header(query_no: usize, hits: usize, rebuild: Option<crate::error::Rebuild>) -> String {
    match rebuild {
        None => format!("QUERY {query_no} {hits}\r\n"),
        Some(p) => format!("PARTIAL_QUERY {query_no} {hits} {}/{}\r\n", p.done, p.total),
    }
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
    let k = k.min((index.maxcount as usize).max(1));

    let judged: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
    let by_attr = |key: u64| -> bool {
        let Some(filter) = filter else {
            return true;
        };

        let Some(passed) = store.with_item_at(key, |_key, value| {
            layout.attr_of(value).ok().and_then(|attr| {
                filter
                    .matches(attr)
                    .then(|| String::from_utf8_lossy(attr).into_owned())
            })
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

    let mut rendered = Vec::with_capacity(named.len().min(k));
    for (key, id, distance) in named {
        if rendered.len() == k {
            break;
        }

        let attr = if !with_attr {
            None
        } else if let Some(attr) = judged_attr(&judged, key) {
            Some(attr)
        } else {
            let stored = match store
                .get_kv(&crate::trigger::key::vector_key(&index.name, &id))
                .and_then(|body| {
                    layout
                        .attr_of(&body)
                        .map(<[u8]>::to_vec)
                        .map_err(|_| StoreError::CorruptElement)
                }) {
                Ok(stored) => stored,

                // This vector's key, not the index's: a delete that landed
                // between `resolve` and here. One hit goes, the query stands.
                Err(StoreError::KeyGone) => continue,

                Err(StoreError::CorruptElement) => {
                    // 그래프에서 빼지 않는다. 이 아이템이 실제로 사라질 때
                    // unlink 이벤트가 오고, 그때 콜백이 노드를 뺀다.
                    eprintln!(
                        "ArcVector: element '{id}' of index '{}' is unreadable",
                        index.name
                    );
                    return Err(StoreError::CorruptElement.into());
                }
                Err(_) => continue,
            };
            Some(String::from_utf8_lossy(&stored).into_owned())
        };
        rendered.push((
            id.to_owned(),
            index.ann.metric.score(distance, layout.dim),
            attr,
        ));
    }

    out.push_str(&query_header(
        query_no,
        rendered.len(),
        index.rebuilding_progress(),
    ));
    for (id, score, attr) in rendered {
        match attr {
            Some(attr) => {
                let _ = write!(out, "VALUE {id} {score} {}\r\n{attr}\r\n", attr.len());
            }
            None => {
                let _ = write!(out, "VALUE {id} {score}\r\n");
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
    let addr = match store.hold_kv(&crate::trigger::key::vector_key(name, key)) {
        Ok(addr) => addr,
        // The query vector's own key. Gone means gone: this is a NOT_FOUND for
        // the query, never a verdict on the index.
        Err(StoreError::KeyGone) => return Ok(Reply::NotFound),
        Err(e) => return Err(e.into()),
    };

    // The graph may not hold this key yet, but the item always carries the
    // vector: read it from there so `vsim KEY` works either way.
    let layout = index.ann.layout;
    let query = match index.ann.vector_of(addr)? {
        Some(query) => query,
        None => store
            .with_item_at(addr, |_key, value| {
                layout.vector_of(value).map(<[u8]>::to_vec)
            })
            .flatten()
            .ok_or(Error::Unreadable)?,
    };
    // Only taken to read the query out; the graph keeps its own.
    store.release_items(&[addr]);

    let mut out = String::new();
    similar(store, &index, &query, k, filter, with_attr, 0, &mut out)?;
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Rebuild;

    #[test]
    fn a_whole_index_writes_the_plain_header() {
        assert_eq!(query_header(0, 5, None), "QUERY 0 5\r\n");
    }

    #[test]
    fn a_half_built_index_says_what_it_answered_from() {
        let half = Rebuild {
            done: 693_000,
            total: 999_000,
        };
        assert_eq!(
            query_header(0, 5, Some(half)),
            "PARTIAL_QUERY 0 5 693000/999000\r\n"
        );
    }

    #[test]
    fn a_partial_query_that_found_nothing_still_reports_progress() {
        let just_begun = Rebuild {
            done: 0,
            total: 999_000,
        };
        assert_eq!(
            query_header(2, 0, Some(just_begun)),
            "PARTIAL_QUERY 2 0 0/999000\r\n"
        );
    }
}
