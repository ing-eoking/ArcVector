use super::coords::vector as decode_vector;
use crate::bson;
use crate::command::request::{Delete, Parts, SetMeta, Upsert};
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_write, map_is_gone};
use crate::handler::arcus::element::Layout;
use crate::handler::arcus::engine::{Inserted, Store, StoreError};
use crate::handler::quant;
use crate::handler::registry;
use crate::trigger::{event, key, recover};

/// `upsert`/`setmeta`가 엔진에 쓴 뒤, 그래프에 넣기를 기다리는 것.
///
/// 쓰기는 워커가 하고, 넣기는 그 키의 레인이 하고, 답은 다시 워커가 한다.
///
/// ```text
/// [워커] write   요소를 Map에 넣는다. 링크 콜백이 주소를 `Reserved`로 적어
///                이 명령에게 돌려준다. cache lock 아래의 일은 그것뿐이다
/// [레인] insert  그 주소의 벡터를 HNSW에 넣는다. 락 없음. 무거운 일은 여기뿐
/// [워커] reply   답한다. 넣기가 실패했어도 엔진에는 쓰지 않는다
/// ```
///
/// 엔진 쓰기가 워커에 있어야 하는 것은 복제가 `rp_before_check`에서 스레드
/// 로컬(`tls_cookie`)을 세우기 때문이다.
pub struct Added {
    name: String,
    id: String,
    /// 링크 콜백이 돌려준 주소. 넣은 것은 빠진다.
    linked: event::Linked,
    /// 넣기가 성공하면 이것으로 답한다.
    reply: Reply,
}

impl Drop for Added {
    /// 넣지 못한 채 버려지면 -- 레인이 결과를 못 건넸거나 연결이 먼저 끊겼거나 --
    /// 여기서 넣는다. `Reserved`로 남겨 두면 adopt도 건드리지 않아 그 벡터가
    /// 영영 그래프 밖에 남는다.
    fn drop(&mut self) {
        for (index, addr) in self.linked.drain(..) {
            let _ = recover::insert_reserved(&index, addr);
        }
    }
}

/// 쓰기가 끝난 뒤 무엇이 남았는가.
pub enum UpsertPlan {
    /// 그래프에 넣을 것이 없다. 이 답을 그대로 준다.
    Answered(Reply),
    /// 쓰기는 끝났다. 레인이 넣고, 워커가 답한다.
    Insert(Added),
}

impl UpsertPlan {
    /// `setmeta`는 새 필드든 교체든 `STORED`다.
    fn stored(self) -> Self {
        match self {
            UpsertPlan::Answered(_) => UpsertPlan::Answered(Reply::Stored),
            UpsertPlan::Insert(mut added) => {
                added.reply = Reply::Stored;
                UpsertPlan::Insert(added)
            }
        }
    }
}

/// 인덱스 이름이 키 모양에 맞는가.
pub(super) fn check_index(name: &str) -> Result<()> {
    if key::valid_index_name(name) {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "index name must be 1..={} bytes with no spaces",
        key::MAX_INDEX_NAME
    )))
}

/// 벡터 이름이 Map 필드 모양에 맞는가.
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

/// **워커 스레드.** 검증하고, 본문을 만들고, 요소를 Map에 쓴다.
pub fn upsert_write(store: &Store, spec: &Upsert, parts: Parts) -> Result<UpsertPlan> {
    let Upsert {
        index: name, vkey, ..
    } = spec;
    check_names(name, vkey)?;

    let index = for_write(store, name)?;
    let layout = index.ann.layout;
    let vector = parts
        .vector
        .ok_or_else(|| Error::bad_request("bad data chunk"))?;
    // 차원은 인덱스가 정한다. 명령은 본문 길이만 말하고, 그와 맞는지는 여기서 본다.
    let vector = decode_vector(vector, layout.dim, spec.format)?;

    let meta = meta_of(parts.meta)?;
    check_still_fits(store, layout, meta.len())?;

    let stamp = registry::now();
    let quantized = quant::encode(&vector, layout.quant)?;
    let value = build_body(layout, &quantized, &meta)?;

    write(store, name, vkey, &value, stamp)
}

/// **워커 스레드.** 요소를 읽어 메타만 새 BSON으로 바꿔 갱신한다.
///
/// 쓰기는 `map_elem_update`라 없는 필드를 만들지 않는다 -- 읽은 뒤 지워진
/// vkey는 `NOT_FOUND_VECTOR`다. 알려진 한계: 읽기와 갱신 사이에 같은 vkey의
/// upsert가 끼어들면 그 벡터가 읽어 둔 옛 벡터 바이트로 덮인다(엔진에 조건부
/// 갱신이 없다).
///
/// 그래프는 다시 넣지 않는다. 이 쓰기가 엔진의 replace 이벤트를 부르고, 벡터가
/// 같으니 콜백이 노드를 새 주소로 옮긴다.
pub fn setmeta_write(store: &Store, spec: &SetMeta, parts: Parts) -> Result<UpsertPlan> {
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
        Err(StoreError::ElemGone) => return Ok(UpsertPlan::Answered(Reply::NotFoundVector)),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(UpsertPlan::Answered(Reply::NotFound));
        }
        Err(e) => return Err(e.into()),
    };
    // 메타 길이가 바뀌면 벡터가 앉는 자리도 바뀐다. 제자리 수정이 불가능하니
    // 벡터를 꺼내 새 길이로 다시 만든다.
    let Ok((_, vector)) = layout.split(&value) else {
        return Err(StoreError::CorruptElement.into());
    };
    let body = build_body(layout, vector, &meta).map_err(|_| StoreError::CorruptElement)?;

    let outcome = event::capture(|| {
        store.update_elem(name, vkey, &body).map(|()| Inserted {
            replaced: true,
            ..Inserted::default()
        })
    });
    match finish_write(name, vkey, stamp, outcome) {
        Err(Error::IndexEvicted) => Ok(UpsertPlan::Answered(Reply::NotFound)),
        Err(Error::Store(StoreError::ElemGone)) => Ok(UpsertPlan::Answered(Reply::NotFoundVector)),
        Err(e) => Err(e),
        Ok(plan) => Ok(plan.stored()),
    }
}

/// 요소를 쓰고, 링크 콜백이 돌려준 주소를 챙긴다.
fn write(store: &Store, name: &str, id: &str, value: &[u8], stamp: u64) -> Result<UpsertPlan> {
    let outcome = event::capture(|| store.insert_elem(name, id, value, true, None));
    finish_write(name, id, stamp, outcome)
}

/// 쓰기의 결과와, 그 쓰기가 링크한 주소로 다음 일을 정한다.
fn finish_write(
    name: &str,
    id: &str,
    stamp: u64,
    (written, linked): (std::result::Result<Inserted, StoreError>, event::Linked),
) -> Result<UpsertPlan> {
    let mut added = Added {
        name: name.to_owned(),
        id: id.to_owned(),
        linked,
        reply: Reply::Stored,
    };

    match written {
        // 실패한 쓰기는 아무것도 링크하지 않는다. 혹시 남은 것이 있으면
        // `Added`가 버려지면서 넣는다.
        Err(e) => return Err(store_failed(name, stamp, e)),
        Ok(done) if done.replaced => added.reply = Reply::Replaced,
        Ok(_) => {}
    }
    if added.linked.is_empty() {
        // 콜백이 그래프를 이미 맞췄다(메타만 바뀐 교체), 또는 평소 길로 넘겼다.
        return Ok(UpsertPlan::Answered(std::mem::replace(
            &mut added.reply,
            Reply::Stored,
        )));
    }
    Ok(UpsertPlan::Insert(added))
}

/// **레인 스레드.** 무거운 일은 이것뿐이다.
pub fn upsert_insert(added: &mut Added) -> Result<()> {
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

/// 넣기의 결과로 답만 한다. **엔진에 쓰지 않는다.**
///
/// `on_io_complete` 콜백에서 쓰인다. 거기서는 엔진 쓰기를 할 수 없다 -- 그래프가
/// 받지 못한 요소는 저장소에 남고(stored but not indexed), 그것을 로그로 남긴다.
/// 레인이든 그 자리든 같은 길이다.
pub fn upsert_reply(mut added: Added, inserted: Result<()>) -> Result<Reply> {
    match inserted {
        Ok(()) => Ok(std::mem::replace(&mut added.reply, Reply::Stored)),
        Err(e) => {
            eprintln!(
                "ArcVector: '{}' in '{}' is stored but not indexed: {e}",
                added.id, added.name
            );
            Err(e)
        }
    }
}

fn store_failed(name: &str, stamp: u64, e: StoreError) -> Error {
    match e {
        // 기준 PDF에 해당 응답이 없어 서버 오류로 낸다.
        StoreError::Overflow => Error::Index("index is full".into()),
        StoreError::KeyGone => {
            map_is_gone(name, stamp);
            Error::IndexEvicted
        }
        e => e.into(),
    }
}

/// 요소 값 하나. BSON 메타, 벡터. 종단자는 `insert_elem`이 붙인다.
fn build_body(layout: Layout, vector: &[u8], meta: &[u8]) -> Result<Vec<u8>> {
    let mut body = crate::room::zeroed(layout.element_len(meta.len()))?;
    layout.write(&mut body, vector, meta)?;
    Ok(body)
}

/// 클라이언트의 JSON을 요소 메타(BSON)로 바꾼다. 없으면 빈 문서다.
/// JSON 객체가 아니면 `from_json`의 오류가 그대로 나간다.
fn meta_of(json: Option<&[u8]>) -> Result<Vec<u8>> {
    match json {
        None => Ok(bson::EMPTY.to_vec()),
        Some(json) => Ok(bson::from_json(json)?),
    }
}

/// 저장된 요소 값에서 메타를 공백 없는 JSON으로 꺼낸다.
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
    let _index = for_write(store, name)?;
    let stamp = registry::now();

    // 그래프는 건드리지 않는다. 이 삭제가 엔진의 unlink 이벤트를 부르고, 그
    // 콜백이 노드를 빼고 참조를 sweeper에게 넘긴다.
    match store.delete_elem(name, vkey) {
        Ok(()) => Ok(Reply::Deleted),
        Err(StoreError::ElemGone) => Ok(Reply::NotFoundVector),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}
