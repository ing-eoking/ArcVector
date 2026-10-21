use super::coords::vector as decode_vector;
use crate::bson;
use crate::cmd::access::{for_write, map_is_gone};
use crate::engine::element::Layout;
use crate::engine::{Store, StoreError};
use crate::error::{Error, Reply, Result};
use crate::index::quant;
use crate::index::registry::{self, Reader};
use crate::protocol::request::{Delete, Parts, SetMeta, Upsert};
use crate::trigger::{event, key};

pub(super) fn check_index(name: &str) -> Result<()> {
    if key::valid_index_name(name) {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "index name must be 1..={} bytes with no spaces",
        key::MAX_INDEX_NAME
    )))
}

pub(super) fn check_vkey(vkey: &str) -> Result<()> {
    if vkey.is_empty() || vkey.len() > key::MAX_ID || vkey.contains(' ') {
        return Err(Error::bad_request(format!(
            "vector key must be 1..={} bytes with no spaces (got {})",
            key::MAX_ID,
            vkey.len()
        )));
    }
    Ok(())
}

pub(super) fn check_names(index: &str, vkey: &str) -> Result<()> {
    check_index(index)?;
    check_vkey(vkey)
}

/// Worker thread: validates input, builds the element body, and writes it to the Map.
pub fn upsert_write(store: &Store, spec: &Upsert, parts: Parts) -> Result<Reply> {
    let Upsert {
        index: name, vkey, ..
    } = spec;
    check_names(name, vkey)?;

    let index = for_write(store, name)?;
    let layout = index.ann.layout;
    let vector = parts
        .vector
        .ok_or_else(|| Error::bad_request("bad data chunk"))?;
    // Dimension is determined by the index. The command specifies body length, verified here.
    let vector = decode_vector(vector, layout.dim, spec.format)?;

    let meta = meta_of(parts.meta)?;
    check_still_fits(store, layout, meta.len())?;

    let stamp = registry::now();
    let quantized = quant::encode(&vector, layout.quant)?;
    let value = build_body(layout, &quantized, &meta)?;

    write(store, index, name, vkey, &value, stamp)
}

/// Worker thread: reads the element and updates its metadata with new BSON.
///
/// Uses `map_elem_update` so non-existent fields are not created; an element
/// deleted after read returns `NOT_FOUND_VECTOR`. Known limitation: if a concurrent
/// upsert for the same vkey occurs between read and update, the vector will be
/// overwritten with the old vector bytes (the engine lacks conditional updates).
///
/// Does not re-insert into the graph: the update triggers an engine replace event,
/// and since the vector remains unchanged, the callback updates the node address.
pub fn setmeta_write(store: &Store, spec: &SetMeta, parts: Parts) -> Result<Reply> {
    let SetMeta {
        index: name, vkey, ..
    } = spec;
    check_names(name, vkey)?;

    let index = for_write(store, name)?;
    let layout = index.ann.layout;
    let stamp = registry::now();
    let meta = parts
        .meta
        .ok_or_else(|| Error::bad_request("bad data chunk"))?;
    let meta = meta_of(Some(meta))?;
    check_still_fits(store, layout, meta.len())?;

    let value = match store.get_elem(name, vkey) {
        Ok(value) => value,
        Err(StoreError::ElemGone) => return Ok(Reply::NotFoundVector),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };
    // If metadata length changes, vector offset shifts. Rebuild with new layout.
    let Ok((_, vector)) = layout.split(&value) else {
        return Err(StoreError::CorruptElement.into());
    };
    let body = match build_body(layout, vector, &meta) {
        Ok(body) => body,
        Err(Error::Codec(_)) => return Err(StoreError::CorruptElement.into()),
        Err(e) => return Err(e),
    };

    // Abort write if index was deleted after lookup to avoid writing with stale schema.
    if index.is_deleted() {
        return Ok(Reply::NotFound);
    }
    match event::expect(&index, || store.update_elem(name, vkey, &body)) {
        Ok(()) => Ok(Reply::Stored),
        Err(StoreError::ElemGone) => Ok(Reply::NotFoundVector),
        Err(e) => store_failed(name, stamp, e),
    }
}

fn write(
    store: &Store,
    index: Reader,
    name: &str,
    id: &str,
    value: &[u8],
    stamp: u64,
) -> Result<Reply> {
    // Abort write if deleted after lookup (same as in `setmeta_write`).
    if index.is_deleted() {
        return Ok(Reply::NotFound);
    }
    match event::expect(&index, || store.insert_elem(name, id, value, true, None)) {
        Ok(done) if done.replaced => Ok(Reply::Replaced),
        Ok(_) => Ok(Reply::Stored),
        Err(e) => store_failed(name, stamp, e),
    }
}

fn store_failed(name: &str, stamp: u64, e: StoreError) -> Result<Reply> {
    match e {
        // Return server error as there is no specific protocol response for overflow.
        StoreError::Overflow => Err(Error::Index("index is full".into())),
        StoreError::KeyGone => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        e => Err(e.into()),
    }
}

fn build_body(layout: Layout, vector: &[u8], meta: &[u8]) -> Result<Vec<u8>> {
    let mut body = crate::fallible::zeroed(layout.element_len(meta.len()))?;
    layout.write(&mut body, vector, meta)?;
    Ok(body)
}

/// Converts client JSON to element metadata (BSON), default to empty document.
/// Returns an error if the JSON is not an object.
fn meta_of(json: Option<&[u8]>) -> Result<Vec<u8>> {
    match json {
        None => Ok(bson::EMPTY.to_vec()),
        Some(json) => Ok(bson::from_json(json)?),
    }
}

/// Extracts metadata from a stored element as compact JSON.
pub(super) fn meta_of_stored(
    layout: Layout,
    body: &[u8],
) -> std::result::Result<String, StoreError> {
    let (meta, _) = layout.split(body).map_err(|_| StoreError::CorruptElement)?;
    bson::to_json(meta).map_err(|_| StoreError::CorruptElement)
}

fn check_still_fits(store: &Store, layout: Layout, meta_len: usize) -> Result<()> {
    let limit = store.max_element_bytes() as usize;
    let stored = layout.stored_len(meta_len);
    if stored <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element is {stored} bytes ({meta_len} metadata + {} vector), over max_element_bytes {limit}",
        layout.vector_bytes(),
    )))
}

pub fn delete(store: &Store, spec: &Delete) -> Result<Reply> {
    let Delete { index: name, vkey } = *spec;
    check_names(name, vkey)?;
    let index = for_write(store, name)?;
    let stamp = registry::now();

    // Graph is not touched directly; the engine unlink event dispatches removal to the key's lane.
    match event::expect(&index, || store.delete_elem(name, vkey)) {
        Ok(()) => Ok(Reply::Deleted),
        Err(StoreError::ElemGone) => Ok(Reply::NotFoundVector),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}
