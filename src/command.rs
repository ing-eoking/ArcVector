//! Command handlers.
//!
//! Every handler returns `Result<Reply>`; turning that into an ASCII response is
//! [`crate::protocol::Responder::reply`]'s job, so nothing here formats errors.

use std::cell::RefCell;
use std::fmt::Write as _;

use crate::codec::{self, Layout};
use crate::error::{Error, Reply, Result};
use crate::filter::Filter;
use crate::index::{self, AnnIndex, Metric};
use crate::protocol::Tokens;
use crate::quant::{self, Quant};
use crate::registry::{self, VectorIndex};
use crate::store::{Store, StoreError};

fn to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Decode and validate a client-supplied query or vector.
fn coords(bytes: &[u8], expected_dim: usize, what: &str) -> Result<Vec<f32>> {
    let v = to_f32(bytes);
    if v.len() != expected_dim {
        return Err(Error::bad_request(format!(
            "expected {expected_dim} dimensions, got {}",
            v.len()
        )));
    }
    if !v.iter().all(|x| x.is_finite()) {
        return Err(Error::bad_request(format!(
            "{what} contains NaN or infinity"
        )));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// vcreate
// ---------------------------------------------------------------------------

/// Everything `vcreate` can be told, with the defaults it falls back to.
struct Options {
    metric: Metric,
    quant: Quant,
    filter_bytes: usize,
    threads: usize,
    connectivity: usize,
    expansion_add: usize,
    expansion_search: usize,
    maxcount: Option<u32>,
    exptime: Option<u32>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            metric: Metric::Cos,
            quant: Quant::F32,
            filter_bytes: codec::DEFAULT_FILTER_BYTES,
            threads: index::DEFAULT_THREADS,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            maxcount: None,
            exptime: None,
        }
    }
}

impl Options {
    fn parse(tokens: &Tokens, from: usize) -> Result<Options> {
        let mut opts = Options::default();
        for (key, raw) in tokens.options(from)? {
            let number = |kind: &str| -> Result<usize> {
                raw.parse::<usize>()
                    .map_err(|_| Error::bad_request(format!("invalid {kind} '{raw}'")))
            };
            match key.as_str() {
                "METRIC" => {
                    opts.metric = Metric::parse(raw)
                        .ok_or_else(|| Error::bad_request(format!("unknown metric '{raw}'")))?;
                }
                "QUANT" => {
                    opts.quant = Quant::parse(raw).ok_or_else(|| {
                        Error::bad_request(format!("unknown quantization '{raw}'"))
                    })?;
                }
                "FBYTES" => {
                    opts.filter_bytes = Layout::validate_filter_bytes(number("FBYTES")?)
                        .map_err(Error::bad_request)?;
                }
                "THREADS" => opts.threads = number("THREADS")?.clamp(1, 1024),
                "M" => opts.connectivity = number("M")?,
                "EFC" => opts.expansion_add = number("EFC")?,
                "EFS" => opts.expansion_search = number("EFS")?,
                // item_attr.maxcount is an i32, so anything past its range
                // would wrap to a negative limit.
                "MAXCOUNT" => {
                    let n = number("MAXCOUNT")?;
                    if n == 0 || n > i32::MAX as usize {
                        return Err(Error::bad_request(
                            "MAXCOUNT must be between 1 and 2147483647",
                        ));
                    }
                    opts.maxcount = Some(n as u32);
                }
                "EXPTIME" => {
                    let n = number("EXPTIME")?;
                    if n > u32::MAX as usize {
                        return Err(Error::bad_request("EXPTIME is out of range"));
                    }
                    opts.exptime = Some(n as u32);
                }
                other => return Err(Error::bad_request(format!("unknown option {other}"))),
            }
        }
        Ok(opts)
    }
}

pub fn vcreate(store: &Store, tokens: &Tokens) -> Result<Reply> {
    if tokens.len() < 3 {
        return Err(Error::bad_request("bad command line format"));
    }
    let name = tokens.text(1)?;
    let dim: usize = tokens.parse(2, "dimension")?;
    if dim == 0 || dim > u16::MAX as usize {
        return Err(Error::bad_request("dimension out of range (1..65535)"));
    }

    let opts = Options::parse(tokens, 3)?;
    opts.metric.check_quant(opts.quant)?;

    // The Map element size limit is the index's real dimension ceiling. The
    // engine does not enforce it on the API path and an oversized element has
    // been observed to abort the daemon, so it is checked here.
    let limit = store.max_element_bytes() as usize;
    let layout = Layout::new(dim, opts.quant, opts.filter_bytes);
    if layout.element_len() > limit {
        let max_dim = Layout::max_dim_for(opts.quant, opts.filter_bytes, limit);
        return Err(Error::bad_request(format!(
            "element would be {} bytes, over max_element_bytes {limit} \
             (max dimension is {max_dim} for quant {} with FBYTES {})",
            layout.element_len(),
            opts.quant,
            opts.filter_bytes,
        )));
    }

    if registry::contains(name) {
        return Ok(Reply::Exists);
    }

    let ann = AnnIndex::new(
        layout,
        opts.metric,
        opts.connectivity,
        opts.expansion_add,
        opts.expansion_search,
        opts.threads,
    )?;

    if store.create_map(name, opts.maxcount, opts.exptime).is_err() {
        // The engine may still hold an orphan Map from before a restart. Clear
        // it and retry once so module and engine resynchronize.
        store.drop_map(name)?;
        store.create_map(name, opts.maxcount, opts.exptime)?;
    }

    let maxcount = opts.maxcount.unwrap_or_else(|| store.max_map_size());
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

pub fn vadd(store: &Store, name: &str, id: &str, vec_bytes: usize, body: &[u8]) -> Result<Reply> {
    let index = registry::require(name)?;
    index.ensure_built(store)?;

    let layout = index.ann.layout;
    let vector = coords(&body[..vec_bytes], layout.dim, "vector")?;
    let json = &body[vec_bytes..];

    if !json.is_empty() && serde_json::from_slice::<serde_json::Value>(json).is_err() {
        return Err(Error::bad_request("filter payload is not valid JSON"));
    }

    if index.ann.key_of(id).is_none() && index.ann.len() >= index.maxcount as usize {
        return Ok(Reply::Overflowed);
    }

    let quantized = quant::encode(&vector, layout.quant);
    let value = layout.encode(&quantized, json)?;

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
// vsearch
// ---------------------------------------------------------------------------

pub fn vsearch(
    store: &Store,
    name: &str,
    k: usize,
    vec_bytes: usize,
    body: &[u8],
) -> Result<Reply> {
    let index = registry::require(name)?;
    index.ensure_built(store)?;

    let layout = index.ann.layout;
    let query = coords(&body[..vec_bytes], layout.dim, "query")?;

    let expression = &body[vec_bytes..];
    let filter = if expression.is_empty() {
        None
    } else {
        let text = std::str::from_utf8(expression)
            .map_err(|_| Error::bad_request("filter expression is not valid UTF-8"))?;
        Some(Filter::parse(text)?)
    };

    let quantized = quant::encode(&query, layout.quant);

    // Reused across predicate calls so the hot path allocates nothing after the
    // first visited node.
    let scratch = RefCell::new(Vec::with_capacity(layout.filter_bytes));
    let accept = |key: u64| -> bool {
        let Some(filter) = filter.as_ref() else {
            return true;
        };
        let Some(id) = index.ann.id_of(key) else {
            return false;
        };
        let mut slot = scratch.borrow_mut();
        match store.read_filter_slot(name, id, &layout, &mut slot) {
            Ok(()) => filter.matches(&slot),
            // Evicted or vanished mid-search: no longer a candidate.
            Err(_) => false,
        }
    };

    let hits = index.ann.search(&quantized, k, accept)?;

    let mut out = String::new();
    for (key, distance) in hits {
        let Some(id) = index.ann.id_of(key) else {
            continue;
        };
        let Ok(stored) = store.get_elem(name, id) else {
            continue;
        };
        let json = layout
            .decode(&stored)
            .map(|e| String::from_utf8_lossy(e.filter).into_owned())
            .unwrap_or_default();
        let _ = write!(out, "VALUE {id} {distance} {}\r\n{json}\r\n", json.len());
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

// ---------------------------------------------------------------------------
// vget / vdel / vdrop / vlist
// ---------------------------------------------------------------------------

pub fn vget(store: &Store, tokens: &Tokens) -> Result<Reply> {
    if tokens.len() != 3 {
        return Err(Error::bad_request("bad command line format"));
    }
    let name = tokens.text(1)?;
    let id = tokens.text(2)?;
    let index = registry::require(name)?;

    match store.get_elem(name, id) {
        Ok(stored) => {
            let element = index.ann.layout.decode(&stored)?;
            let json = String::from_utf8_lossy(element.filter);
            Ok(Reply::Body(format!(
                "VALUE {id} {}\r\n{json}\r\nEND\r\n",
                json.len()
            )))
        }
        Err(StoreError::ElemGone | StoreError::KeyGone) => Ok(Reply::NotFound),
        Err(e) => Err(e.into()),
    }
}

pub fn vdel(store: &Store, tokens: &Tokens) -> Result<Reply> {
    if tokens.len() != 3 {
        return Err(Error::bad_request("bad command line format"));
    }
    let name = tokens.text(1)?;
    let id = tokens.text(2)?;
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

pub fn vdrop(store: &Store, tokens: &Tokens) -> Result<Reply> {
    if tokens.len() != 2 {
        return Err(Error::bad_request("bad command line format"));
    }
    let name = tokens.text(1)?;

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

pub fn vlist(tokens: &Tokens) -> Result<Reply> {
    if tokens.len() != 1 {
        return Err(Error::bad_request("bad command line format"));
    }
    let mut out = String::new();
    for index in registry::snapshot() {
        let layout = &index.ann.layout;
        let _ = writeln!(
            out,
            "INDEX {} dim={} quant={} metric={} fbytes={} count={} maxcount={}\r",
            index.name,
            layout.dim,
            layout.quant,
            index.ann.metric,
            layout.filter_bytes,
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
    fn coords_decodes_little_endian_f32() {
        let bytes: Vec<u8> = [1.0f32, -2.0, 0.5]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        assert_eq!(coords(&bytes, 3, "vector").unwrap(), vec![1.0, -2.0, 0.5]);
    }

    #[test]
    fn coords_rejects_a_dimension_mismatch() {
        let bytes = [0u8; 8]; // two floats
        let msg = coords(&bytes, 3, "vector").unwrap_err().to_string();
        assert!(msg.contains("expected 3 dimensions, got 2"), "{msg}");
    }

    #[test]
    fn coords_rejects_non_finite_values() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let bytes = bad.to_le_bytes();
            let msg = coords(&bytes, 1, "query").unwrap_err().to_string();
            assert!(msg.contains("NaN or infinity"), "{bad}: {msg}");
        }
    }

    fn parse_options(args: &[&str]) -> Result<Options> {
        let raw: Vec<crate::engine_api::token_t> = args
            .iter()
            .map(|s| crate::engine_api::token_t {
                value: s.as_ptr().cast::<std::os::raw::c_char>().cast_mut(),
                length: s.len(),
            })
            .collect();
        // SAFETY: `raw` outlives the view and the parse that borrows from it.
        let tokens = unsafe { Tokens::new(raw.as_ptr(), raw.len() as std::os::raw::c_int) };
        Options::parse(&tokens, 3)
    }

    #[test]
    fn options_are_parsed_case_insensitively() {
        let o = parse_options(&[
            "vcreate", "docs", "8", "quant", "i8", "metric", "l2", "FBYTES", "128",
        ])
        .unwrap();
        assert_eq!(o.quant, Quant::I8);
        assert_eq!(o.metric, Metric::L2);
        assert_eq!(o.filter_bytes, 128);
    }

    #[test]
    fn unknown_options_and_values_are_rejected() {
        assert!(parse_options(&["vcreate", "docs", "8", "NOPE", "1"]).is_err());
        assert!(parse_options(&["vcreate", "docs", "8", "QUANT", "f64"]).is_err());
        assert!(parse_options(&["vcreate", "docs", "8", "METRIC", "manhattan"]).is_err());
        // FBYTES must stay a multiple of 16 within range.
        assert!(parse_options(&["vcreate", "docs", "8", "FBYTES", "24"]).is_err());
        assert!(parse_options(&["vcreate", "docs", "8", "FBYTES", "512"]).is_err());
    }

    #[test]
    fn maxcount_is_bounded_by_the_engines_i32_field() {
        // item_attr.maxcount is an i32; without this guard 2^31 would wrap to a
        // negative limit and the index would reject every insert.
        assert_eq!(
            parse_options(&["vcreate", "docs", "8", "MAXCOUNT", "2147483647"])
                .unwrap()
                .maxcount,
            Some(2_147_483_647)
        );
        assert!(parse_options(&["vcreate", "docs", "8", "MAXCOUNT", "2147483648"]).is_err());
        assert!(parse_options(&["vcreate", "docs", "8", "MAXCOUNT", "0"]).is_err());
    }

    #[test]
    fn threads_are_clamped_rather_than_rejected() {
        // Zero would leave usearch with no reserved contexts at all.
        assert_eq!(
            parse_options(&["vcreate", "docs", "8", "THREADS", "0"])
                .unwrap()
                .threads,
            1
        );
    }

    #[test]
    fn options_default_to_a_usable_index() {
        let o = Options::default();
        assert_eq!(o.metric, Metric::Cos);
        assert_eq!(o.quant, Quant::F32);
        assert_eq!(o.filter_bytes, codec::DEFAULT_FILTER_BYTES);
        assert!(o.maxcount.is_none());
    }
}
