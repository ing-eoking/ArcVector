use super::access::{for_read, for_write, map_is_gone};
use super::coords::coords;
use crate::command::request::Add;
use crate::error::{Error, Reply, Result};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::usearch::PublishError;

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

    // No capacity pre-check here. The engine already refuses the link with EOVERFLOW at
    // exactly `maxcount` vectors — the Map is sized `maxcount + 1` for the metadata element
    // — and it counts elements, which is the authority. The graph's `len` is not: a write
    // whose link failed can leave a node named for an element the Map no longer holds, and
    // a count that runs high would answer OVERFLOWED for an id that fits.

    let quantized = quant::encode(&vector, layout.quant);

    // ① The element body first. Allocation is the step most likely to fail under memory
    // pressure, and taking it before the graph insert means a failure costs nothing but this
    // call — no node to unwind.
    let mut pending = match store.reserve_elem(name, id, layout.element_len()) {
        Ok(pending) => pending,
        Err(e) => return store_failed(name, e),
    };

    // ② ③ ④ Mint the key, count the write in flight, insert the node. Nothing names it yet,
    // so no reader can reach it and there is nothing to publish or undo — which is what lets
    // all of this happen outside the lock below. It is also the expensive part: measured at
    // ~370us for a 768-dimension vector against ~0.2us for the mapping write it feeds.
    //
    // Staged under the owner this write started with; the publish compares it against the
    // owner then, so a takeover in between voids the node instead of naming a wiped one.
    let staged = index.ann.stage(&quantized, index.owner())?;
    layout.write(pending.value_mut(), &quantized, attr)?;

    // ⑤ The locked pair, and ⑦ the count coming back down when `staged` drops on the way out.
    // The node this write displaces comes from the mapping, so the hold contains one engine
    // call and nothing else.
    //
    // That call is the step which leaves this node: `CLOG_MAP_ELEM_INSERT` is emitted from
    // `do_map_elem_link`, so replicas and the persistence log learn of the write there and
    // nowhere earlier — reserving logs nothing. Everything fallible is already done.
    match index
        .ann
        .insert_published(id, staged, index.owner(), || pending.insert())
    {
        Ok(()) => Ok(Reply::Stored),
        Err(PublishError::Store(e)) => store_failed(name, e),
        // The mapping refused to grow. Nothing was written and nothing is left over, so this
        // is a reply like any other — the daemon does not fall over a write it declined.
        Err(PublishError::Mapping(e)) => Err(e),
    }
}

/// How a failed Map write answers: a full index and an evicted one are replies, not errors.
fn store_failed(name: &str, e: StoreError) -> Result<Reply> {
    match e {
        StoreError::Overflow => Ok(Reply::Overflowed),
        StoreError::KeyGone => {
            map_is_gone(name);
            Err(Error::IndexEvicted)
        }
        e => Err(e.into()),
    }
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
        Err(StoreError::ElemGone) => Ok(Reply::NotFound),
        // The Map itself is gone, which no read path used to act on.
        Err(StoreError::KeyGone) => {
            map_is_gone(name);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}

/// `vdel <index> <id>`
pub fn vdel(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_write(store, name)?;

    // Engine first, always: the Map is the copy replicas and the persistence log follow, so a
    // delete that only reached the graph would come back. Under the mapping's hold, so no
    // reader sees one store without the other.
    let mut map_gone = false;
    let removed = index
        .ann
        .remove_published(id, || match store.take_elem(name, id) {
            Ok(_) => Ok(true),
            Err(StoreError::ElemGone) => Ok(false),
            Err(StoreError::KeyGone) => {
                map_gone = true;
                Ok(false)
            }
            Err(e) => Err(e),
        });

    // Outside the hold: releasing the registry entry drops the graph, and the mapping lock is
    // inside it.
    if map_gone {
        map_is_gone(name);
    }
    match removed {
        Ok(Some(_)) => Ok(Reply::Deleted),
        Ok(None) => Ok(Reply::NotFound),
        Err(PublishError::Store(e)) => Err(e.into()),
        // Refused before the element was touched, so the delete simply did not happen.
        Err(PublishError::Mapping(e)) => Err(e),
    }
}
