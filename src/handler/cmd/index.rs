use std::fmt::Write as _;

use crate::command::request::Create;
use crate::error::{Error, Reply, Result};
use crate::handler::access::resolve;
use crate::handler::arcus::element::{self, Layout, MetaRecord};
use crate::handler::arcus::engine::{self, Store, StoreError};
use crate::handler::quant::Quant;
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::AnnIndex;

pub fn vcreate(store: &Store, spec: &Create) -> Result<Reply> {
    let Create {
        index: name,
        dim,
        metric,
        quant,
        ..
    } = *spec;

    let layout = Layout::new(dim, quant);
    check_dimension_fits(store, layout, quant)?;

    let ann = AnnIndex::new(
        layout,
        metric,
        spec.connectivity,
        spec.expansion_add,
        spec.expansion_search,
        std::sync::Arc::new(engine::ItemElements),
    )?;

    let meta = MetaRecord {
        metric: metric.as_str().to_owned(),
        connectivity: spec.connectivity,
        expansion_add: spec.expansion_add,
        expansion_search: spec.expansion_search,
    };
    let index = VectorIndex::building(name.to_owned(), ann);

    let (registered, previous) = match registry::put(index) {
        Ok(pair) => pair,
        Err(e) => {
            return Err(Error::Index(format!(
                "the index registry could not grow: {e}"
            )));
        }
    };

    // 이 이름으로 먼저 도착해 기다리던 벡터가 있으면 여기서 그래프에 넣는다.
    // 보통은 없어서 곧장 지나간다. 메타를 쓰기 **전**에 하는 것은, 링크 콜백이
    // 곧바로 이 인덱스를 서빙으로 올리기 때문이다 -- 그 뒤에 넣으면 반쯤 찬
    // 그래프가 잠깐 조회에 보인다.
    crate::trigger::recover::adopt(&registered);

    match store.add_meta(name, &meta, layout) {
        Ok(()) => {
            // 서빙으로 올리는 것은 `on_meta`가 한다. 복제본에는 `vcreate`가
            // 오지 않아 콜백이 유일한 길이므로, 마스터도 같은 길을 타게 둔다.
            if previous.is_some() {
                eprintln!(
                    "ArcVector: index '{name}' had no metadata; released the graph it stood on"
                );
                registry::keep_until_empty(previous);
            }
            Ok(Reply::Created)
        }

        // `add` refuses a key already there, which is exactly the question
        // `vcreate` is asking.
        Err(StoreError::NotStored) => {
            registry::unput(&registered, previous);
            already_there(store, name)
        }
        Err(e) => {
            registry::unput(&registered, previous);
            Err(e.into())
        }
    }
}

/// `add`가 거절했다 -- 그 키에 이미 무언가 있다.
///
/// 우리가 아는 인덱스면 `EXISTS`다. 아니면 그 자리에 우리 것이 아닌 아이템이
/// 있다는 뜻이라 `TYPE_MISMATCH`로 답한다.
fn already_there(store: &Store, name: &str) -> Result<Reply> {
    match resolve(store, name) {
        Ok(_) => Ok(Reply::Exists),
        Err(_) => Ok(Reply::TypeMismatch),
    }
}

pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    // 살아 있는 엔트리만 센다. 앞서 드롭돼 비워지는 중인 그래프는 `known`에
    // 잡히지만, 그건 이미 없는 인덱스다 -- 두 번째 `vdrop`은 `NOT_FOUND`다.
    let known = registry::contains(name);

    // 1. The metadata item goes first.
    //
    // Unlinking it fires the unlink callback, and that callback is what takes
    // the index out of the registry -- here and on a replica alike, because a
    // replica never sees this command, only the replicated delete. From the
    // moment it returns, nothing can reach the index.
    //
    // The graph is not freed with it. It still holds the reference to every
    // vector that is still linked, and `registry::holding` is how the unlinks
    // below find it again.
    let had_meta = match store.delete_kv(&crate::trigger::key::meta_key(name)) {
        Ok(()) => true,
        Err(StoreError::KeyGone) => false,
        Err(e) => return Err(e.into()),
    };
    // With no metadata there was no callback, so the entry is still there.
    registry::remove(name);

    // 2. Then the vectors, in bulk.
    //
    // The flush does not unlink them all. It stamps the prefix and walks each
    // LRU as far as the first item older than the stamp, leaving the rest to be
    // invalidated when something touches them. Those late unlinks are why the
    // graph has to outlive this call.
    if had_meta && let Err(e) = store.flush_prefix(&crate::trigger::key::index_prefix(name)) {
        eprintln!("ArcVector: '{name}' was dropped, but its vectors were not flushed ({e})");
    }

    Ok(if known || had_meta {
        Reply::Dropped
    } else {
        Reply::NotFound
    })
}

fn check_dimension_fits(store: &Store, layout: Layout, quant: Quant) -> Result<()> {
    let limit = store.max_item_size() as usize;

    if layout.max_stored_len() <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element would be {} bytes, over max_item_size {limit} \
         (max dimension is {} for quant {quant})",
        layout.max_stored_len(),
        Layout::max_dim_for(quant, limit),
    )))
}

pub fn vstats() -> Result<Reply> {
    let indexes = registry::snapshot();

    let mut vectors = 0usize;
    let mut held_bytes = 0usize;
    let mut used_bytes = 0usize;
    let mut per_index = String::new();
    for index in &indexes {
        let (count, held, used) = (
            index.ann.len(),
            index.ann.held_bytes(),
            index.ann.used_bytes(),
        );
        vectors += count;
        held_bytes += held;
        used_bytes += used;

        let name = &index.name;
        let _ = write!(
            per_index,
            "STAT {name}:vectors {count}\r\n\
             STAT {name}:index_held_bytes {held}\r\n\
             STAT {name}:index_used_bytes {used}\r\n\
             STAT {name}:reserved {}\r\n",
            index.ann.reserved()
        );
    }

    let mut out = String::new();
    let _ = write!(
        out,
        "STAT indexes {}\r\n\
         STAT vectors {vectors}\r\n\
         STAT index_held_bytes {held_bytes}\r\n\
         STAT index_used_bytes {used_bytes}\r\n\
         STAT attr_bytes_per_vector {}\r\n\
         STAT vectors_awaiting_metadata {}\r\n",
        indexes.len(),
        element::ATTR_BYTES,
        crate::trigger::waiting::count(),
    );
    out.push_str(&per_index);
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

pub fn vlist() -> Result<Reply> {
    let mut out = String::new();
    for index in registry::snapshot() {
        let layout = &index.ann.layout;
        let _ = writeln!(
            out,
            "INDEX {} dim={} quant={} metric={} attrbytes={} count={}\r",
            index.name,
            layout.dim,
            layout.quant,
            index.ann.metric,
            element::ATTR_BYTES,
            index.ann.len(),
        );
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
