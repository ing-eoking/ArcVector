//! 엔진이 아이템 수명을 알려오는 콜백.
//!
//! 세 경우가 한 콜백으로 온다. `event_data_t`가 어느 경우인지와, 우리 답을 담을
//! 자리와, 관련된 아이템 포인터를 들고 온다.
//!
//! | | 우리가 받으면 | 우리가 거절하면 |
//! |---|---|---|
//! | LINK | `INCR(new)` | **아무것도 안 준다** |
//! | UNLINK | 우리가 놓아준다 | `DECR(old)` |
//! | REPLACE | `INCR(new)` | `INCR(new)` + `DECR(old)` |
//!
//! # 지켜야 하는 하나
//!
//! **우리가 LINK에서 받았다고 답한 아이템의 참조가, 들고 있는 것의 전부다.**
//! 거절하면 엔진은 참조를 주지 않는다.
//!
//! 그래서 넣지 못하면 거기서 끝낸다. 메모리가 없어 그래프에 못 넣든, 붙잡아 둘
//! 자리가 없든, 거절하고 손을 뗀다 -- 아이템은 저장소에 남지만 우리 것이 아니고,
//! 엔진이 평소처럼 evict한다. 들고 있지도 않은 것을 나중에 놓아주면 아직 쓰는
//! 아이템이 먼저 해제되고, 들고 있는 것을 안 놓아주면 프로세스가 끝날 때까지
//! 붙잡힌다.
//!
//! 그러려면 unlink에서 "이걸 내가 들고 있나"에 답할 수 있어야 한다. 장부를 따로
//! 두지 않고 이미 있는 것으로 답한다:
//!
//! | 무엇 | 들고 있나 | 무엇을 보고 아나 |
//! |---|---|---|
//! | 보류 중인 벡터 | 예 | `waiting`에 있다 |
//! | 등록된 인덱스의 메타 | 예 | 등록돼 있다는 것이 곧 링크에서 받았다는 뜻 |
//! | 그래프에 있던 벡터 | 예 | `unlink_at`이 `true` |
//! | 그 밖 | 아니오 | 링크에서 거절했다 |
//!
//! 마지막 줄이 참이려면 `vdrop`이 flush하는 동안 인덱스가 레지스트리에 남아
//! 있어야 한다. 접근은 `DRAINING`으로 막고 레지스트리에서 빼는 것은 flush 뒤로
//! 미룬다 -- 먼저 빼면 그 벡터들의 unlink가 그래프에 물어볼 곳을 잃는다.
//!
//! **이 함수는 엔진의 `cache_lock`을 쥔 채 불린다.** 아이템을 링크한 그 스레드에서
//! 온다 -- 클라이언트 쓰기면 워커, 복제본이면 복제 적용 스레드, 기동 시면 복구
//! 스레드다. 여기서 부를 수 있는 엔진 함수는 `get_item_info` 하나뿐이고, 나머지는
//! 전부 `cache_lock`을 다시 잡아 그 자리에서 데드락이다.

use std::os::raw::c_void;
use std::sync::Arc;

use crate::handler::access::sweep;
use crate::handler::arcus::element::MetaRecord;
use crate::handler::arcus::engine::{ItemElements, Store};
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::AnnIndex;
use crate::handler::usearch::metric::Metric;
use crate::trigger::waiting::{self, Unlinked};
use crate::trigger::recover;

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

/// 아이템의 키를 읽어 어느 인덱스의 무엇인지 알아낸다.
///
/// 두 번째 값이 `true`면 그 인덱스의 **메타 레코드**다 -- id가 빈 키다. 메타를
/// 벡터로 오해하면 본문이 벡터가 아니라 매번 거절하게 되고, 복제본에서는 인덱스가
/// 생겨야 할 바로 그 순간을 놓친다.
fn route(store: &Store, addr: u64) -> Option<(String, bool)> {
    if addr == 0 {
        return None;
    }
    store.with_item_at(addr, |key, _value| {
        crate::trigger::key::parse(key).map(|p| (p.index.to_owned(), p.id.is_empty()))
    })?
}

/// 아이템이 링크됐다. 그 벡터를 그래프에 넣는다.
///
/// 엔진이 우리 몫의 참조를 이미 잡아뒀다. 인덱스를 아직 모르면 -- 복제본에서
/// 메타보다 벡터가 먼저 도착한 경우다 -- 주소를 붙잡아 뒀다가 메타가 올 때
/// 복구가 넣는다.
fn on_link(store: &Store, addr: u64) -> Decision {
    let Some((name, is_meta)) = route(store, addr) else {
        // `ITEM_WITH_EVENT`는 키 앞 열한 바이트만 보고 서므로 우리 모양이 아닌
        // 키도 여기까지 온다. 받아두면 그 아이템의 unlink는 우리 것이 아니라고
        // 답할 테고, 참조는 프로세스가 끝날 때까지 남는다.
        return Decision::Declined;
    };
    if is_meta {
        return on_meta(store, addr, &name);
    }

    let Some(index) = registry::get(&name) else {
        if waiting::push(&name, addr) {
            return Decision::Took;
        }
        // LINK의 거절은 엔진이 보지 않으므로 여기서 참조를 돌려줄 길이 없다.
        // 이 아이템이 빠질 때 `on_unlink`이 거절해서 그때 회수된다.
        eprintln!(
            "ArcVector: no room to hold '{name}' vectors until its metadata arrives; \
             this one will be missing from the graph"
        );
        return Decision::Declined;
    };
    link_into(store, &index, addr)
}

/// 주소 하나를 그래프에 넣는다.
fn link_into(store: &Store, index: &VectorIndex, addr: u64) -> Decision {
    let layout = index.ann.layout;
    let took = store
        .with_item_at(addr, |_key, value| {
            layout
                .vector_of(value)
                .map(|vector| index.ann.link_node(addr, vector))
        })
        .flatten()
        .is_some_and(|r| r.is_ok());

    if took {
        Decision::Took
    } else {
        // 여기서 끝낸다. 참조를 안 받으므로 이 아이템은 우리 것이 아니고,
        // unlink는 그래프에 노드가 없는 것을 보고 놓아주지 않는다.
        eprintln!(
            "ArcVector: the graph would not take a linked item; it stays in the \
             store, out of this index"
        );
        Decision::Declined
    }
}

/// 인덱스의 메타 레코드가 링크됐다.
///
/// 이 노드의 `vcreate`가 쓴 것이면 레지스트리에 이미 있다 -- 그래프에 들어갈
/// 것이 없으니 그대로 받는다. 없으면 복제나 persistence 재생으로 들어온 것이고,
/// **그때가 이 인덱스가 이 프로세스에서 생기는 순간**이다. 복제본에는 `vcreate`가
/// 오지 않으므로 이 경로 말고는 인덱스가 생길 길이 없다.
///
/// 여기서는 등록까지만 하고 보류된 벡터는 풀 스레드가 넣는다. 이 함수는 엔진의
/// cache lock을 쥔 채 돌기 때문이다.
fn on_meta(store: &Store, addr: u64, name: &str) -> Decision {
    if registry::contains(name) {
        return Decision::Took;
    }

    let Some(index) = build(store, addr, name) else {
        return Decision::Declined;
    };
    let Ok((registered, previous)) = registry::put(index) else {
        eprintln!("ArcVector: the index registry could not grow; '{name}' stays unknown");
        return Decision::Declined;
    };
    sweep::retire(previous);

    if !recover::submit(&registered) {
        // 안전망은 sweeper다. 매 틱 `BUILDING`으로 남은 인덱스를 다시 맡긴다.
        eprintln!("ArcVector: '{name}' waits for a free worker before it can recover");
    }
    Decision::Took
}

/// 메타 본문으로 인덱스를 세운다. 아직 등록하지는 않는다.
///
/// `AnnIndex::new`가 usearch의 그래프를 잡으므로 cache lock 아래에서 도는
/// 할당이다. 인덱스 하나당 한 번뿐이라 감수한다 -- 벡터를 넣는 쪽이 비싼
/// 일이고, 그건 풀로 넘어간다.
fn build(store: &Store, addr: u64, name: &str) -> Option<VectorIndex> {
    let decoded = store
        .with_item_at(addr, |_key, value| MetaRecord::decode(value).ok())
        .flatten();
    let Some((meta, layout)) = decoded else {
        eprintln!("ArcVector: the metadata of '{name}' is unreadable; the index stays unknown");
        return None;
    };
    let Some(metric) = Metric::parse(&meta.metric) else {
        eprintln!(
            "ArcVector: the metadata of '{name}' names metric '{}', which this build \
             does not know",
            meta.metric
        );
        return None;
    };
    match AnnIndex::new(
        layout,
        metric,
        meta.connectivity,
        meta.expansion_add,
        meta.expansion_search,
        Arc::new(ItemElements),
    ) {
        Ok(ann) => Some(VectorIndex::building(name.to_owned(), ann, meta.maxcount)),
        Err(e) => {
            eprintln!("ArcVector: could not build the graph for '{name}': {e}");
            None
        }
    }
}

/// 아이템이 빠진다. 우리가 들고 있는 것이면 놓아준다.
///
/// 무엇을 들고 있는지는 이 모듈 머리의 표가 답한다.
fn on_unlink(store: &Store, addr: u64) -> Decision {
    let Some((name, is_meta)) = route(store, addr) else {
        return Decision::NotOurs;
    };

    match waiting::unlink(&name, addr) {
        // 그래프에 들어간 적은 없지만 참조는 우리 것이다. 거절하면 엔진이 회수한다.
        Unlinked::NeverLinked => return Decision::Declined,
        // 복구가 넣는 중이다. 받아두면 그쪽이 노드를 빼고 참조를 돌려준다.
        Unlinked::Recovering => return Decision::Took,
        Unlinked::NotWaiting => {}
    }

    let Some(index) = registry::get(&name) else {
        // 이 이름으로 등록된 인덱스가 없다. 링크 때 거절했거나 -- 메타를 못
        // 읽었거나 붙잡아 둘 자리가 없었거나 -- 애초에 받은 적이 없다는 뜻이라,
        // 놓아줄 참조가 없다. `vdrop`이 flush를 먼저 하는 것이 이 줄을 참으로
        // 만든다.
        return Decision::NotOurs;
    };

    if is_meta {
        // 인덱스가 등록돼 있다는 것이 이 메타를 링크에서 받았다는 뜻이다.
        // 그래프에 노드가 없으니 큐를 거칠 이유도 없다 -- 아무도 이 주소를
        // 역참조하지 않으므로 엔진이 그 자리에서 회수해도 된다.
        return Decision::Declined;
    }

    if !index.ann.unlink_at(addr) {
        // 그래프에 없었다. 링크 때 그래프가 안 받은 벡터라 참조도 받지 않았다.
        return Decision::NotOurs;
    }

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
/// REPLACE에서 거절은 "new를 못 받았다"가 아니라 **"old의 참조를 도로 가져가라"**
/// 하나뿐이다 -- 엔진은 `ret`과 무관하게 `INCR(new)`를 하고, `ret`이 실패일 때만
/// `DECR(old)`를 한다.
fn on_replace(store: &Store, old: u64, new: u64) -> Decision {
    let Some((name, is_meta)) = route(store, old) else {
        return Decision::NotOurs;
    };

    if is_meta {
        // 메타는 그래프에 노드가 없다. old를 들고 있었나는 그 인덱스가 등록돼
        // 있었나와 같다.
        if registry::contains(&name) {
            return Decision::Declined;
        }
        on_meta(store, new, &name);
        return Decision::NotOurs;
    }

    let waiting = waiting::unlink(&name, old);

    let Some(index) = registry::get(&name) else {
        // 메타가 아직 안 왔다. new를 old의 자리에 붙잡아 둔다.
        if !waiting::push(&name, new) {
            eprintln!("ArcVector: no room to hold a replaced '{name}' vector; it is dropped");
        }
        return Decision::Declined;
    };

    if waiting != Unlinked::NotWaiting {
        // old는 그래프에 들어간 적이 없다. new만 정상 경로로 넣는다. 못 넣으면
        // `link_into`가 로그를 남기고, new의 참조는 그 아이템이 빠질 때
        // `on_unlink`이 거절해서 회수된다.
        link_into(store, &index, new);
        return match waiting {
            // 복구가 old를 들고 있다. 그쪽이 빼고 돌려준다.
            Unlinked::Recovering => Decision::Took,
            // 아무도 안 들고 있다. 엔진이 회수한다.
            _ => Decision::Declined,
        };
    }

    if !index.ann.rename_node(old, new) {
        // 그래프가 old를 갖고 있지 않았다 -- 링크 때 안 받은 벡터라 참조도 없다.
        // new만 새로 넣는다. `INCR(new)`는 우리 답과 무관하게 이미 일어났으므로
        // 여기서 거절하면 없는 old의 참조를 회수하라는 말이 된다.
        link_into(store, &index, new);
        return Decision::NotOurs;
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
