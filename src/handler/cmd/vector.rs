use super::coords::coords;
use crate::bson;
use crate::command::request::Add;
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, for_write, map_is_gone};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry;
use crate::trigger::{event, key, recover};

/// `vadd`가 엔진에 쓴 뒤, 그래프에 넣기를 기다리는 것.
///
/// 쓰기는 워커가 하고, 넣기는 풀이 하고, 답은 다시 워커가 한다.
///
/// ```text
/// [워커] write   요소를 Map에 넣는다. 링크 콜백이 주소를 `Reserved`로 적어
///                이 명령에게 돌려준다. cache lock 아래의 일은 그것뿐이다
/// [풀]   insert  그 주소의 벡터를 HNSW에 넣는다. 락 없음. 무거운 일은 여기뿐
/// [워커] finish  답한다. 넣기가 실패했으면 저장소에서도 지워 둘을 맞춘다
/// ```
///
/// 엔진 쓰기가 워커에 있어야 하는 것은 복제가 `rp_before_check`에서 스레드
/// 로컬(`tls_cookie`)을 세우기 때문이다.
pub struct Added {
    name: String,
    id: String,
    /// 링크 콜백이 돌려준 주소. 넣은 것은 빠진다.
    linked: event::Linked,
}

impl Drop for Added {
    /// 넣지 못한 채 버려지면 -- 풀이 결과를 못 건넸거나 연결이 먼저 끊겼거나 --
    /// 여기서 넣는다. `Reserved`로 남겨 두면 adopt도 건드리지 않아 그 벡터가
    /// 영영 그래프 밖에 남는다.
    fn drop(&mut self) {
        for (index, addr) in self.linked.drain(..) {
            let _ = recover::insert_reserved(&index, addr);
        }
    }
}

/// What the first half decided.
pub enum AddPlan {
    /// 그래프에 넣을 것이 없다. 이 답을 그대로 준다.
    Answered(Reply),
    /// 쓰기는 끝났다. 풀이 넣고, 워커가 답한다.
    Insert(Added),
}

/// **워커 스레드.** 검증하고, 본문을 만들고, 요소를 Map에 쓴다.
pub fn vadd_write(store: &Store, spec: &Add, body: &[u8]) -> Result<AddPlan> {
    let Add {
        index: name,
        vkey,
        attr,
    } = spec;
    let attr = attr.as_slice();

    let index = for_write(store, name)?;
    let layout = index.ann.layout;
    // 차원은 인덱스가 정한다. 명령은 본문 길이만 말하고, 좌표 개수가 그와
    // 맞는지는 여기서 본다.
    let vector = coords(body, layout.dim, "vector")?;

    check_id(vkey)?;
    let meta = meta_of(attr)?;
    check_still_fits(store, layout, meta.len())?;

    let stamp = registry::now();
    let quantized = quant::encode(&vector, layout.quant)?;
    let value = build_body(layout, &quantized, &meta)?;

    write(store, name, vkey, &value, stamp)
}

/// 요소를 쓰고, 링크 콜백이 돌려준 주소를 챙긴다.
fn write(store: &Store, name: &str, id: &str, value: &[u8], stamp: u64) -> Result<AddPlan> {
    let (written, linked) =
        event::capture(|| store.insert_elem(&key::map_key(name), id, value, true, None));
    let added = Added {
        name: name.to_owned(),
        id: id.to_owned(),
        linked,
    };

    if let Err(e) = written {
        // 실패한 쓰기는 아무것도 링크하지 않는다. 혹시 남은 것이 있으면
        // `Added`가 버려지면서 넣는다.
        return store_failed(name, stamp, e).map(AddPlan::Answered);
    }
    if added.linked.is_empty() {
        // 콜백이 그래프를 이미 맞췄다(attr만 바뀐 교체), 또는 평소 길로 넘겼다.
        return Ok(AddPlan::Answered(Reply::Stored));
    }
    Ok(AddPlan::Insert(added))
}

/// **풀 스레드.** 무거운 일은 이것뿐이다.
pub fn vadd_insert(added: &mut Added) -> Result<()> {
    let mut outcome = Ok(());
    for (index, addr) in added.linked.drain(..) {
        if let Err(e) = recover::insert_reserved(&index, addr)
            && outcome.is_ok()
        {
            outcome = Err(e);
        }
    }
    outcome
}

/// **워커 스레드.** 넣기의 결과로 답한다.
///
/// 그래프가 받지 못했으면 저장소에서도 지운다. 남겨 두면 저장소에는 있는데
/// 검색에는 영영 안 나오는 벡터가 된다 -- 실패를 답한 쓰기가 반쯤 남는 것보다
/// 없던 일이 되는 편이 낫다.
pub fn vadd_finish(store: &Store, added: Added, inserted: Result<()>) -> Result<Reply> {
    match inserted {
        Ok(()) => Ok(Reply::Stored),
        Err(e) => {
            if let Err(gone) = store.delete_elem(&key::map_key(&added.name), &added.id)
                && gone != StoreError::ElemGone
            {
                eprintln!(
                    "ArcVector: '{}' in '{}' is stored but not indexed: {gone}",
                    added.id, added.name
                );
            }
            Err(e)
        }
    }
}

/// 넣기의 결과로 답만 한다. **엔진에 쓰지 않는다.**
///
/// `on_io_complete` 콜백에서 쓰인다. 거기서는 엔진 쓰기를 할 수 없어
/// [`vadd_finish`]처럼 지워 맞추지 못한다 -- 그래프가 받지 못한 요소는 저장소에
/// 남고(stored but not indexed), 그것을 로그로 남긴다.
pub fn vadd_reply(added: Added, inserted: Result<()>) -> Result<Reply> {
    match inserted {
        Ok(()) => Ok(Reply::Stored),
        Err(e) => {
            eprintln!(
                "ArcVector: '{}' in '{}' is stored but not indexed: {e}",
                added.id, added.name
            );
            Err(e)
        }
    }
}

/// 세 단계를 한 스레드에서 이어 한다.
///
/// 풀에 못 넘겼을 때 쓰인다. 워커 스레드이므로 엔진 쓰기도 여기서 해도 된다.
pub fn vadd(store: &Store, spec: &Add, body: &[u8]) -> Result<Reply> {
    finish_here(store, vadd_write(store, spec, body)?)
}

fn finish_here(store: &Store, plan: AddPlan) -> Result<Reply> {
    match plan {
        AddPlan::Answered(reply) => Ok(reply),
        AddPlan::Insert(mut added) => {
            let inserted = vadd_insert(&mut added);
            vadd_finish(store, added, inserted)
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

/// 요소 값 하나. BSON 메타, 벡터. 종단자는 `insert_elem`이 붙인다.
fn build_body(layout: Layout, vector: &[u8], meta: &[u8]) -> Result<Vec<u8>> {
    let mut body = crate::room::zeroed(layout.element_len(meta.len()))?;
    layout.write(&mut body, vector, meta)?;
    Ok(body)
}

fn check_id(id: &str) -> Result<()> {
    if id.len() > key::MAX_ID {
        return Err(Error::bad_request(format!(
            "vector key is {} bytes, over the {}-byte limit",
            id.len(),
            key::MAX_ID
        )));
    }
    Ok(())
}

/// 클라이언트의 JSON attr을 요소 메타(BSON)로 바꾼다. 비었으면 빈 문서다.
/// JSON 객체가 아니면 `from_json`의 오류가 그대로 나간다.
fn meta_of(attr: &[u8]) -> Result<Vec<u8>> {
    if attr.is_empty() {
        return Ok(bson::EMPTY.to_vec());
    }
    Ok(bson::from_json(attr)?)
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

pub fn vgetattr(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let index = for_read(store, name)?;
    let stamp = registry::now();

    let layout = index.ann.layout;
    match store.get_elem(&key::map_key(name), id).and_then(|body| {
        let (meta, _) = layout
            .split(&body)
            .map_err(|_| StoreError::CorruptElement)?;
        bson::to_json(meta).map_err(|_| StoreError::CorruptElement)
    }) {
        Ok(json) => {
            // attr은 공백 없는 JSON 한 덩어리라 길이를 앞세울 필요가 없다.
            Ok(Reply::Body(format!("ATTR {id}={json}\r\nEND\r\n")))
        }
        Err(StoreError::ElemGone) => Ok(Reply::NotFound),

        Err(StoreError::CorruptElement) => {
            // 그래프에서 빼지 않는다. 이 요소가 실제로 사라질 때 unlink
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
    let meta = meta_of(attr)?;
    check_still_fits(store, index.ann.layout, meta.len())?;
    let layout = index.ann.layout;

    // 지금 값을 읽어 attr만 갈아 끼우고 다시 쓴다. 그래프는 건드리지 않는다 --
    // 이 쓰기가 엔진의 replace 이벤트를 부르고, 벡터가 같으니 콜백이 노드를 새
    // 주소로 옮긴다.
    let value = match store.get_elem(&key::map_key(name), id) {
        Ok(value) => value,
        Err(StoreError::ElemGone) => return Ok(Reply::NotFound),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };
    // 메타 길이가 바뀌면 벡터가 앉는 자리도 바뀐다. 제자리 수정이 불가능하니
    // 벡터를 꺼내 새 길이로 다시 만든다.
    let Ok((_, vector)) = layout.split(&value) else {
        return Err(StoreError::CorruptElement.into());
    };
    let body = build_body(layout, vector, &meta).map_err(|_| StoreError::CorruptElement)?;

    match write(store, name, id, &body, stamp) {
        Err(Error::IndexEvicted) => Ok(Reply::NotFound),
        Err(e) => Err(e),
        // 아직 그래프에 들어가지 않았던 벡터면 콜백이 이 명령에게 넘긴다.
        // 드문 길이라 이 스레드에서 마저 넣는다.
        Ok(plan) => finish_here(store, plan),
    }
}

pub fn vdel(store: &Store, name: &str, id: &str) -> Result<Reply> {
    let _index = for_write(store, name)?;
    let stamp = registry::now();

    // 그래프는 건드리지 않는다. 이 삭제가 엔진의 unlink 이벤트를 부르고, 그
    // 콜백이 노드를 빼고 참조를 sweeper에게 넘긴다.
    match store.delete_elem(&key::map_key(name), id) {
        Ok(()) => Ok(Reply::Deleted),
        Err(StoreError::ElemGone) => Ok(Reply::NotFoundVector),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}
