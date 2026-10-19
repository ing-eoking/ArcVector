//! 엔진이 인덱스 Map과 그 요소의 수명을 알려오는 콜백.
//!
//! 여섯 경우가 한 콜백으로 온다. `event_data_t`가 어느 경우인지와, 우리 답을
//! 담을 자리와, 관련된 포인터를 들고 온다. 요소 이벤트는 그 요소가 속한 Map
//! 아이템(`coll_it`)도 함께 들고 온다 -- 요소에는 자기가 어느 Map 것인지 적혀
//! 있지 않아서, 그것이 어느 인덱스의 일인지 알 길이 그것뿐이다.
//!
//! 요소 쪽 참조 규칙은 다음과 같다.
//!
//! | | 우리가 받으면 | 우리가 거절하면 |
//! |---|---|---|
//! | LINK_ELEM | `refcount++(new)` | **아무것도 안 준다** |
//! | UNLINK_ELEM | 우리가 놓아준다 | `refcount--(old)` |
//! | REPLACE_ELEM | `refcount++(new)` | `refcount++(new)` + `refcount--(old)` |
//!
//! Map 아이템 쪽은 참조를 주고받지 않는다. LINK_ITEM은 아무것도 주지 않고,
//! UNLINK_ITEM에서 거절하면 엔진이 받은 적 없는 참조를 깎는다 -- 그래서 아이템
//! 이벤트에는 언제나 받았다고 답한다.
//!
//! # 지켜야 하는 하나
//!
//! **우리가 LINK에서 받았다고 답한 요소의 참조가, 들고 있는 것의 전부다.**
//! 거절하면 엔진은 참조를 주지 않는다.
//!
//! 그래서 붙잡아 둘 수 없으면 거기서 끝낸다. 요소는 저장소에 남지만 우리 것이
//! 아니다. 들고 있지도 않은 것을 나중에 놓아주면 아직 쓰는 요소가 먼저
//! 해제되고, 들고 있는 것을 안 놓아주면 Map이 지워져도 요소가 남는다.
//!
//! 그러려면 unlink에서 "이걸 내가 들고 있나"에 답할 수 있어야 한다. 장부를 따로
//! 두지 않고 이미 있는 것으로 답한다:
//!
//! | 무엇 | 들고 있나 | 무엇을 보고 아나 |
//! |---|---|---|
//! | 그래프에 들어가길 기다리는 벡터 | 예 | `waiting`에 있다 |
//! | 메타 요소 | 예 | 메타는 언제나 받는다 |
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
    ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_ENOMEM, ENGINE_EVENT_TYPE,
    ENGINE_EVENT_TYPE_ON_EVENT_ITEM, event_data_t, event_type_t, event_type_t_EVENT_LINK_ELEM,
    event_type_t_EVENT_LINK_ITEM, event_type_t_EVENT_REPLACE_ELEM, event_type_t_EVENT_REPLACE_ITEM,
    event_type_t_EVENT_UNLINK_ELEM, event_type_t_EVENT_UNLINK_ITEM,
};

/// 콜백이 무엇을 하기로 했는지. 디스패치를 엔진 포인터에서 떼어내 시험할 수 있게
/// 한다.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// 받았다. 엔진은 아무것도 되돌리지 않는다.
    Took,
    /// 못 받았다. 엔진이 참조를 회수한다.
    Declined,
    /// 우리 것이 아니다. 받을 것이 없다.
    NotOurs,
}

/// 엔진이 알려온 여섯 경우.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// Map이 링크됐다. 아직 메타가 없으니 할 일이 없다.
    LinkMap,
    /// Map이 빠진다. **이것이 인덱스가 사라지는 것이다.**
    UnlinkMap,
    /// Map이 교체된다. 컬렉션에는 일어나지 않는다.
    ReplaceMap,
    /// 요소가 링크됐다. 엔진이 우리 몫으로 참조를 잡아뒀다.
    Link,
    /// 요소가 빠진다. 링크 때 받은 참조를 계속 들지 말지 답한다.
    Unlink,
    /// 요소가 새것으로 교체된다. 그래프를 옮기고 old를 놓아준다.
    Replace,
}

impl Event {
    pub(crate) fn of(ty: event_type_t) -> Option<Self> {
        match ty {
            t if t == event_type_t_EVENT_LINK_ITEM => Some(Self::LinkMap),
            t if t == event_type_t_EVENT_UNLINK_ITEM => Some(Self::UnlinkMap),
            t if t == event_type_t_EVENT_REPLACE_ITEM => Some(Self::ReplaceMap),
            t if t == event_type_t_EVENT_LINK_ELEM => Some(Self::Link),
            t if t == event_type_t_EVENT_UNLINK_ELEM => Some(Self::Unlink),
            t if t == event_type_t_EVENT_REPLACE_ELEM => Some(Self::Replace),
            _ => None,
        }
    }
}

/// 결정을 엔진의 out 파라미터에 적는다.
///
/// 엔진이 `ENGINE_SUCCESS`로 초기화해 두고 오므로, 받았을 때는 손대지 않는다.
/// 우리 것이 아닐 때도 마찬가지다 -- 우리가 받은 것이 없으니 되돌릴 것도 없다.
pub(crate) fn answer(ret: &mut ENGINE_ERROR_CODE, decision: Decision) {
    if decision == Decision::Declined {
        *ret = ENGINE_ERROR_CODE_ENGINE_ENOMEM;
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
    let ev = unsafe { &*(event_data as *const event_data_t) };
    let Some(event) = Event::of(ev.type_) else {
        return;
    };

    let decision = unsafe { dispatch(event, ev) };

    if !ev.ret.is_null() {
        answer(unsafe { &mut *ev.ret }, decision);
    }
}

/// # Safety
///
/// `ev`의 포인터들은 엔진이 살려둔 것이고 이 호출 동안 유효하다.
unsafe fn dispatch(event: Event, ev: &event_data_t) -> Decision {
    let Some(store) = Store::background() else {
        // vtable이 아직 없다. 등록된 인덱스도 없으니 받을 것도 없다.
        return Decision::NotOurs;
    };

    match event {
        Event::LinkMap | Event::ReplaceMap => Decision::NotOurs,
        Event::UnlinkMap => {
            if let Some(name) = index_of(&store, ev.old_it as u64) {
                on_drop(&name);
            }
            // 아이템 이벤트의 거절은 받은 적 없는 참조를 깎는다. 언제나 받는다.
            Decision::NotOurs
        }
        Event::Link | Event::Unlink | Event::Replace => {
            let Some(name) = index_of(&store, ev.coll_it as u64) else {
                // `COLL_META_FLAG_EVENT`는 키 앞머리만 보고 서므로 우리 모양이
                // 아닌 키도 여기까지 온다. LINK는 거절해 참조를 받지 않는다.
                // 나머지는 받은 것이 없으니 엔진의 계정 그대로 둔다 -- REPLACE가
                // 넘겨준 new는 새는 쪽으로 틀린다.
                return if event == Event::Link {
                    Decision::Declined
                } else {
                    Decision::NotOurs
                };
            };
            match event {
                Event::Link => on_link(&store, &name, ev.new_it as u64),
                Event::Unlink => on_unlink(&store, &name, ev.old_it as u64),
                _ => on_replace(&store, &name, ev.old_it as u64, ev.new_it as u64),
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

/// 요소가 인덱스의 메타인가.
fn is_meta(store: &Store, elem: u64) -> bool {
    store
        .with_elem_at(elem, |field, _value| crate::trigger::key::is_meta(field))
        .unwrap_or(false)
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
/// 인덱스를 아직 모르면 -- 복제본에서 메타보다 벡터가 먼저 도착한 경우다 --
/// 주소만 적어 뒀다가 메타가 올 때 복구가 넣는다.
fn on_link(store: &Store, name: &str, addr: u64) -> Decision {
    if is_meta(store, addr) {
        on_meta(store, addr, name);
        return Decision::Took;
    }

    let Some(index) = registry::get(name) else {
        if waiting::push(name, addr) {
            return Decision::Took;
        }
        eprintln!(
            "ArcVector: no room to hold '{name}' vectors until its metadata arrives; \
             this one will be missing from the graph"
        );
        return Decision::Declined;
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
fn link_into(index: &Arc<VectorIndex>, addr: u64) -> Decision {
    if hand_to_command(index, addr, false) {
        return Decision::Took;
    }

    if !waiting::push(&index.name, addr) {
        // 적어 둘 자리가 없다. 참조를 안 받으므로 이 요소는 우리 것이 아니고,
        // unlink는 그래프에 노드가 없는 것을 보고 놓아주지 않는다.
        eprintln!(
            "ArcVector: no room to hold a linked '{}' vector; it stays in the \
             store, out of this index",
            index.name
        );
        return Decision::Declined;
    }

    if !recover::submit_adopt(index) {
        // 풀이 못 받았다. 목록에 남아 있고 sweeper가 매 틱 다시 맡긴다.
        eprintln!(
            "ArcVector: '{}' waits for a free worker before this vector is indexed",
            index.name
        );
    }
    Decision::Took
}

/// REPLACE가 우리 답과 무관하게 넘겨준 새 요소를 받아 둔다.
///
/// 붙잡아 둘 수 없으면 놓아줄 큐에 넣는다. 아직 링크된 요소를 놓아주는 것은
/// 안전하고, 그러면 이 요소의 unlink는 "우리 것이 아니다"로 읽힌다.
fn take_replacement(live: Option<&Arc<VectorIndex>>, name: &str, new: u64) {
    match live {
        Some(index) => {
            if link_into(index, new) == Decision::Declined && !index.ann.retire_one(new) {
                index.ann.halt_for_overflow();
            }
        }
        None => {
            if !waiting::push(name, new) {
                eprintln!("ArcVector: no room to hold a replaced '{name}' vector; it is dropped");
            }
        }
    }
}

/// 인덱스의 메타 요소가 링크됐다.
///
/// 이 노드의 `vcreate`가 쓴 것이면 레지스트리에 이미 있다 -- 그래프에 들어갈
/// 것이 없으니 서빙으로 올리기만 한다. 없으면 복제나 persistence 재생으로 들어온
/// 것이고, **그때가 이 인덱스가 이 프로세스에서 생기는 순간**이다. 복제본에는
/// `vcreate`가 오지 않으므로 이 경로 말고는 인덱스가 생길 길이 없다.
///
/// 메타 요소의 참조는 인덱스를 세웠든 못 세웠든 언제나 받는다. 그래야 unlink와
/// replace가 "들고 있었나"를 묻지 않고 돌려줄 수 있다.
fn on_meta(store: &Store, addr: u64, name: &str) {
    if let Some(index) = registry::get(name) {
        index.publish();
        index.mark_serving();
        return;
    }

    let Some(index) = build(store, addr, name) else {
        return;
    };
    let Ok((registered, previous)) = registry::put(index) else {
        eprintln!("ArcVector: the index registry could not grow; '{name}' stays unknown");
        return;
    };
    registry::keep_until_empty(previous);

    if !recover::submit(&registered) {
        // 안전망은 sweeper다. 매 틱 `BUILDING`으로 남은 인덱스를 다시 맡긴다.
        eprintln!("ArcVector: '{name}' waits for a free worker before it can recover");
    }
}

/// 메타 본문으로 인덱스를 세운다. 아직 등록하지는 않는다.
///
/// `AnnIndex::new`가 usearch의 그래프를 잡으므로 cache lock 아래에서 도는
/// 할당이다. 인덱스 하나당 한 번뿐이라 감수한다.
fn build(store: &Store, addr: u64, name: &str) -> Option<VectorIndex> {
    let decoded = store
        .with_elem_at(addr, |_field, value| IndexConfig::decode(value).ok())
        .flatten();
    let Some(config) = decoded else {
        eprintln!("ArcVector: the metadata of '{name}' is unreadable; the index stays unknown");
        return None;
    };
    // maxcount는 설정 문서에 없다. 이 콜백은 cache lock 아래라 Map 속성을 물을
    // 수도 없으므로 모르는 채로 0을 둔다 -- 한도는 엔진이 Map의 maxcount로 지킨다.
    match AnnIndex::new(
        config.layout(),
        config.metric,
        config.m,
        config.efc,
        config.efs,
        Arc::new(MapElements),
    ) {
        Ok(ann) => Some(VectorIndex::building(name.to_owned(), ann, 0)),
        Err(e) => {
            eprintln!("ArcVector: could not build the graph for '{name}': {e}");
            None
        }
    }
}

/// 요소가 빠진다. 우리가 들고 있는 것이면 놓아준다.
///
/// 무엇을 들고 있는지는 이 모듈 머리의 표가 답한다.
fn on_unlink(store: &Store, name: &str, addr: u64) -> Decision {
    if is_meta(store, addr) {
        // 메타는 언제나 받았다. 그래프에 노드가 없으니 아무도 이 주소를
        // 역참조하지 않는다 -- 엔진이 그 자리에서 회수해도 된다.
        //
        // 레지스트리는 건드리지 않는다. 메타 요소는 Map이 지워진 **뒤에**
        // 컬렉션 삭제 스레드가 지우는데, 그 사이 같은 이름으로 새 인덱스가
        // 생겼을 수 있다. 인덱스를 내리는 것은 Map의 unlink다.
        return Decision::Declined;
    }

    match waiting::unlink(name, addr) {
        // 그래프에 들어간 적은 없지만 참조는 우리 것이다. 거절하면 엔진이 회수한다.
        Unlinked::NeverLinked => return Decision::Declined,
        // 풀이 넣는 중이다. 받아두면 그쪽이 노드를 빼고 참조를 돌려준다.
        Unlinked::Recovering => return Decision::Took,
        Unlinked::NotWaiting => {}
    }

    let Some(index) = registry::holding(name, addr) else {
        // 어느 그래프도 이 주소를 갖고 있지 않다. 링크 때 거절했다는 뜻이라
        // 놓아줄 참조가 없다 -- 없는 것을 큐에 넣으면 안 받은 참조를 놓는다.
        return Decision::NotOurs;
    };

    index.ann.unlink_at(addr);
    if index.ann.retire_one(addr) {
        return Decision::Took;
    }

    index.ann.halt_for_overflow();
    Decision::Declined
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
/// REPLACE에서 거절은 "new를 못 받았다"가 아니라 **"old의 참조를 도로 가져가라"**
/// 하나뿐이다 -- 엔진은 `ret`과 무관하게 `refcount++(new)`를 하고, `ret`이
/// 실패일 때만 `refcount--(old)`를 한다. 그래서 new는 어느 길로든 받아 둔다.
fn on_replace(store: &Store, name: &str, old: u64, new: u64) -> Decision {
    if is_meta(store, old) {
        // 메타는 언제나 받았으니 old를 돌려준다. new는 엔진이 넘겨줬고, 메타의
        // 링크처럼 받아 둔다.
        on_meta(store, new, name);
        return Decision::Declined;
    }

    let live = registry::get(name);

    // 기다리던 벡터가 교체됐다. 자리를 그대로 물려받으므로 목록이 자라지 않고,
    // 그래서 이 경로는 자리가 없어 실패할 일이 없다.
    match waiting::replace(name, old, new, false) {
        Replaced::Swapped => {
            // new가 old의 자리에 들어갔다. 이 스레드의 명령이 쓴 것이면 그 명령
            // 몫으로 돌리고, 아니면 adopt에 맡긴다. old는 그래프에 없으니
            // 거절해서 엔진이 회수하게 한다.
            if let Some(index) = &live
                && !hand_to_command(index, new, true)
            {
                recover::submit_adopt(index);
            }
            return Decision::Declined;
        }
        // 풀이 old를 넣는 중이다. 그쪽이 빼고 돌려주고, new는 따로 잡아둔다.
        Replaced::Recovering => {
            take_replacement(live.as_ref(), name, new);
            return Decision::Took;
        }
        Replaced::NotWaiting => {}
    }

    let Some(holder) = registry::holding(name, old) else {
        // old는 링크 때 거절한 것이라 돌려줄 참조가 없다. new만 받아 둔다.
        take_replacement(live.as_ref(), name, new);
        return Decision::NotOurs;
    };
    let in_live = live.as_ref().is_some_and(|live| Arc::ptr_eq(live, &holder));

    // attr만 바뀌었다(`vsetattr`). 노드를 새 주소로 옮기면 끝이다 -- 다시 넣을
    // 이유가 없고, 그래프가 한순간도 이 벡터를 놓치지 않는다.
    if in_live && same_vector(store, &holder, old, new) && holder.ann.rename_node(old, new) {
        if holder.ann.retire_one(old) {
            return Decision::Took;
        }
        // old는 이미 그래프에 없다. 잠그는 것이 도는 역참조를 기다려 주므로
        // 엔진이 그 자리에서 회수해도 된다.
        holder.ann.halt_for_overflow();
        return Decision::Declined;
    }

    // 벡터가 바뀌었거나 old가 버려지는 중인 그래프에 있다. old를 빼고 new를
    // 새로 넣는다. `rename_node`로 끝내면 주소만 바뀌고 그래프는 옛 좌표를
    // 계속 들고 있다.
    holder.ann.unlink_at(old);
    let gave_back = holder.ann.retire_one(old);
    if !gave_back {
        holder.ann.halt_for_overflow();
    }
    take_replacement(live.as_ref(), name, new);
    if gave_back {
        Decision::Took
    } else {
        Decision::Declined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_to_decline_is_written_into_the_out_parameter() {
        let mut ret: ENGINE_ERROR_CODE = 0;
        answer(&mut ret, Decision::Declined);
        assert_eq!(ret, ENGINE_ERROR_CODE_ENGINE_ENOMEM);
    }

    #[test]
    fn taking_leaves_the_out_parameter_alone() {
        // 엔진이 SUCCESS로 초기화해 두고 온다. 받았으면 건드릴 이유가 없다.
        let mut ret: ENGINE_ERROR_CODE = 0;
        answer(&mut ret, Decision::Took);
        assert_eq!(ret, 0);
    }

    #[test]
    fn a_key_that_is_not_ours_is_left_alone() {
        let mut ret: ENGINE_ERROR_CODE = 0;
        answer(&mut ret, Decision::NotOurs);
        assert_eq!(ret, 0, "우리 것이 아니면 엔진의 계정을 건드리지 않는다");
    }

    #[test]
    fn the_six_event_types_are_told_apart() {
        assert_eq!(
            Event::of(event_type_t_EVENT_LINK_ITEM),
            Some(Event::LinkMap)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_UNLINK_ITEM),
            Some(Event::UnlinkMap)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_REPLACE_ITEM),
            Some(Event::ReplaceMap)
        );
        assert_eq!(Event::of(event_type_t_EVENT_LINK_ELEM), Some(Event::Link));
        assert_eq!(
            Event::of(event_type_t_EVENT_UNLINK_ELEM),
            Some(Event::Unlink)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_REPLACE_ELEM),
            Some(Event::Replace)
        );
        assert_eq!(Event::of(9999), None, "모르는 값은 무시한다");
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
