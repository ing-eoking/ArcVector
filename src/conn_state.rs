//! 연결 하나가 `accept`와 `execute` 사이에 들고 있는 것.
//!
//! memcached가 연결마다 확장용 포인터 슬롯을 하나씩 준다
//! (`store_engine_specific` / `get_engine_specific`). 본문을 받는 동안만 쓴다.
//!
//! ```text
//! accept   본문을 받을 자리를 잡는다        → Body
//! execute  본문을 꺼낸다 (자리가 빈다)
//! abort    남은 것을 버린다
//! ```
//!
//! 답을 기다리는 상태는 여기 두지 않는다. `on_io_complete`의 `cb_data`로 넘기고
//! 콜백이 되찾는다(`offload`).
//!
//! 슬롯은 태그 없는 `void*` 하나뿐이라, 무엇이 들었는지는 이 enum이 들고 있다.
//! `abort`처럼 무엇이 들었는지 모르고 꺼내는 자리에서도 엉뚱한 타입으로 읽지
//! 않게 한다.
//!
//! 이 슬롯을 만지는 것은 `accept`·`execute`·`abort` 뿐이고, 전부 **그 연결의
//! 워커 스레드**에서 돈다 -- 그래서 락이 없다.

use std::os::raw::c_void;

use crate::command::nread::Pending;
use crate::server;

pub enum ConnState {
    /// `accept`가 본문을 기다린다. 버퍼를 엔진에 빌려준 상태다.
    Body(Pending),
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

/// 본문을 가져온다.
pub unsafe fn take_body(cookie: *const c_void) -> Option<Pending> {
    let state = unsafe { take(cookie) }?;
    match *state {
        ConnState::Body(body) => Some(body),
    }
}
