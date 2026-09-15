use super::coords::coords;
use crate::command::request::Add;
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, for_write, map_is_gone};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry;
use crate::handler::usearch::{PublishError, Published};

pub fn vadd(store: &Store, spec: &Add, body: &[u8]) -> Result<Reply> {
    let Add {
        index: name,
        id,
        dim,
        attr,
    } = spec;
    let attr = attr.as_slice();

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

    let stamp = registry::now();
    let quantized = quant::encode(&vector, layout.quant);

    let vkey = crate::trigger::key::vector_key(name, id);

    // A Map enforced `maxcount` as an attribute; separate items have nothing to
    // count against each other, so the graph is the count and this is the gate.
    // An id already stored is an update and must not be refused -- it does not
    // grow the index -- so the existence check runs only once the limit is hit.
    if index.ann.len() >= index.maxcount as usize {
        match store.hold_kv(&vkey) {
            Ok(addr) => store.release_items(&[addr]),
            Err(StoreError::KeyGone) => return Ok(Reply::Overflowed),
            Err(e) => return Err(e.into()),
        }
    }

    let mut body = vec![0u8; layout.stored_len()];
    layout.write(&mut body[..layout.element_len()], &quantized, attr)?;
    // The daemon hands an item's body to a client verbatim, and these keys are
    // reachable from the ASCII protocol now, so the body ends the way every
    // other item's does.
    body[layout.element_len()..].copy_from_slice(b"\r\n");

    let staged = index.ann.stage(&quantized)?;

    match index.ann.insert_published(
        staged,
        || store.hold_kv(&vkey).ok(),
        || store.set_kv(&vkey, &body),
    ) {
        Ok(Published::Indexed) => Ok(Reply::Stored),
        Ok(Published::Unindexed(addr)) => {
            index_it(&index, name, addr, quantized);
            Ok(Reply::Stored)
        }
        Err(PublishError::Store(e)) => store_failed(name, stamp, e),

        Err(PublishError::Mapping(e)) => Err(e),
    }
}

fn index_it(
    index: &std::sync::Arc<crate::handler::registry::VectorIndex>,
    name: &str,
    addr: u64,
    vector: Vec<u8>,
) {
    match index.ann.add_unless_known(addr, || Ok(Some(vector))) {
        Ok(true) => {}
        Ok(false) => index.ann.unclaimed(addr),
        Err(e) => {
            eprintln!(
                "ArcVector: '{name}' stored an element its graph would not take ({e}); \
                 dropping the graph so the next read rebuilds it from the Map"
            );
            index.ann.unclaimed(addr);
            registry::remove_observed(name, index);
        }
    }
}

fn store_failed(name: &str, stamp: u64, e: StoreError) -> Result<Reply> {
    match e {
        StoreError::Overflow => Ok(Reply::Overflowed),
        StoreError::KeyGone => {
            map_is_gone(name, stamp);
            Err(Error::IndexEvicted)
        }
        e => Err(e.into()),
    }
}

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

fn check_still_fits(store: &Store, layout: Layout) -> Result<()> {
    let limit = store.max_item_size() as usize;
    if layout.stored_len() <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element is {} bytes ({} header+ATTR + {} vector), over max_item_size {limit}",
        layout.stored_len(),
        Layout::VECTOR_OFFSET,
        layout.vector_bytes(),
    )))
}

pub fn vgetattr(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_read(store, name)?;
    let stamp = registry::now();

    let layout = index.ann.layout;
    match store
        .get_kv(&crate::trigger::key::vector_key(name, id))
        .and_then(|body| {
            layout
                .attr_of(&body)
                .map(<[u8]>::to_vec)
                .map_err(|_| StoreError::CorruptElement)
        }) {
        Ok(attr) => {
            let json = String::from_utf8_lossy(&attr);
            Ok(Reply::Body(format!(
                "VALUE {id} {}\r\n{json}\r\nEND\r\n",
                json.len()
            )))
        }
        Err(StoreError::ElemGone) => Ok(Reply::NotFound),

        Err(StoreError::CorruptElement) => {
            if let Ok(addr) = store.hold_kv(&crate::trigger::key::vector_key(name, id)) {
                index.ann.forget_unreadable(addr);
                store.release_items(&[addr]);
            }
            eprintln!(
                "ArcVector: element '{id}' of index '{name}' is unreadable; dropped from the graph"
            );
            Err(StoreError::CorruptElement.into())
        }

        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn vsetattr(store: &Store, name: &str, id: &str, attr: &[u8]) -> Result<Reply> {
    let index = for_write(store, name)?;
    let stamp = registry::now();
    check_attr(attr)?;
    let layout = index.ann.layout;
    let vkey = crate::trigger::key::vector_key(name, id);

    let mut gone = false;
    let mut kept_vector: Option<Vec<u8>> = None;
    let settled = index.ann.update_published(
        || {
            let addr = match store.hold_kv(&vkey) {
                Ok(addr) => addr,
                Err(StoreError::KeyGone) => {
                    gone = true;
                    return None;
                }
                Err(_) => return None,
            };
            let body = store.with_item_at(addr, |_key, value| value.to_vec());
            // The lookup's own reference has done its job; the graph keeps the
            // one it already holds.
            store.release_items(&[addr]);
            body.map(|body| (addr, body))
        },
        |value| {
            if value.len() < layout.element_len() {
                return Err(StoreError::CorruptElement);
            }
            kept_vector = layout.vector_of(&value).map(<[u8]>::to_vec);

            let mut body = vec![0u8; layout.stored_len()];
            body[..layout.element_len()].copy_from_slice(&value[..layout.element_len()]);
            layout
                .set_attr(&mut body[..layout.element_len()], attr)
                .map_err(|_| StoreError::CorruptElement)?;
            body[layout.element_len()..].copy_from_slice(b"\r\n");

            store.set_kv(&vkey, &body)
        },
    );

    if gone {
        map_is_gone(name, stamp);
        return Ok(Reply::NotFound);
    }
    match settled {
        Ok(Some(Published::Indexed)) => Ok(Reply::Stored),
        Ok(Some(Published::Unindexed(addr))) => {
            match kept_vector {
                Some(vector) => index_it(&index, name, addr, vector),
                None => {
                    index.ann.unclaimed(addr);
                    registry::remove_observed(name, &index);
                }
            }
            Ok(Reply::Stored)
        }
        Ok(None) => Ok(Reply::NotFound),
        Err(PublishError::Store(StoreError::ElemGone)) => Ok(Reply::NotFound),
        Err(PublishError::Store(StoreError::KeyGone)) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(PublishError::Store(e)) => Err(e.into()),
        Err(PublishError::Mapping(e)) => Err(e),
    }
}

pub fn vdel(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_write(store, name)?;
    let stamp = registry::now();

    let vkey = crate::trigger::key::vector_key(name, id);
    let mut map_gone = false;

    // A second reference, taken only to learn the address the graph is holding.
    // The one the graph owns is what `remove_published` retires; this extra one
    // goes back below, from this request thread, where no lock is held.
    let looked_up = match store.hold_kv(&vkey) {
        Ok(addr) => Some(addr),
        Err(StoreError::KeyGone) => {
            map_gone = true;
            None
        }
        Err(e) => return Err(e.into()),
    };

    let removed = index.ann.remove_published(|| match looked_up {
        Some(addr) => store.delete_kv(&vkey).map(|()| Some(addr)),
        None => Ok(None),
    });

    if let Some(addr) = looked_up {
        store.release_items(&[addr]);
    }
    if map_gone {
        map_is_gone(name, stamp);
    }
    match removed {
        Ok(Some(_)) => Ok(Reply::Deleted),
        Ok(None) => Ok(Reply::NotFound),
        Err(PublishError::Store(e)) => Err(e.into()),

        Err(PublishError::Mapping(e)) => Err(e),
    }
}
