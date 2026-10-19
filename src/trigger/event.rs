//! 엔진이 인덱스 Map과 그 요소의 수명을 알려오는 콜백.
//!
//! 다섯 경우가 한 콜백으로 온다. `event_data_t`가 어느 경우인지와, 관련된
//! 포인터(`target`, `source`)와, 우리가 참조를 몇 개 가져갈지 적을 자리
//! (`target_ref`, `source_ref`)를 들고 온다. 요소 이벤트는 그 요소가 속한 Map
//! 아이템(`coll_item`)도 함께 들고 온다 -- 요소에는 자기가 어느 Map 것인지 적혀
//! 있지 않아서, 그것이 어느 인덱스의 일인지 알 길이 그것뿐이다. Map의 키가 곧
//! 인덱스 이름이다.
//!
//! 인덱스는 Map이 링크될 때(`ITEM_LINK`) 생긴다. 설정이 Map 아이템의 값에 있어
//! 콜백이 그 자리에서 읽는다 -- 이 노드의 `vcreate`든, 복제나 persistence 재생으로
//! 들어온 Map이든 같은 길이다. 요소는 언제나 링크된 Map에 들어가므로, 요소
//! 이벤트가 올 때 그 인덱스는 이미 레지스트리에 있다. 없으면 우리 인덱스가 아니다.
//!
//! 엔진은 콜백이 돌아온 뒤 우리가 적은 수만큼 참조를 옮긴다.
//!
//! | | 엔진이 하는 일 | 우리가 적는 값 |
//! |---|---|---|
//! | ELEM_INSERT | `target += target_ref` | 붙잡으면 1, 못 붙잡으면 0 |
//! | ELEM_DELETE | `target -= target_ref` | 지금 놓을 것이면 1, 계속 들거나 안 잡던 것이면 0 |
//! | ELEM_REPLACE | `target += target_ref`, `source -= source_ref` | new는 INSERT, old는 DELETE 규칙 |
//! | ITEM_LINK / ITEM_UNLINK | 같은 식 | 언제나 0 -- Map 아이템의 참조는 주고받지 않는다 |
//!
//! DELETE·REPLACE 뒤에 참조가 0이 된 old는 엔진이 그 자리에서 해제한다.
//!
//! # 지켜야 하는 하나
//!
//! **우리가 INSERT에서 1로 답한 요소의 참조가, 들고 있는 것의 전부다.**
//! 0으로 답하면 엔진은 참조를 주지 않는다. REPLACE의 new도 같다.
//!
//! 그래서 붙잡아 둘 수 없으면 거기서 끝낸다. 요소는 저장소에 남지만 우리 것이
//! 아니다. 들고 있지도 않은 것을 나중에 놓아주면 아직 쓰는 요소가 먼저
//! 해제되고, 들고 있는 것을 안 놓아주면 Map이 지워져도 요소가 남는다.
//!
//! 그러려면 delete에서 "이걸 내가 들고 있나"에 답할 수 있어야 한다. 장부를 따로
//! 두지 않고 이미 있는 것으로 답한다:
//!
//! | 무엇 | 들고 있나 | 무엇을 보고 아나 |
//! |---|---|---|
//! | 그래프에 들어가길 기다리는 벡터 | 예 | `waiting`에 있다 |
//! | 그래프에 있던 벡터 | 예 | 그 이름의 그래프가 `holds` |
//! | 그 밖의 것 | 아니오 | 링크에서 거절했다 |
//!
//! # 넣는 일은 여기서 하지 않는다
//!
//! **이 함수는 엔진의 `cache_lock`을 쥔 채 불린다.** 요소를 링크한 그 스레드에서
//! 온다 -- 클라이언트 쓰기면 워커, 복제본이면 복제 적용 스레드, 기동 시면 복구
//! 스레드, Map이 지워지면 엔진의 컬렉션 삭제 스레드다. 여기서 HNSW에 넣으면
//! 그동안 데몬 전체가 서므로, 콜백은 주소만 `waiting`에 적고 풀에 넘긴다.
//!
//! 링크한 것이 이 스레드의 `vadd`라면 [`capture`]가 그 주소를 명령에게
//! 돌려준다. 명령이 직접 풀에 넘기고 넣기가 끝난 뒤에 답하므로, 답을 받은
//! 클라이언트의 다음 검색은 그 벡터를 본다.
//!
//! 여기서 부를 수 있는 엔진 함수는 `get_item_info`와 `get_elem_info` 뿐이고,
//! 나머지는 전부 `cache_lock`을 다시 잡아 그 자리에서 데드락이다.

use std::cell::RefCell;
use std::os::raw::c_void;
use std::sync::Arc;

use crate::handler::arcus::element::IndexConfig;
use crate::handler::arcus::engine::{MapElements, Store};
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::AnnIndex;
use crate::trigger::recover;
use crate::trigger::waiting::{self, Replaced, Unlinked};

use crate::engine_api::{
    ENGINE_EVENT_TYPE, ENGINE_EVENT_TYPE_ON_EVENT_ITEM, event_data_t, event_type_t,
    event_type_t_EVENT_ELEM_DELETE, event_type_t_EVENT_ELEM_INSERT,
    event_type_t_EVENT_ELEM_REPLACE, event_type_t_EVENT_ITEM_LINK, event_type_t_EVENT_ITEM_UNLINK,
};

/// 빠지는(또는 교체되는) 요소 old를 어떻게 하기로 했는지.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Old {
    /// 계속 들고 있다. 그래프에 있던 것이라 진행 중인 검색이 끝난 뒤 sweeper가,
    /// 또는 넣던 풀이 놓는다.
    Held,
    /// 그래프에 들어간 적이 없다. 아무도 역참조하지 않으니 엔진이 지금 회수한다.
    Returned,
    /// 우리가 잡은 적이 없다. 깎을 참조가 없다.
    NotOurs,
}

/// 콜백이 무엇을 하기로 했는지. 디스패치를 엔진 포인터에서 떼어내 시험할 수 있게
/// 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    /// 새로 들어온 요소(INSERT·REPLACE의 `target`)를 붙잡았나.
    new: bool,
    /// 나가는 요소(DELETE의 `target`, REPLACE의 `source`)를 어떻게 하나.
    old: Old,
}

impl Decision {
    /// 참조를 주고받지 않는다. 아이템 이벤트와 우리 것이 아닌 요소.
    pub(crate) const NOTHING: Self = Self {
        new: false,
        old: Old::NotOurs,
    };

    pub(crate) fn insert(held: bool) -> Self {
        Self {
            new: held,
            old: Old::NotOurs,
        }
    }

    pub(crate) fn delete(old: Old) -> Self {
        Self { new: false, old }
    }

    pub(crate) fn replace(new: bool, old: Old) -> Self {
        Self { new, old }
    }
}

/// 엔진에 돌려줄 참조 변화량. `target_ref`, `source_ref` 그대로다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refs {
    pub target: u16,
    pub source: u16,
}

/// 결정을 엔진이 옮길 참조 수로 바꾼다. 모듈 머리의 첫 표가 이것이다.
pub(crate) fn refs_for(event: Event, decision: Decision) -> Refs {
    let returned = u16::from(decision.old == Old::Returned);
    let held = u16::from(decision.new);
    match event {
        Event::ItemLink | Event::ItemUnlink => Refs {
            target: 0,
            source: 0,
        },
        Event::Insert => Refs {
            target: held,
            source: 0,
        },
        Event::Delete => Refs {
            target: returned,
            source: 0,
        },
        Event::Replace => Refs {
            target: held,
            source: returned,
        },
    }
}

/// 엔진이 알려온 다섯 경우.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// Map이 링크됐다. 값이 설정 문서면 **이것이 인덱스가 생기는 것이다.**
    ItemLink,
    /// Map이 빠진다. **이것이 인덱스가 사라지는 것이다.**
    ItemUnlink,
    /// 요소가 들어왔다. 붙잡을지 답한다.
    Insert,
    /// 요소가 빠진다. 들고 있던 참조를 지금 놓을지 답한다.
    Delete,
    /// 요소가 새것으로 교체된다. 그래프를 옮기고 old를 놓아준다.
    Replace,
}

impl Event {
    pub(crate) fn of(ty: event_type_t) -> Option<Self> {
        match ty {
            t if t == event_type_t_EVENT_ITEM_LINK => Some(Self::ItemLink),
            t if t == event_type_t_EVENT_ITEM_UNLINK => Some(Self::ItemUnlink),
            t if t == event_type_t_EVENT_ELEM_INSERT => Some(Self::Insert),
            t if t == event_type_t_EVENT_ELEM_DELETE => Some(Self::Delete),
            t if t == event_type_t_EVENT_ELEM_REPLACE => Some(Self::Replace),
            _ => None,
        }
    }
}

/// 콜백을 등록한다. `memcached_extensions_initialize`에서 한 번 부른다.
///
/// 그 시점은 getopt 루프 안이라 엔진 초기화보다 **앞**이고, 따라서 persistence
/// 복구가 요소를 다시 링크하기 전이다. 그래서 복구가 만드는 link도 놓치지 않는다.
pub fn install() {
    let server = crate::server::handle();
    if server.is_null() {
        eprintln!("ArcVector: no server API; the item event callback is not installed");
        return;
    }
    let callback = unsafe { (*server).callback };
    if callback.is_null() {
        eprintln!("ArcVector: no callback API; the item event callback is not installed");
        return;
    }
    let Some(register) = (unsafe { (*callback).register_callback }) else {
        eprintln!("ArcVector: register_callback missing; the item event callback is not installed");
        return;
    };
    unsafe {
        register(
            std::ptr::null_mut(),
            ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
            Some(on_item_event),
            std::ptr::null(),
        );
    }
}

/// 한 명령이 링크한 요소를 몇 개까지 돌려받는가.
///
/// `vadd` 하나는 요소 하나를 링크하거나 교체한다. 자리는 미리 잡아 두므로
/// 콜백이 cache lock 아래에서 할당하지 않는다. 넘치면 그 요소는 평소 길(adopt)로
/// 간다 -- 명령이 기다리지 않을 뿐 그래프에는 들어간다.
const CAPTURE_ROOM: usize = 4;

/// 명령이 돌려받는 주소와, 그것이 들어갈 인덱스.
pub(crate) type Linked = Vec<(Arc<VectorIndex>, u64)>;

thread_local! {
    /// 이 스레드에서 쓰기를 하는 명령이 돌려받을 주소. 그 명령이 도는 동안만
    /// `Some`이다.
    static LINKED: RefCell<Option<Linked>> = const { RefCell::new(None) };
}

/// 명령이 끝나면 칸을 내린다. 도중에 풀려도 다음 콜백이 엉뚱한 명령에 주소를
/// 돌려주지 않도록.
struct Capturing;

impl Drop for Capturing {
    fn drop(&mut self) {
        LINKED.with(|linked| linked.borrow_mut().take());
    }
}

/// `f`가 링크한 벡터 요소를 돌려받는다. **워커 스레드에서, 엔진 쓰기를 감싸
/// 부른다.**
///
/// 돌려받은 주소는 `waiting`에 `Reserved`로 적혀 있어 adopt가 건드리지 않는다.
/// 부른 쪽이 하나하나 [`recover::insert_reserved`]로 넣어야 한다 -- 넣지 않으면
/// 그 벡터는 그래프 밖에 남는다.
pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, Linked) {
    let mut room = Vec::new();
    // 자리를 못 잡으면 빈 채로 간다. 콜백이 넘친 것으로 보고 평소 길로 보낸다.
    let _ = room.try_reserve_exact(CAPTURE_ROOM);
    LINKED.with(|linked| *linked.borrow_mut() = Some(room));
    let guard = Capturing;
    let out = f();
    let linked = LINKED
        .with(|linked| linked.borrow_mut().take())
        .unwrap_or_default();
    drop(guard);
    (out, linked)
}

/// 이 스레드의 명령에게 주소를 넘긴다. 넘겼으면 `true`.
///
/// `waiting`에 이미 있는 주소(replace가 자리를 물려준 것)면 `already` -- 새로
/// 적지 않고 그 자리를 명령 몫으로 돌린다.
fn hand_to_command(index: &Arc<VectorIndex>, addr: u64, already: bool) -> bool {
    LINKED.with(|linked| {
        let Ok(mut linked) = linked.try_borrow_mut() else {
            return false;
        };
        let Some(list) = linked.as_mut() else {
            return false;
        };
        if list.len() == list.capacity() {
            return false;
        }
        let held = if already {
            waiting::reserve_waiting(&index.name, addr)
        } else {
            waiting::reserve(&index.name, addr)
        };
        if held {
            list.push((Arc::clone(index), addr));
        }
        held
    })
}

/// 엔진이 `perform_callbacks(ON_EVENT_ITEM, &ev, NULL)`로 부른다.
///
/// `mc_util.c`의 `h->cb(c, type, data, cb_data)` 순서 때문에 **`event_data`가
/// `&ev`이고 `cookie`는 NULL**이다. 반대로 읽으면 널 포인터를 역참조한다.
unsafe extern "C" fn on_item_event(
    _cookie: *const c_void,
    _type: ENGINE_EVENT_TYPE,
    event_data: *const c_void,
    _cb_data: *const c_void,
) {
    if event_data.is_null() {
        return;
    }
    // 엔진이 자기 스택의 `ev`를 넘긴다. 콜백이 돌아온 뒤 `target_ref`와
    // `source_ref`를 읽어 참조를 옮기므로, 우리 답은 거기 적는다.
    let ev = unsafe { &mut *(event_data as *mut event_data_t) };
    let Some(event) = Event::of(ev.type_) else {
        return;
    };

    let decision = unsafe { dispatch(event, ev) };

    let refs = refs_for(event, decision);
    ev.target_ref = refs.target;
    ev.source_ref = refs.source;
}

/// # Safety
///
/// `ev`의 포인터들은 엔진이 살려둔 것이고 이 호출 동안 유효하다.
unsafe fn dispatch(event: Event, ev: &event_data_t) -> Decision {
    let Some(store) = Store::background() else {
        // vtable이 아직 없다. 등록된 인덱스도 없으니 받을 것도 없다.
        return Decision::NOTHING;
    };

    match event {
        Event::ItemLink => {
            on_create(&store, ev.target as u64);
            // Map 아이템의 참조는 주고받지 않는다.
            Decision::NOTHING
        }
        Event::ItemUnlink => {
            if let Some(name) = index_of(&store, ev.target as u64) {
                on_drop(&name);
            }
            // Map 아이템의 참조는 주고받지 않는다.
            Decision::NOTHING
        }
        Event::Insert | Event::Delete | Event::Replace => {
            let Some(name) = index_of(&store, ev.coll_item as u64) else {
                // 이름으로 읽히지 않는 키다. 우리 인덱스일 수 없으니 아무것도
                // 받지 않고, 받은 적 없는 것은 깎지 않는다.
                return Decision::NOTHING;
            };
            match event {
                Event::Insert => Decision::insert(on_link(&name, ev.target as u64)),
                Event::Delete => Decision::delete(on_unlink(&name, ev.target as u64)),
                _ => on_replace(&store, &name, ev.source as u64, ev.target as u64),
            }
        }
    }
}

/// Map 아이템의 키를 읽어 어느 인덱스인지 알아낸다.
fn index_of(store: &Store, map: u64) -> Option<String> {
    if map == 0 {
        return None;
    }
    store.with_key_at(map, |key| {
        let index = crate::trigger::key::parse(key)?;
        // **엔진 캐시락 안이다.** 이름 한 벌을 뜨는 것뿐이지만, `to_owned`였다면
        // 그 실패가 abort였다 -- 락을 쥔 채로. 자리가 없으면 이 이벤트를 흘린다.
        match crate::room::string(index) {
            Ok(index) => Some(index),
            Err(_) => {
                eprintln!("ArcVector: no room to route an item event; the graph will lag");
                None
            }
        }
    })?
}

/// 인덱스의 Map이 빠졌다. 레지스트리에서 내린다.
///
/// 마스터에서는 `vdrop`이, 복제본과 재생에서는 복제된 삭제가, 어디서든 만료와
/// eviction이 같은 자리에 닿는다. 그래프는 버리지 않고 비워질 때까지 들고
/// 있는다 -- 요소들은 엔진의 컬렉션 삭제 스레드가 나중에 지우고, 그때마다 오는
/// unlink가 `registry::holding`으로 이 그래프를 다시 찾는다.
fn on_drop(name: &str) {
    registry::remove(name);
}

/// 벡터 요소가 링크됐다. 붙잡아 두고 그래프에 넣을 길로 보낸다.
///
/// 인덱스를 모르면 우리 Map이 아니다 -- 인덱스는 Map이 링크될 때 등록되고,
/// 요소는 링크된 Map에만 들어간다. 받지 않는다.
///
/// 붙잡았으면 `true`. 엔진은 그때만 참조를 하나 올린다.
fn on_link(name: &str, addr: u64) -> bool {
    let Some(index) = registry::get(name) else {
        return false;
    };
    link_into(&index, addr)
}

/// 주소 하나를 인덱스에 들인다.
///
/// **여기서 HNSW에 삽입하지 않는다.** 이 함수는 엔진의 cache lock을 쥔 채
/// 불리고, 삽입은 `vadd`의 유일한 무거운 일이다.
///
/// * 이 스레드의 명령이 링크한 것이면 그 명령에게 넘긴다. 명령이 풀에 넘겨
///   넣고 나서 답한다.
/// * 그 밖에는 주소만 적어 두고 풀의 adopt에 맡긴다 -- 복제본이 따라잡는 동안
///   데몬이 서지 않게 하는 것이 요점이다.
///
/// 붙잡았으면 `true`.
fn link_into(index: &Arc<VectorIndex>, addr: u64) -> bool {
    if hand_to_command(index, addr, false) {
        return true;
    }

    if !waiting::push(&index.name, addr) {
        // 적어 둘 자리가 없다. 참조를 안 받으므로 이 요소는 우리 것이 아니고,
        // delete는 그래프에 노드가 없는 것을 보고 놓아주지 않는다.
        eprintln!(
            "ArcVector: no room to hold a linked '{}' vector; it stays in the \
             store, out of this index",
            index.name
        );
        return false;
    }

    if !recover::submit_adopt(index) {
        // 풀이 못 받았다. 목록에 남아 있고 sweeper가 매 틱 다시 맡긴다.
        eprintln!(
            "ArcVector: '{}' waits for a free worker before this vector is indexed",
            index.name
        );
    }
    true
}

/// REPLACE의 새 요소를 붙잡는다. 붙잡았으면 `true`.
///
/// 엔진은 우리가 1로 답할 때만 new의 참조를 올린다. 그래서 붙잡을 수 없으면
/// 여기서 끝낸다 -- 받은 적 없는 참조를 놓아줄 큐에 넣으면 안 된다. 그 요소는
/// 저장소에 남지만 그래프 밖이고(stored but not indexed), 나중의 delete는
/// "우리 것이 아니다"로 읽는다. 살아 있는 인덱스가 없으면 우리 Map이 아니라
/// 받지 않는다.
fn take_replacement(live: Option<&Arc<VectorIndex>>, new: u64) -> bool {
    live.is_some_and(|index| link_into(index, new))
}

/// 이벤트 플래그가 선 Map이 링크됐다. 값이 설정 문서면 인덱스를 세워 등록한다.
///
/// **이때가 이 인덱스가 이 프로세스에서 생기는 순간이다.** 이 노드의 `vcreate`,
/// 복제로 들어온 Map, persistence 재생이 모두 이 링크에 닿는다. 복제본에는
/// `vcreate`가 오지 않으므로 이 경로 말고는 인덱스가 생길 길이 없다.
///
/// 등록하자마자 서빙한다. 막 링크된 Map에는 요소가 없고, 뒤따라 들어오는 요소는
/// 링크 콜백이 `waiting`에 적어 풀의 adopt가 넣는다.
fn on_create(store: &Store, map: u64) {
    let Some(name) = index_of(store, map) else {
        return;
    };
    let Some(index) = store
        .with_item_value_at(map, |value| build_from_value(name, value))
        .flatten()
    else {
        return;
    };
    let Ok((registered, previous)) = registry::put(index) else {
        // 이름은 인덱스와 함께 넘어갔다. cache lock 아래라 따로 떠 두지 않는다.
        eprintln!("ArcVector: the index registry could not grow; a new index stays unknown");
        return;
    };
    // 같은 키의 Map은 둘일 수 없고 옛 Map의 unlink가 먼저 내렸을 것이다. 혹시
    // 남아 있었으면 그 그래프는 비워질 때까지 들고 있는다.
    registry::keep_until_empty(previous);
    // 이 시각 전에 시작한 명령의 "Map이 없다"는 판정이 새 인덱스를 내리지 않게
    // 한다(`registry::remove_if_stale`).
    registered.publish();
}

/// Map 값으로 인덱스를 세운다. 아직 등록하지는 않는다.
///
/// 값이 설정 문서로 읽히지 않으면 `None` -- 우리 인덱스가 아니다. 조용히
/// 지나간다.
///
/// `AnnIndex::new`가 usearch의 그래프를 잡으므로 cache lock 아래에서 도는
/// 할당이다. 인덱스 하나당 한 번뿐이라 감수한다.
fn build_from_value(name: String, value: &[u8]) -> Option<VectorIndex> {
    let config = IndexConfig::decode(value).ok()?;
    match AnnIndex::new(
        config.layout(),
        config.metric,
        config.m,
        config.efc,
        config.efs,
        Arc::new(MapElements),
    ) {
        Ok(ann) => Some(VectorIndex::ours(name, ann)),
        Err(e) => {
            eprintln!("ArcVector: could not build the graph for '{name}': {e}");
            None
        }
    }
}

/// 요소가 빠진다. 우리가 들고 있는 것이면 놓아준다.
///
/// 무엇을 들고 있는지는 이 모듈 머리의 표가 답한다.
///
/// 레지스트리에 없는 이름이어도 묻는다. Map이 지워진 **뒤에** 컬렉션 삭제
/// 스레드가 요소를 지우므로, 그때 인덱스는 이미 내려가 있고 그래프는
/// `registry::holding`이 찾는 버려지는 쪽에 있다.
fn on_unlink(name: &str, addr: u64) -> Old {
    match waiting::unlink(name, addr) {
        // 그래프에 들어간 적은 없지만 참조는 우리 것이다. 엔진이 지금 회수한다.
        Unlinked::NeverLinked => return Old::Returned,
        // 풀이 넣는 중이다. 들고 있으면 그쪽이 노드를 빼고 참조를 돌려준다.
        Unlinked::Recovering => return Old::Held,
        Unlinked::NotWaiting => {}
    }

    let Some(index) = registry::holding(name, addr) else {
        // 어느 그래프도 이 주소를 갖고 있지 않다. 링크 때 거절했다는 뜻이라
        // 놓아줄 참조가 없다 -- 없는 것을 큐에 넣으면 안 받은 참조를 놓는다.
        return Old::NotOurs;
    };

    index.ann.unlink_at(addr);
    if index.ann.retire_one(addr) {
        return Old::Held;
    }

    // 놓아줄 큐가 찼다. 잠그는 것이 도는 역참조를 기다려 주므로 엔진이 그
    // 자리에서 회수해도 된다.
    index.ann.halt_for_overflow();
    Old::Returned
}

/// 두 요소가 같은 벡터를 담고 있나. attr만 바꾼 쓰기를 가려낸다.
fn same_vector(store: &Store, index: &VectorIndex, old: u64, new: u64) -> bool {
    let layout = index.ann.layout;
    store
        .with_elem_at(old, |_field, old_value| {
            store.with_elem_at(new, |_field, new_value| {
                match (layout.split(old_value), layout.split(new_value)) {
                    (Ok((_, a)), Ok((_, b))) => a == b,
                    _ => false,
                }
            })
        })
        .flatten()
        .unwrap_or(false)
}

/// 요소가 새것으로 교체된다.
///
/// 두 답이 따로다. new는 INSERT처럼 붙잡았는지(`target_ref`), old는 DELETE처럼
/// 지금 놓을지(`source_ref`). 엔진은 new를 우리가 답한 만큼만 올린다.
fn on_replace(store: &Store, name: &str, old: u64, new: u64) -> Decision {
    let live = registry::get(name);

    // 기다리던 벡터가 교체됐다. 자리를 그대로 물려받으므로 목록이 자라지 않고,
    // 그래서 이 경로는 자리가 없어 실패할 일이 없다.
    match waiting::replace(name, old, new, false) {
        Replaced::Swapped => {
            // new가 old의 자리에 들어갔다. 이 스레드의 명령이 쓴 것이면 그 명령
            // 몫으로 돌리고, 아니면 adopt에 맡긴다. old는 그래프에 없으니
            // 엔진이 지금 회수한다.
            if let Some(index) = &live
                && !hand_to_command(index, new, true)
            {
                recover::submit_adopt(index);
            }
            return Decision::replace(true, Old::Returned);
        }
        // 풀이 old를 넣는 중이다. 그쪽이 빼고 돌려주고, new는 따로 잡아둔다.
        Replaced::Recovering => {
            let held = take_replacement(live.as_ref(), new);
            return Decision::replace(held, Old::Held);
        }
        Replaced::NotWaiting => {}
    }

    let Some(holder) = registry::holding(name, old) else {
        // old는 링크 때 거절한 것이라 돌려줄 참조가 없다. new만 받아 둔다.
        let held = take_replacement(live.as_ref(), new);
        return Decision::replace(held, Old::NotOurs);
    };
    let in_live = live.as_ref().is_some_and(|live| Arc::ptr_eq(live, &holder));

    // attr만 바뀌었다(`vsetattr`). 노드를 새 주소로 옮기면 끝이다 -- 다시 넣을
    // 이유가 없고, 그래프가 한순간도 이 벡터를 놓치지 않는다.
    // 노드가 new를 가리키게 됐으니 new는 붙잡은 것이다.
    if in_live && same_vector(store, &holder, old, new) && holder.ann.rename_node(old, new) {
        if holder.ann.retire_one(old) {
            return Decision::replace(true, Old::Held);
        }
        // old는 이미 그래프에 없다. 잠그는 것이 도는 역참조를 기다려 주므로
        // 엔진이 그 자리에서 회수해도 된다.
        holder.ann.halt_for_overflow();
        return Decision::replace(true, Old::Returned);
    }

    // 벡터가 바뀌었거나 old가 버려지는 중인 그래프에 있다. old를 빼고 new를
    // 새로 넣는다. `rename_node`로 끝내면 주소만 바뀌고 그래프는 옛 좌표를
    // 계속 들고 있다.
    holder.ann.unlink_at(old);
    let gave_back = holder.ann.retire_one(old);
    if !gave_back {
        holder.ann.halt_for_overflow();
    }
    let held = take_replacement(live.as_ref(), new);
    let old = if gave_back { Old::Held } else { Old::Returned };
    Decision::replace(held, old)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(target: u16, source: u16) -> Refs {
        Refs { target, source }
    }

    #[test]
    fn insert_takes_one_when_held_and_none_when_declined() {
        assert_eq!(refs_for(Event::Insert, Decision::insert(true)), refs(1, 0));
        // 못 붙잡았으면 참조를 받지 않는다. 나중에 놓아줄 것도 없다.
        assert_eq!(refs_for(Event::Insert, Decision::insert(false)), refs(0, 0));
    }

    #[test]
    fn delete_returns_now_only_what_never_reached_the_graph() {
        // 그래프에 있던 것: 들고 있다가 배리어 뒤에 sweeper가 놓는다.
        assert_eq!(
            refs_for(Event::Delete, Decision::delete(Old::Held)),
            refs(0, 0)
        );
        // `waiting`에서 지운 것: 아무도 역참조하지 않으니 엔진이 지금 회수한다.
        assert_eq!(
            refs_for(Event::Delete, Decision::delete(Old::Returned)),
            refs(1, 0)
        );
        // 안 잡던 것: 받은 적 없는 참조를 깎으면 안 된다.
        assert_eq!(
            refs_for(Event::Delete, Decision::delete(Old::NotOurs)),
            refs(0, 0)
        );
    }

    #[test]
    fn replace_takes_new_and_keeps_old_for_the_sweeper() {
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(true, Old::Held)),
            refs(1, 0)
        );
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(true, Old::Returned)),
            refs(1, 1)
        );
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(true, Old::NotOurs)),
            refs(1, 0)
        );
        // new를 못 잡았으면 엔진이 new를 올리지 않는다. old는 따로 정한다.
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(false, Old::Held)).target,
            0
        );
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(false, Old::Returned)),
            refs(0, 1)
        );
    }

    #[test]
    fn item_events_never_move_references() {
        for event in [Event::ItemLink, Event::ItemUnlink] {
            for decision in [
                Decision::NOTHING,
                Decision::insert(true),
                Decision::replace(true, Old::Returned),
            ] {
                assert_eq!(refs_for(event, decision), refs(0, 0), "{event:?}");
            }
        }
    }

    #[test]
    fn the_five_event_types_are_told_apart() {
        assert_eq!(
            Event::of(event_type_t_EVENT_ITEM_LINK),
            Some(Event::ItemLink)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ITEM_UNLINK),
            Some(Event::ItemUnlink)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ELEM_INSERT),
            Some(Event::Insert)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ELEM_DELETE),
            Some(Event::Delete)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ELEM_REPLACE),
            Some(Event::Replace)
        );
        assert_eq!(Event::of(9999), None, "모르는 값은 무시한다");
    }

    #[test]
    fn an_item_link_builds_the_index_from_the_map_value() {
        use crate::handler::quant::Quant;
        use crate::handler::usearch::Metric;

        let config = IndexConfig {
            dim: 4,
            metric: Metric::L2,
            quant: Quant::F32,
            m: 16,
            efc: 128,
            efs: 64,
        };
        // Map 값은 엔진이 붙인 CRLF까지 그대로 온다.
        let index = build_from_value("photos".to_owned(), &config.encode())
            .expect("설정 문서가 든 Map은 우리 인덱스다");
        assert_eq!(index.name, "photos");
        assert_eq!(index.ann.layout, config.layout());
        assert_eq!(index.ann.metric, Metric::L2);
        assert!(
            !index.is_rebuilding(),
            "Map이 요소보다 먼저 링크되므로 기다릴 것이 없다 -- 곧장 서빙한다"
        );
    }

    #[test]
    fn a_map_whose_value_is_not_a_config_is_not_ours() {
        // `mop create`로 만든 Map은 값이 CRLF뿐이다.
        assert!(build_from_value("plain".to_owned(), b"\r\n").is_none());
        assert!(build_from_value("plain".to_owned(), b"").is_none());
        assert!(build_from_value("plain".to_owned(), &crate::bson::EMPTY).is_none());
    }

    #[test]
    fn nothing_is_captured_outside_a_command() {
        // 복제 적용 스레드처럼 명령이 없는 곳에서는 넘길 대상이 없다.
        assert!(LINKED.with(|linked| linked.borrow().is_none()));
        let ((), linked) = capture(|| ());
        assert!(linked.is_empty());
        assert!(
            LINKED.with(|linked| linked.borrow().is_none()),
            "명령이 끝나면 칸을 내린다"
        );
    }
}
