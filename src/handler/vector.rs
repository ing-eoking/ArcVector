//! Commands that act on one vector, named by its id.
//!
//! The two that write do so Map first and the graph second, so a failure between
//! them loses a cache entry rather than data.

use super::access::{for_read, for_write};
use super::coords::coords;
use super::registry;
use crate::command::request::Add;
use crate::error::{Error, Reply, Result};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;

/// `vadd <index> <id> <veclen> <dim> [ATTR <attrlen> <attr JSON>]`
pub fn vadd(store: &Store, spec: &Add, body: &[u8]) -> Result<Reply> {
    let Add {
        index: name,
        id,
        dim,
        attr,
    } = spec;
    let attr = attr.as_slice();

    // veclen only sized the body; this is what checks it holds `dim` coordinates.
    let vector = coords(body, *dim, "vector")?;

    let index = for_write(store, name)?;
    if *dim != index.ann.layout.dim {
        return Err(Error::bad_request(format!(
            "index {name} has dimension {}, got {dim}",
            index.ann.layout.dim
        )));
    }
    let layout = index.ann.layout;

    check_attr(attr)?;
    check_still_fits(store, layout)?;

    if index.ann.key_of(id).is_none() && index.ann.len() >= index.maxcount as usize {
        return Ok(Reply::Overflowed);
    }

    let quantized = quant::encode(&vector, layout.quant);
    let value = layout.encode(&quantized, attr)?;

    // Map first: it is the source of truth.
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

/// `vadd`'s ATTR must be a JSON object: it is stored as one and queried by field.
fn check_attr(attr: &[u8]) -> Result<()> {
    if attr.is_empty() {
        return Ok(());
    }
    match serde_json::from_slice::<serde_json::Value>(attr) {
        Ok(serde_json::Value::Object(_)) => Ok(()),
        Ok(_) => Err(Error::bad_request("ATTR must be a JSON object")),
        Err(e) => Err(Error::bad_request(format!("ATTR is not valid JSON: {e}"))),
    }
}

/// Re-check the element size against the engine's current limit.
fn check_still_fits(store: &Store, layout: Layout) -> Result<()> {
    let limit = store.max_element_bytes() as usize;
    if layout.stored_len() <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element is {} bytes ({} header+ATTR + {} vector), over max_element_bytes {limit}",
        layout.stored_len(),
        Layout::VECTOR_OFFSET,
        layout.vector_bytes(),
    )))
}

/// `vget <index> <id>` — the stored attributes, read from Map by field.
pub fn vget(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_read(store, name)?;

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

/// `vdel <index> <id>`
pub fn vdel(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_write(store, name)?;

    // Map first. A leftover graph node is filtered out of searches.
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
