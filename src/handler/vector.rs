use super::access::{for_read, for_write, map_is_gone};
use super::coords::coords;
use crate::command::request::Add;
use crate::error::{Error, Reply, Result};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::HeldAddr;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry;
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

    // Taken now, after `for_write`: this write's entry is already published, and anything
    // registered from here on is not what a failure below is about.
    let stamp = registry::now();
    let quantized = quant::encode(&vector, layout.quant);

    // ① The element body first. Allocation is the step most likely to fail under memory
    // pressure, and taking it before the graph insert means a failure costs nothing but this
    // call — no node to unwind.
    let mut pending = match store.reserve_elem(name, id, layout.element_len()) {
        Ok(pending) => pending,
        Err(e) => return store_failed(name, stamp, e),
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
    match index.ann.insert_published(
        staged,
        index.owner(),
        // Read inside the hold, so two writes to one id cannot both see the same outgoing
        // element. The refcount comes along and `insert_published` hands it back.
        || store.hold_addr(name, id).ok().map(HeldAddr::keep),
        // The link, then the refcount that keeps the address the graph is about to key by.
        // Reading it back rather than trusting `pending.addr()` is what makes the key the
        // element the Map actually holds, whatever landed in between.
        || {
            pending.insert()?;
            store.hold_addr(name, id).map(HeldAddr::keep)
        },
    ) {
        Ok(()) => Ok(Reply::Stored),
        Err(PublishError::Store(e)) => store_failed(name, stamp, e),
        // The mapping refused to grow. Nothing was written and nothing is left over, so this
        // is a reply like any other — the daemon does not fall over a write it declined.
        Err(PublishError::Mapping(e)) => Err(e),
    }
}

/// How a failed Map write answers: a full index and an evicted one are replies, not errors.
///
/// `stamp` is the registry clock from before this write's engine call, so an eviction releases
/// the entry this write was working with and nothing registered since.
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

/// `vgetattr <index> <id>` — the stored attributes, read from Map by field.
pub fn vgetattr(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_read(store, name)?;
    let stamp = registry::now();

    // Only the ATTR is answered, so only the ATTR is copied.
    match store.get_attr(name, id, index.ann.layout) {
        Ok(attr) => {
            let json = String::from_utf8_lossy(&attr);
            Ok(Reply::Body(format!(
                "VALUE {id} {}\r\n{json}\r\nEND\r\n",
                json.len()
            )))
        }
        Err(StoreError::ElemGone) => Ok(Reply::NotFound),
        // The engine described this element in a way that cannot be read. Stop offering the id
        // — a search would keep returning a row nothing can render — and say so. The Map is
        // left alone: deleting on a reading we do not trust is the same bad reading twice.
        Err(StoreError::CorruptElement) => {
            if let Ok(held) = store.hold_addr(name, id) {
                index.ann.forget_unreadable(held.addr());
            }
            eprintln!(
                "ArcVector: element '{id}' of index '{name}' is unreadable; dropped from the graph"
            );
            Err(StoreError::CorruptElement.into())
        }
        // The Map itself is gone, which no read path used to act on.
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}

/// `vsetattr <index> <id> <attrlen> <attr JSON>` — replace the attributes, keep the vector.
///
/// The graph is not searched, not inserted into, and not rebuilt. It only follows the element:
/// the engine writes the new value into a **new** element — it writes in place only when
/// nothing holds a refcount, and the graph holds one on every element it keys a node by — so
/// the node moves to the new address. Moving it is a `rename`, which is a hash entry rather
/// than the ~370us HNSW insert a `vadd` pays.
pub fn vsetattr(store: &Store, name: &str, id: &str, attr: &[u8]) -> Result<Reply> {
    let index = for_write(store, name)?;
    let stamp = registry::now();
    check_attr(attr)?;

    // The current value, and where it lives. Both are needed: the value because everything
    // outside the ATTR region has to come back byte for byte — the vector, where this build
    // stores one — and the address because that is the node the graph will have to move.
    //
    // The hold is this call's own. It outlives the update, so the outgoing address still means
    // this element while the graph is told about it.
    let held = match store.hold_elem(name, id) {
        Ok(held) => held,
        Err(StoreError::ElemGone) => return Ok(Reply::NotFound),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };
    let old = held.addr();
    let mut value = held.value().to_vec();
    index.ann.layout.set_attr(&mut value, attr)?;

    match index.ann.update_published(old, || {
        store.update_elem(name, id, &value)?;
        // Where it ended up. Reading it back rather than assuming is what makes the node follow
        // the element the Map actually holds.
        store.hold_addr(name, id).map(HeldAddr::keep)
    }) {
        Ok(()) => Ok(Reply::Stored),
        Err(PublishError::Store(StoreError::ElemGone)) => Ok(Reply::NotFound),
        Err(PublishError::Store(StoreError::KeyGone)) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(PublishError::Store(e)) => Err(e.into()),
        Err(PublishError::Mapping(e)) => Err(e),
    }
}

/// `vdel <index> <id>`
pub fn vdel(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_write(store, name)?;
    let stamp = registry::now();

    // Engine first, always: the Map is the copy replicas and the persistence log follow, so a
    // delete that only reached the graph would come back. Under the mapping's hold, so no
    // reader sees one store without the other.
    let mut map_gone = false;
    // The hold outlives the unlink, so the address still means this element while the graph is
    // asked about it. It is released when `taken` drops, after the graph has let go of its own.
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

    // Outside the hold: releasing the registry entry drops the graph, and the mapping lock is
    // inside it.
    if map_gone {
        map_is_gone(name, stamp);
    }
    match removed {
        Ok(Some(_)) => Ok(Reply::Deleted),
        Ok(None) => Ok(Reply::NotFound),
        Err(PublishError::Store(e)) => Err(e.into()),
        // Refused before the element was touched, so the delete simply did not happen.
        Err(PublishError::Mapping(e)) => Err(e),
    }
}
