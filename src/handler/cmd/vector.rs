use super::coords::coords;
use crate::command::request::Add;
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, for_write, map_is_gone};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry;

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

    // 그래프는 건드리지 않는다. 이 쓰기가 엔진의 link(또는 replace) 이벤트를
    // 부르고, 그 콜백이 노드를 넣는다 -- 복제본과 복구가 거치는 경로와 같다.
    // 같은 id를 다시 쓰는 것은 갱신이라 거절하지 않는다.
    match store.set_kv(&vkey, &body) {
        Ok(_) => Ok(Reply::Stored),
        Err(e) => store_failed(name, stamp, e),
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
            // 그래프에서 빼지 않는다. 이 아이템이 실제로 사라질 때 unlink
            // 이벤트가 오고, 그때 콜백이 노드를 뺀다.
            eprintln!("ArcVector: element '{id}' of index '{name}' is unreadable");
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

    // 지금 값을 읽어 attr만 갈아 끼우고 다시 쓴다. 그래프는 건드리지 않는다 --
    // 이 쓰기가 엔진의 replace 이벤트를 부르고, 그 콜백이 노드를 새 주소로 옮긴다.
    let addr = match store.hold_kv(&vkey) {
        Ok(addr) => addr,
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };
    let value = store.with_item_at(addr, |_key, value| value.to_vec());
    store.release_items(&[addr]);

    let Some(value) = value else {
        return Ok(Reply::NotFound);
    };
    if value.len() < layout.element_len() {
        return Err(StoreError::CorruptElement.into());
    }

    let mut body = vec![0u8; layout.stored_len()];
    body[..layout.element_len()].copy_from_slice(&value[..layout.element_len()]);
    layout
        .set_attr(&mut body[..layout.element_len()], attr)
        .map_err(|_| StoreError::CorruptElement)?;
    body[layout.element_len()..].copy_from_slice(b"\r\n");

    match store.set_kv(&vkey, &body) {
        Ok(_) => Ok(Reply::Stored),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn vdel(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let _index = for_write(store, name)?;
    let stamp = registry::now();
    let vkey = crate::trigger::key::vector_key(name, id);

    // 그래프는 건드리지 않는다. 이 삭제가 엔진의 unlink 이벤트를 부르고, 그
    // 콜백이 노드를 빼고 참조를 sweeper에게 넘긴다.
    match store.delete_kv(&vkey) {
        Ok(()) => Ok(Reply::Deleted),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}
