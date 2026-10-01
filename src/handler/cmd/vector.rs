use std::sync::Arc;

use super::coords::coords;
use crate::command::request::Add;
use crate::engine_api::ENGINE_STORE_OPERATION_OPERATION_SET;
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, for_write, map_is_gone};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry::{self, VectorIndex};

/// `vadd`가 세 단계로 나뉜 상태.
///
/// 문서의 순서 그대로다 -- 아이템을 먼저 할당해 주소를 확정하고, 그 주소에
/// 자리표 노드를 넣고, 마지막에 링크한다. 갈라 둔 이유는 가운데 단계만 무거운
/// 데다 나머지 둘은 엔진 쓰기라서다. 엔진 쓰기는 워커 스레드에 있어야 한다 --
/// 복제가 `rp_before_check`에서 스레드 로컬(`tls_cookie`)을 세운다.
///
/// ```text
/// [워커] allocate  주소 확정. 아직 아무도 못 본다
/// [풀]   stage     addr|STAGED 로 HNSW 삽입. 락 없음. 무거운 일은 여기뿐
/// [워커] link      콜백이 addr|STAGED → addr 로 옮긴다. 해시 항목 하나
/// ```
pub struct PreparedAdd {
    /// For `store_failed`, which needs to know whose index went away.
    name: String,
    stamp: u64,

    /// 할당해 둔 아이템. 링크하면 0이 되어 이 구조체의 소유를 벗어난다.
    addr: u64,
    /// 자리표가 그래프에 들어가 있나.
    staged: bool,

    vector: Vec<u8>,
    index: Arc<VectorIndex>,
}

impl Drop for PreparedAdd {
    /// 링크까지 못 간 모든 길에서 여기로 모인다 -- 풀이 결과를 못 건넸거나,
    /// 엔진 쓰기가 거부됐거나, 연결이 먼저 끊겼거나.
    fn drop(&mut self) {
        if self.addr == 0 {
            return;
        }
        if self.staged {
            self.index.ann.drop_staged(self.addr);
        }
        // 링크된 적이 없으니 unlink도 오지 않는다. 참조를 직접 돌려준다.
        if let Some(store) = Store::background() {
            store.discard_allocated(self.addr);
        }
    }
}

/// What the first half decided.
pub enum AddPlan {
    /// 할당까지 끝났다. 풀이 자리표를 넣고, 워커가 링크한다.
    Write(PreparedAdd),
}

/// **워커 스레드.** 검증하고, 본문을 만들고, 아이템을 할당한다.
pub fn vadd_allocate(store: &Store, spec: &Add, body: &[u8]) -> Result<AddPlan> {
    let Add {
        index: name,
        vkey,
        exptime,
        attr,
    } = spec;
    let attr = attr.as_slice();

    let index = for_write(store, name)?;
    let layout = index.ann.layout;
    // 차원은 인덱스가 정한다. 명령은 본문 길이만 말하고, 좌표 개수가 그와
    // 맞는지는 여기서 본다.
    let vector = coords(body, layout.dim, "vector")?;

    check_attr(attr)?;
    check_still_fits(store, layout)?;

    let stamp = registry::now();
    let quantized = quant::encode(&vector, layout.quant)?;
    let vkey = crate::trigger::key::vector_key(name, vkey);

    let item = build_body(layout, &quantized, attr)?;

    // 클라이언트의 초를 엔진의 시계로 옮긴다. 그대로 넘기면 기동 기준 상대
    // 시각으로 읽혀 대부분 이미 지난 값이 된다.
    let addr = store.allocate_kv(&vkey, &item, crate::server::realtime(*exptime))?;

    Ok(AddPlan::Write(PreparedAdd {
        name: name.to_string(),
        stamp,
        addr,
        staged: false,
        vector: quantized,
        index,
    }))
}

/// **풀 스레드.** 무거운 일은 이 한 줄이다.
pub fn vadd_stage(plan: &mut AddPlan) -> Result<()> {
    let AddPlan::Write(prepared) = plan;
    prepared
        .index
        .ann
        .stage_at(prepared.addr, &prepared.vector)?;
    prepared.staged = true;
    Ok(())
}

/// **워커 스레드.** 링크하면 콜백이 자리표를 실제 주소로 올린다.
pub fn vadd_commit(store: &Store, plan: AddPlan) -> Result<Reply> {
    let AddPlan::Write(mut prepared) = plan;

    // 링크가 콜백을 부르고, 콜백이 `unstage`로 자리표를 제자리에 올린다.
    match store.link_allocated(prepared.addr, ENGINE_STORE_OPERATION_OPERATION_SET) {
        Ok(()) => {
            // 아이템도 자리표도 이제 이 구조체 것이 아니다.
            prepared.addr = 0;
            Ok(Reply::Stored)
        }
        Err(e) => {
            if prepared.staged {
                prepared.index.ann.drop_staged(prepared.addr);
                prepared.staged = false;
            }
            // `link_allocated`가 실패하면서 참조는 이미 돌려줬다. `Drop`이
            // 한 번 더 돌려주지 않도록 여기서 손을 뗀다.
            prepared.addr = 0;
            store_failed(&prepared.name, prepared.stamp, e)
        }
    }
}

/// 세 단계를 한 스레드에서 이어 한다.
///
/// 풀에 못 넘겼을 때 쓰인다. 워커 스레드이므로 엔진 쓰기도 여기서 해도 된다.
pub fn vadd(store: &Store, spec: &Add, body: &[u8]) -> Result<Reply> {
    let mut plan = vadd_allocate(store, spec, body)?;
    vadd_stage(&mut plan)?;
    vadd_commit(store, plan)
}

fn store_failed(name: &str, stamp: u64, e: StoreError) -> Result<Reply> {
    match e {
        StoreError::KeyGone => {
            map_is_gone(name, stamp);
            Err(Error::IndexEvicted)
        }
        e => Err(e.into()),
    }
}

/// 아이템 본문 하나. 길이 바이트, attr, 벡터, 그리고 `\r\n`.
///
/// 종단자가 붙는 것은 이 키들이 ASCII 프로토콜로 읽히기 때문이다 -- 데몬은
/// 아이템 본문을 클라이언트에게 그대로 넘긴다.
fn build_body(layout: Layout, vector: &[u8], attr: &[u8]) -> Result<Vec<u8>> {
    let mut body = crate::room::zeroed(layout.stored_len(attr.len()))?;
    let split = layout.element_len(attr.len());
    layout.write(&mut body[..split], vector, attr)?;
    body[split..].copy_from_slice(b"\r\n");
    Ok(body)
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
    if layout.max_stored_len() <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element is {} bytes ({} header+ATTR + {} vector), over max_item_size {limit}",
        layout.max_stored_len(),
        Layout::vector_offset(crate::handler::arcus::element::ATTR_BYTES),
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
            // attr은 공백 없는 JSON 한 덩어리라 길이를 앞세울 필요가 없다.
            let json = String::from_utf8_lossy(&attr);
            Ok(Reply::Body(format!("ATTR {id}={json}\r\nEND\r\n")))
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
    // attr 길이가 바뀌면 벡터가 앉는 자리도 바뀐다. 제자리 수정이 불가능하니
    // 벡터를 꺼내 새 길이로 다시 만든다.
    let Some(vector) = layout.vector_of(&value) else {
        return Err(StoreError::CorruptElement.into());
    };
    let body = build_body(layout, vector, attr).map_err(|_| StoreError::CorruptElement)?;

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
        // 인덱스는 있다 -- `for_write`가 방금 확인했다. 없는 것은 이 이름의
        // 벡터뿐이다.
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFoundVector)
        }
        Err(e) => Err(e.into()),
    }
}
