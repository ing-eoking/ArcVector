//! 엔진이 아이템 수명을 알려오는 콜백.
//!
//! 세 경우가 한 콜백으로 온다. `event_data_t`가 어느 경우인지와, 우리 답을 담을
//! 자리와, 관련된 아이템 포인터를 들고 온다.
//!
//! | | 엔진이 콜백 전에 | `ret`의 뜻 | 우리가 거절하면 |
//! |---|---|---|---|
//! | LINK | `INCR(new)` | new를 받았나 | (지금 EE는 안 봄) |
//! | UNLINK | — | old를 계속 들 건가 | `DECR(old)` |
//! | REPLACE | — | new를 받았나 | `DECR(old)` |
//!
//! **이 함수는 엔진의 `cache_lock`을 쥔 채 불린다.** 아이템을 링크한 그 스레드에서
//! 온다 -- 클라이언트 쓰기면 워커, 복제본이면 복제 적용 스레드, 기동 시면 복구
//! 스레드다. 여기서 부를 수 있는 엔진 함수는 `get_item_info` 하나뿐이고, 나머지는
//! 전부 `cache_lock`을 다시 잡아 그 자리에서 데드락이다.

use std::os::raw::c_void;
use std::sync::Arc;

use crate::handler::arcus::engine::Store;
use crate::handler::registry::{self, VectorIndex};

use crate::engine_api::{
    ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_ENOMEM, ENGINE_EVENT_TYPE,
    ENGINE_EVENT_TYPE_ON_EVENT_ITEM, event_data_t, event_type_t, event_type_t_EVENT_LINK,
    event_type_t_EVENT_REPLACE, event_type_t_EVENT_UNLINK,
};

/// 콜백이 무엇을 하기로 했는지. 디스패치를 엔진 포인터에서 떼어내 시험할 수 있게
/// 한다.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// 받았다. 엔진은 아무것도 되돌리지 않는다.
    #[allow(dead_code)] // 세 핸들러가 아직 안 붙었다.
    Took,
    /// 못 받았다. 엔진이 참조를 회수한다.
    Declined,
    /// 우리 키가 아니거나 인덱스를 모른다. 받을 것이 없다.
    NotOurs,
}

/// 엔진이 알려온 세 경우.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// 아이템이 링크됐다. 엔진이 우리 몫으로 참조를 이미 잡아뒀다.
    Link,
    /// 아이템이 빠진다. 링크 때 받은 참조를 계속 들지 말지 답한다.
    Unlink,
    /// 아이템이 새것으로 교체된다. 그래프를 옮기고 old를 놓아준다.
    Replace,
}

impl Event {
    pub(crate) fn of(ty: event_type_t) -> Option<Self> {
        match ty {
            t if t == event_type_t_EVENT_LINK => Some(Self::Link),
            t if t == event_type_t_EVENT_UNLINK => Some(Self::Unlink),
            t if t == event_type_t_EVENT_REPLACE => Some(Self::Replace),
            _ => None,
        }
    }
}

/// 결정을 엔진의 out 파라미터에 적는다.
///
/// 엔진이 `ENGINE_SUCCESS`로 초기화해 두고 오므로, 받았을 때는 손대지 않는다.
/// 우리 키가 아닐 때도 마찬가지다 -- 우리가 받은 것이 없으니 되돌릴 것도 없다.
pub(crate) fn answer(ret: &mut ENGINE_ERROR_CODE, decision: Decision) {
    if decision == Decision::Declined {
        *ret = ENGINE_ERROR_CODE_ENGINE_ENOMEM;
    }
}

/// 콜백을 등록한다. `memcached_extensions_initialize`에서 한 번 부른다.
///
/// 그 시점은 getopt 루프 안이라 엔진 초기화보다 **앞**이고, 따라서 persistence
/// 복구가 아이템을 다시 링크하기 전이다. 그래서 복구가 만드는 link도 놓치지 않는다.
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
/// `ev`의 아이템 포인터는 엔진이 살려둔 것이고 이 호출 동안 유효하다.
unsafe fn dispatch(event: Event, ev: &event_data_t) -> Decision {
    let Some(store) = crate::handler::arcus::engine::Store::background() else {
        // vtable이 아직 없다. 등록된 인덱스도 없으니 받을 것도 없다.
        return Decision::NotOurs;
    };

    match event {
        Event::Link => on_link(&store, ev.new_it as u64),
        Event::Unlink => on_unlink(&store, ev.old_it as u64),
        Event::Replace => on_replace(&store, ev.old_it as u64, ev.new_it as u64),
    }
}

/// 키를 읽어 우리 인덱스를 찾는다. 우리 키가 아니거나 모르는 인덱스면 `None`.
fn index_of(store: &Store, addr: u64) -> Option<Arc<VectorIndex>> {
    if addr == 0 {
        return None;
    }
    let name = store.with_item_at(addr, |key, _value| {
        crate::trigger::key::parse(key).map(|p| p.index.to_owned())
    })??;
    registry::get(&name)
}

/// 아이템이 링크됐다. 그 벡터를 그래프에 넣는다.
///
/// 엔진이 우리 몫의 참조를 이미 잡아뒀다. 넣었으면 그대로 들고, 못 넣었으면
/// 거절해서 엔진이 회수하게 한다.
fn on_link(store: &Store, addr: u64) -> Decision {
    let Some(index) = index_of(store, addr) else {
        return Decision::NotOurs;
    };
    let layout = index.ann.layout;

    let Some(Some(())) = store.with_item_at(addr, |_key, value| {
        layout
            .vector_of(value)
            .map(|vector| index.ann.link_node(addr, vector))
            .and_then(|r| r.ok())
    }) else {
        eprintln!("ArcVector: the graph would not take a linked item; the engine reclaims it");
        return Decision::Declined;
    };
    Decision::Took
}

/// 아이템이 빠진다. 그래프에서 빼고, 참조를 sweeper에게 넘긴다.
///
/// 큐에 못 넣으면 그 주소를 잃는다 -- 그래프와 저장소가 어긋난 채로 남는다.
/// 그때는 인덱스를 잠그고 거절한다. 잠금이 **진행 중인 역참조가 끝나기를 기다린
/// 뒤에** 돌아오므로, 엔진이 그 자리에서 해제해도 읽고 있는 검색이 없다.
fn on_unlink(store: &Store, addr: u64) -> Decision {
    let Some(index) = index_of(store, addr) else {
        return Decision::NotOurs;
    };

    index.ann.unlink_at(addr);
    if index.ann.retire_one(addr) {
        return Decision::Took;
    }

    index.ann.halt_for_overflow();
    Decision::Declined
}

/// 아이템이 새것으로 교체된다.
///
/// **그래프를 먼저 옮긴다.** 엔진이 해시테이블을 바꾸기 전에 새 주소를 가리켜야,
/// 그 사이의 조회가 사라질 주소를 안 따라간다.
///
/// 옮긴 뒤 old를 큐에 넣는다. 못 넣으면 그래프에서 노드를 빼고 거절한다 --
/// 거절하면 엔진이 `INCR(new)`를 하지 않으므로, 그대로 두면 우리 것이 아닌
/// 포인터를 그래프가 들게 된다.
fn on_replace(store: &Store, old: u64, new: u64) -> Decision {
    let Some(index) = index_of(store, old) else {
        return Decision::NotOurs;
    };

    if !index.ann.rename_node(old, new) {
        // 그래프가 old를 갖고 있지 않았다. 새로 넣는 것과 같다.
        return on_link(store, new);
    }

    if index.ann.retire_one(old) {
        return Decision::Took;
    }

    index.ann.unlink_at(new);
    index.ann.halt_for_overflow();
    Decision::Declined
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
    fn the_three_event_types_are_told_apart() {
        assert_eq!(Event::of(event_type_t_EVENT_LINK), Some(Event::Link));
        assert_eq!(Event::of(event_type_t_EVENT_UNLINK), Some(Event::Unlink));
        assert_eq!(Event::of(event_type_t_EVENT_REPLACE), Some(Event::Replace));
        assert_eq!(Event::of(9999), None, "모르는 값은 무시한다");
    }
}
