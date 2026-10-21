use std::cell::RefCell;
use std::fmt::Write as _;

use super::coords::vector as decode_vector;
use super::vector::{check_index, check_vkey, meta_of_stored};
use crate::cmd::access::{for_read, map_is_gone};
use crate::engine::{Store, StoreError};
use crate::error::{Error, Reply, Result};
use crate::index::Accept;
use crate::index::quant;
use crate::index::registry::{self, VectorIndex};
use crate::protocol::filter::Filter;
use crate::protocol::request::{Parts, SearchKey, SearchVector};

/// Verdict returned by the filter accept callback for a single node.
enum Verdict {
    /// Node is accepted. If `withmeta`, carries the pre-parsed metadata JSON.
    Take(Option<String>),
    /// Filter rejected the node.
    Drop,
    /// Filter matched, but metadata could not be converted to JSON (corrupt BSON).
    Corrupt,
}

/// Retrieves pre-parsed metadata for a key. `judged` must be sorted by key.
fn judged_meta(judged: &mut [(u64, String)], key: u64) -> Option<String> {
    let at = judged.binary_search_by_key(&key, |(k, _)| *k).ok()?;
    Some(std::mem::take(&mut judged[at].1))
}

fn filter_of(has_filter: bool, parts: &Parts) -> Result<Option<Filter>> {
    if !has_filter {
        return Ok(None);
    }
    let text = parts
        .filter
        .ok_or_else(|| Error::bad_request("bad data chunk"))?;
    let text =
        std::str::from_utf8(text).map_err(|_| Error::bad_request("filter is not valid UTF-8"))?;
    Ok(Some(Filter::parse(text)?))
}

type Row = (String, f32, Option<String>);

fn similar(
    store: &Store,
    index: &VectorIndex,
    query: &[u8],
    k: usize,
    filter: Option<&Filter>,
    with_meta: bool,
) -> Result<Vec<Row>> {
    let layout = index.ann.layout;

    let judged: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
    // Errors cannot be returned directly inside the traversal callback.
    // Record corruption and return `CorruptElement` afterwards.
    let corrupt = std::cell::Cell::new(false);
    let accepted = |key: u64| -> bool {
        // Accessing engine memory is protected by the guard held during search.
        let Some(filter) = filter else {
            // If no filter is specified, presence in the graph implies acceptance.
            return true;
        };
        let meta = match store.with_elem_at(key, |_field, value| match layout.split(value) {
            Ok((meta, _)) if filter.matches(meta) => {
                if !with_meta {
                    return Verdict::Take(None);
                }
                match crate::bson::to_json(meta) {
                    Ok(json) => Verdict::Take(Some(json)),
                    Err(_) => Verdict::Corrupt,
                }
            }
            _ => Verdict::Drop,
        }) {
            Some(Verdict::Take(meta)) => meta,
            Some(Verdict::Corrupt) => {
                corrupt.set(true);
                return false;
            }
            Some(Verdict::Drop) | None => return false,
        };

        let Some(meta) = meta else {
            return true;
        };

        let mut judged = judged.borrow_mut();

        if judged.try_reserve(1).is_err() {
            return false;
        }
        judged.push((key, meta));
        true
    };

    // Install the accept callback even without a filter to guard against halted indices
    // and ensure usearch continues traversal when candidates are rejected.
    let accept: Option<Accept> = Some(&accepted as Accept);

    let named = index.ann.search(query, k, accept)?;
    if corrupt.get() {
        eprintln!(
            "ArcVector: an element of index '{}' matched the filter but its metadata is unreadable",
            index.name
        );
        return Err(StoreError::CorruptElement.into());
    }

    let mut judged = judged.take();
    judged.sort_unstable_by_key(|(k, _)| *k);

    let mut rendered = crate::fallible::vec(named.len().min(k))?;
    for (key, id, distance) in named {
        if rendered.len() == k {
            break;
        }
        let meta = if !with_meta {
            None
        } else if let Some(meta) = judged_meta(&mut judged, key) {
            Some(meta)
        } else {
            let stored = match store
                .get_elem(&index.name, &id)
                .and_then(|body| meta_of_stored(layout, &body))
            {
                Ok(stored) => stored,
                Err(StoreError::ElemGone) => continue,
                Err(StoreError::CorruptElement) => {
                    // Do not remove from graph here: the node will be removed on the unlink event callback.
                    eprintln!(
                        "ArcVector: element '{id}' of index '{}' is unreadable",
                        index.name
                    );
                    return Err(StoreError::CorruptElement.into());
                }
                Err(_) => continue,
            };
            Some(stored)
        };
        rendered.push((id.to_string(), distance, meta));
    }
    Ok(rendered)
}

/// Formats response: `VECTORS <count>`, followed by rows, terminated with `END`.
///
/// Each row is `<vkey> <score>`, or `<vkey> <score> <metabytes>\r\n<json>` if `withmeta`.
fn render(rows: &[Row]) -> Result<String> {
    let mut out = String::new();
    // Pre-calculate the required buffer capacity to avoid reallocations or OOM aborts.
    const ROW_BYTES: usize = 96;
    const HEADER_AND_END: usize = 48;
    let body_bytes = rows
        .iter()
        .map(|(id, _, meta)| id.len() + meta.as_ref().map_or(0, String::len) + ROW_BYTES)
        .try_fold(HEADER_AND_END, |sum, n| sum.checked_add(n))
        .ok_or_else(|| Error::Index("answer does not fit in memory".into()))?;
    crate::fallible::reserve_str(&mut out, body_bytes)?;

    let _ = write!(out, "VECTORS {}\r\n", rows.len());
    for (id, score, meta) in rows {
        match meta {
            Some(json) => {
                let _ = write!(out, "{id} {score} {}\r\n{json}\r\n", json.len());
            }
            None => {
                let _ = write!(out, "{id} {score}\r\n");
            }
        }
    }
    out.push_str("END\r\n");
    Ok(out)
}

/// Pool thread (any lane): searches by vector coordinates.
pub fn search_vector(store: &Store, spec: &SearchVector, body: &[u8]) -> Result<Reply> {
    let SearchVector {
        index: name,
        k,
        format,
        with_meta,
        has_filter,
        segments,
    } = spec;
    check_index(name)?;
    let parts = segments.split(body)?;
    let filter = filter_of(*has_filter, &parts)?;

    let index = for_read(store, name)?;
    let layout = index.ann.layout;
    let query = parts
        .vector
        .ok_or_else(|| Error::bad_request("bad data chunk"))?;
    let query = decode_vector(query, layout.dim, *format)?;
    let query = quant::encode(&query, layout.quant)?;

    let rows = similar(store, &index, &query, *k, filter.as_ref(), *with_meta)?;
    Ok(Reply::Body(render(&rows)?))
}

/// Pool thread (any lane): searches by existing vector key.
pub fn search_key(store: &Store, spec: &SearchKey, body: &[u8]) -> Result<Reply> {
    let SearchKey {
        index: name,
        vkey,
        k,
        with_meta,
        has_filter,
        segments,
    } = spec;
    check_index(name)?;
    check_vkey(vkey)?;
    let parts = segments.split(body)?;
    let filter = filter_of(*has_filter, &parts)?;

    let index = for_read(store, name)?;
    let stamp = registry::now();
    let addr = match store.hold_elem(name, vkey) {
        Ok(addr) => addr,
        // Target query vector does not exist.
        Err(StoreError::ElemGone) => return Ok(Reply::NotFoundVector),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };

    // Read query vector directly from the element even if not yet indexed in graph.
    let layout = index.ann.layout;
    let query = match index.ann.vector_of(addr) {
        Ok(Some(query)) => Ok(query),
        Ok(None) => store
            .with_elem_at(addr, |_field, value| {
                layout.split(value).ok().map(|(_, v)| v.to_vec())
            })
            .flatten()
            .ok_or_else(|| {
                eprintln!("ArcVector: element '{vkey}' of index '{name}' is unreadable");
                Error::from(StoreError::CorruptElement)
            }),
        Err(e) => Err(e),
    };
    // Release temporary element hold; the graph maintains its own reference.
    store.release_elems(&[addr]);
    let query = query?;

    let rows = similar(store, &index, &query, *k, filter.as_ref(), *with_meta)?;
    Ok(Reply::Body(render(&rows)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, score: f32, meta: Option<&str>) -> (String, f32, Option<String>) {
        (id.to_owned(), score, meta.map(str::to_owned))
    }

    #[test]
    fn renders_vectors_without_meta() {
        let rows = [row("a", 0.5, None), row("b", 1.0, None)];
        assert_eq!(
            render(&rows).unwrap(),
            "VECTORS 2\r\na 0.5\r\nb 1\r\nEND\r\n"
        );
        assert_eq!(render(&[]).unwrap(), "VECTORS 0\r\nEND\r\n");
    }

    #[test]
    fn renders_metabytes_as_the_json_byte_length() {
        let rows = [
            row("a", 0.25, Some(r#"{"t":"한"}"#)),
            row("b", 0.5, Some("{}")),
        ];
        assert_eq!(
            render(&rows).unwrap(),
            "VECTORS 2\r\na 0.25 11\r\n{\"t\":\"한\"}\r\nb 0.5 2\r\n{}\r\nEND\r\n"
        );
    }
}
