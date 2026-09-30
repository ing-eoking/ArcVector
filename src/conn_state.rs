//! 연결 하나가 명령 하나를 처리하는 동안 들고 있는 것.
//!
//! memcached가 연결마다 확장용 포인터 슬롯을 하나씩 준다
//! (`store_engine_specific` / `get_engine_specific`). 두 가지가 그 자리를
//! 번갈아 쓴다.
//!
//! ```text
//! accept   본문을 받을 자리를 잡는다        → Body
//! execute  본문을 꺼낸다 (자리가 빈다)
//!          무거우면 풀에 넘기고 표시한다     → Waiting
//! block    표시가 있나 들여다본다
//! wake     꺼내서 답한다                     (자리가 빈다)
//! ```
//!
//! **둘을 한 enum으로 묶은 것이 요점이다.** 슬롯은 태그 없는 `void*` 하나뿐이라,
//! 두 타입을 그때그때 넣으면 `abort`처럼 무엇이 들었는지 모르고 꺼내는 자리에서
//! 엉뚱한 타입으로 읽는다. 태그를 우리가 들고 있어야 한다.
//!
//! 이 슬롯을 만지는 것은 `accept`·`execute`·`block`·`wake`·`abort` 뿐이고,
//! 전부 **그 연결의 워커 스레드**에서 돈다. 풀 스레드는 채널과 쿠키만 들고
//! 있고 여기는 건드리지 않는다 -- 그래서 락이 없다.

use std::os::raw::c_void;

use crate::command::nread::Pending;
use crate::handler::offload::Waiting;
use crate::server;

pub enum ConnState {
    /// `accept`가 본문을 기다린다. 버퍼를 엔진에 빌려준 상태다.
    Body(Pending),
    /// 풀에 넘겼고 답을 기다린다.
    Waiting(Waiting),
}

/// 연결에 매단다. 슬롯을 못 쓰면 `false`이고 `state`는 그대로 버려진다.
///
/// 먼저 있던 것은 치운다. 한 연결이 한 번에 명령 하나를 처리하므로 겹칠 일이
/// 없지만, 남아 있었다면 그것은 주인을 잃은 것이라 여기서 끝내는 편이 낫다.
pub unsafe fn put(cookie: *const c_void, state: ConnState) -> bool {
    drop(unsafe { take(cookie) });

    let raw = Box::into_raw(Box::new(state));
    if unsafe { server::store_conn_state(cookie, raw.cast::<c_void>()) } {
        return true;
    }
    drop(unsafe { Box::from_raw(raw) });
    false
}

/// 매달린 것을 가져온다. 슬롯은 비워진다.
pub unsafe fn take(cookie: *const c_void) -> Option<Box<ConnState>> {
    let data = unsafe { server::take_conn_state(cookie) };
    if data.is_null() {
        return None;
    }
    Some(unsafe { Box::from_raw(data.cast::<ConnState>()) })
}

/// 답을 기다리는 중인가. **가져가지 않는다.**
pub unsafe fn is_waiting(cookie: *const c_void) -> bool {
    let data = unsafe { server::peek_conn_state(cookie) };
    if data.is_null() {
        return false;
    }
    // 빌려만 본다. 슬롯은 여전히 연결의 것이다.
    matches!(unsafe { &*data.cast::<ConnState>() }, ConnState::Waiting(_))
}

/// 본문만 가져온다. 다른 것이 들어 있으면 도로 넣어 둔다.
pub unsafe fn take_body(cookie: *const c_void) -> Option<Pending> {
    match unsafe { take(cookie) } {
        Some(state) => match *state {
            ConnState::Body(body) => Some(body),
            other => {
                let _ = unsafe { put(cookie, other) };
                None
            }
        },
        None => None,
    }
}

/// 대기 중인 것만 가져온다. 다른 것이 들어 있으면 도로 넣어 둔다.
pub unsafe fn take_waiting(cookie: *const c_void) -> Option<Waiting> {
    match unsafe { take(cookie) } {
        Some(state) => match *state {
            ConnState::Waiting(waiting) => Some(waiting),
            other => {
                let _ = unsafe { put(cookie, other) };
                None
            }
        },
        None => None,
    }
}
