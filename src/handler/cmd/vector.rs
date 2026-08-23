use super::coords::coords;
use crate::command::request::Add;
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, for_write, map_is_gone};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::HeldAddr;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry;
use crate::handler::usearch::PublishError;

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

    let mut pending = match store.reserve_elem(name, id, layout.element_len()) {
        Ok(pending) => pending,
        Err(e) => return store_failed(name, stamp, e),
    };

    let staged = index.ann.stage(&quantized, index.owner())?;
    layout.write(pending.value_mut(), &quantized, attr)?;

    match index.ann.insert_published(
        staged,
        index.owner(),
        || store.hold_addr(name, id).ok().map(HeldAddr::keep),
        || {
            let _replaced = pending.insert()?;
            store.hold_addr(name, id).map(HeldAddr::keep)
        },
    ) {
        Ok(()) => Ok(Reply::Stored),
        Err(PublishError::Store(e)) => store_failed(name, stamp, e),

        Err(PublishError::Mapping(e)) => Err(e),
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

pub fn vgetattr(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_read(store, name)?;
    let stamp = registry::now();

    match store.get_attr(name, id, index.ann.layout) {
        Ok(attr) => {
            let json = String::from_utf8_lossy(&attr);
            Ok(Reply::Body(format!(
                "VALUE {id} {}\r\n{json}\r\nEND\r\n",
                json.len()
            )))
        }
        Err(StoreError::ElemGone) => Ok(Reply::NotFound),

        Err(StoreError::CorruptElement) => {
            if let Ok(held) = store.hold_addr(name, id) {
                index.ann.forget_unreadable(held.addr());
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

    let mut pending = match store.reserve_elem(name, id, layout.element_len()) {
        Ok(pending) => pending,
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };

    let mut gone = false;
    let settled = index.ann.update_published(
        || {
            let held = match store.hold_elem(name, id) {
                Ok(held) => held,
                Err(StoreError::KeyGone) => {
                    gone = true;
                    return None;
                }
                Err(_) => return None,
            };
            Some((held.addr(), held.value().to_vec()))
        },
        |value| {
            if value.len() < layout.element_len() {
                return Err(StoreError::CorruptElement);
            }
            let body = pending.value_mut();
            let kept = body.len();
            body.copy_from_slice(&value[..kept]);
            layout
                .set_attr(body, attr)
                .map_err(|_| StoreError::CorruptElement)?;
            let addr = pending.addr();
            if !pending.insert()? {
                let _ = store.delete_elem(name, id);
                return Err(StoreError::ElemGone);
            }

            let held = store.hold_addr(name, id)?;
            debug_assert_eq!(held.addr(), addr, "the engine linked a different element");
            Ok(held.keep())
        },
    );

    if gone {
        map_is_gone(name, stamp);
        return Ok(Reply::NotFound);
    }
    match settled {
        Ok(Some(())) => Ok(Reply::Stored),
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

    let mut map_gone = false;

    let mut taken = None;
    let removed = index
        .ann
        .remove_published(|| match store.take_addr(name, id) {
            Ok(held) => {
                let addr = held.as_ref().map(HeldAddr::addr);
                taken = held;
                Ok(addr)
            }
            Err(StoreError::KeyGone) => {
                map_gone = true;
                Ok(None)
            }
            Err(e) => Err(e),
        });
    drop(taken);

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
