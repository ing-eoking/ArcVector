use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::engine_api::{ENGINE_ERROR_CODE, SERVER_CORE_API, SERVER_HANDLE_V1, rel_time_t};
use crate::error::{Reply, Result};

static GET_SERVER_API: OnceLock<unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1> = OnceLock::new();

pub fn set_api(f: unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1) {
    let _ = GET_SERVER_API.set(f);
}

pub fn handle() -> *mut SERVER_HANDLE_V1 {
    match GET_SERVER_API.get() {
        Some(get_api) => unsafe { get_api() },
        None => std::ptr::null_mut(),
    }
}

fn core() -> *const SERVER_CORE_API {
    let server = handle();
    if server.is_null() {
        return std::ptr::null();
    }

    unsafe { (*server).core }
}

/// 클라이언트가 말한 만료 시각을 서버의 시계로 옮긴다.
///
/// 프로토콜의 `exptime`은 "몇 초 뒤"(작은 값)이거나 유닉스 시각(큰 값)이고,
/// 0은 만료 없음이다. 아이템이 들고 있는 `rel_time_t`는 프로세스 시작 기준
/// 초라 둘이 다르다 -- 변환하지 않고 넘기면 `1`이 "기동 1초 후", 즉 이미
/// 지나간 시각이 된다.
pub fn realtime(exptime: u32) -> rel_time_t {
    if exptime == 0 {
        return 0;
    }
    let core = core();
    if core.is_null() {
        return 0;
    }
    match unsafe { (*core).realtime } {
        Some(convert) => unsafe { convert(i64::from(exptime)) },
        None => 0,
    }
}

/// Tells the core this connection has one more answer outstanding.
///
/// **Called before the work is handed to the pool, never after.** The core
/// only takes a connection out of the event loop when this count is above
/// zero, so a pool thread that finished first would otherwise leave the
/// connection parked with nothing left to wake it.
pub unsafe fn waitfor_io_complete(cookie: *const c_void) {
    let core = core();
    if core.is_null() {
        return;
    }
    if let Some(waitfor) = unsafe { (*core).waitfor_io_complete } {
        unsafe { waitfor(cookie) };
    }
}

/// One outstanding answer is ready.
///
/// The count is per connection (`MULTI_NOTIFY_IO_COMPLETE`): only when every
/// wait has been notified does the core put the connection back on its
/// thread's pending list and run the callback registered by
/// [`on_io_complete`], from `conn_io_callback`. Calling it before the
/// connection has actually parked just takes one off the count, which is also
/// how a wait that turned out not to be needed is withdrawn.
pub unsafe fn notify_io_complete(cookie: *const c_void, status: ENGINE_ERROR_CODE) {
    let core = core();
    if core.is_null() {
        return;
    }
    if let Some(notify) = unsafe { (*core).notify_io_complete } {
        unsafe { notify(cookie, status) };
    }
}

/// [`on_io_complete`]가 받는 콜백. 바인딩의 `AIO_CALLBACK`과 같은 모양이다.
///
/// 워커 스레드에서, 이 연결이 기다리던 IO가 모두 끝난 뒤 `conn_io_callback`이
/// 부른다. `false`를 돌려주면 코어가 연결을 닫는다.
pub type IoCallback = unsafe extern "C" fn(*const c_void, *mut c_void, ResponseHandler) -> bool;

/// 이 명령의 답을 쓸 콜백을 건다. 명령 하나에 한 번만 -- 코어가
/// `aiocb == NULL`을 assert한다.
///
/// `execute`가 `true`로 돌아오면 코어가 연결을 `conn_io_callback`으로 보낸다.
/// 기다리는 IO가 있으면(`waitfor_io_complete`) 그것이 모두 notify된 뒤에, 없으면
/// 곧바로 `cb(cookie, data, response_handler)`를 부른다. `data`는 그때까지
/// 살아 있어야 하고, 거두는 것은 `cb`의 몫이다.
///
/// 걸지 못하면 `false`. 그때 `data`는 아무도 거두지 않으므로 부른 쪽이 직접
/// 거두고, 답도 그 자리에서 해야 한다.
pub unsafe fn on_io_complete(cookie: *const c_void, cb: IoCallback, data: *mut c_void) -> bool {
    let core = core();
    if core.is_null() {
        return false;
    }
    match unsafe { (*core).on_io_complete } {
        Some(register) => {
            unsafe { register(cookie, Some(cb), data) };
            true
        }
        None => false,
    }
}

/// 이 크레이트가 부르는 `SERVER_CORE_API` 멤버 중 없는 것의 이름.
///
/// 멤버마다 `(이름, 있는가)`를 받는 순수 함수라 코어 없이 시험할 수 있다.
pub fn missing_members(members: &[(&'static str, bool)]) -> Vec<&'static str> {
    members
        .iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| *name)
        .collect()
}

/// 코어가 이 크레이트가 쓰는 멤버를 모두 갖췄는지 본다. 없는 멤버의 이름을
/// 돌려준다(비었으면 정상). 코어 자체가 없으면 `None`.
///
/// IO 대기 목록(`link_io_wait`/`next_io_wait`)은 vectest 갈래의 EE 서버에만
/// 있다.
pub fn missing_core_members() -> Option<Vec<&'static str>> {
    let core = core();
    if core.is_null() {
        return None;
    }
    let core = unsafe { &*core };
    Some(missing_members(&[
        ("waitfor_io_complete", core.waitfor_io_complete.is_some()),
        ("notify_io_complete", core.notify_io_complete.is_some()),
        ("on_io_complete", core.on_io_complete.is_some()),
        ("link_io_wait", core.link_io_wait.is_some()),
        ("next_io_wait", core.next_io_wait.is_some()),
        ("realtime", core.realtime.is_some()),
        ("store_engine_specific", core.store_engine_specific.is_some()),
        ("get_engine_specific", core.get_engine_specific.is_some()),
    ]))
}

/// `waiter`를 `owner`의 IO 대기 줄 끝에 잇는다.
///
/// `waiter`는 이미 `waitfor_io_complete`를 해 둔 상태여야 하고, 나중에
/// [`next_io_wait`]로 꺼낸 쪽이 `notify_io_complete`로 깨워야 한다.
///
/// 코어에 이 함수가 없으면 잇지 못하고 `false`. 그때 `waiter`를 깨울 쪽은
/// 부른 쪽이다.
pub unsafe fn link_io_wait(owner: *const c_void, waiter: *const c_void, data: *mut c_void) -> bool {
    let core = core();
    if core.is_null() {
        return false;
    }
    match unsafe { (*core).link_io_wait } {
        Some(link) => {
            unsafe { link(owner, waiter, data) };
            true
        }
        None => false,
    }
}

/// `owner`의 IO 대기 줄에서 맨 앞 커넥션과 그것을 이을 때 준 `data`를 꺼낸다.
/// 줄이 비었으면 `None`.
pub unsafe fn next_io_wait(owner: *const c_void) -> Option<(*const c_void, *mut c_void)> {
    let core = core();
    if core.is_null() {
        return None;
    }
    let next = unsafe { (*core).next_io_wait }?;
    let mut data = std::ptr::null_mut();
    let waiter = unsafe { next(owner, &mut data) };
    if waiter.is_null() {
        return None;
    }
    Some((waiter, data))
}

pub unsafe fn store_conn_state(cookie: *const c_void, data: *mut c_void) -> bool {
    let core = core();
    if core.is_null() {
        return false;
    }

    unsafe {
        match (*core).store_engine_specific {
            Some(store) => {
                store(cookie, data);
                true
            }
            None => false,
        }
    }
}

pub unsafe fn take_conn_state(cookie: *const c_void) -> *mut c_void {
    let core = core();
    if core.is_null() {
        return std::ptr::null_mut();
    }

    unsafe {
        let Some(get) = (*core).get_engine_specific else {
            return std::ptr::null_mut();
        };
        let data = get(cookie);
        if !data.is_null() {
            let _ = store_conn_state(cookie, std::ptr::null_mut());
        }
        data
    }
}

pub type ResponseHandler =
    Option<unsafe extern "C" fn(*const c_void, c_int, *const c_char) -> bool>;

pub struct Responder {
    handler: ResponseHandler,
    cookie: *const c_void,
}

impl Responder {
    pub fn new(handler: ResponseHandler, cookie: *const c_void) -> Self {
        Self { handler, cookie }
    }

    /// 답을 내보낸다.
    ///
    /// 핸들러가 NUL로 끝나는 C 문자열을 받으므로 한 번 복사해야 하는데, 그
    /// 복사가 답 전체 크기다 -- 검색이 `room`으로 조심해서 지은 답도 여기서
    /// 다시 한 번 그만큼 잡는다. 못 잡으면 `Vec::with_capacity`는 abort이므로,
    /// 자리를 재서 잡고 없으면 **아무것도 잡지 않는 짧은 답**으로 물러난다.
    /// 그 답은 상수라 할당이 필요 없다.
    pub fn send(&self, msg: &str) {
        let Some(handler) = self.handler else { return };

        let mut buf = Vec::new();
        if buf.try_reserve_exact(msg.len() + 1).is_err() {
            // 지을 자리가 없다. 연결을 끊지 않고 이것만 돌려준다.
            const SHORT: &[u8] = b"SERVER_ERROR out of memory\r\n\0";
            unsafe {
                handler(
                    self.cookie,
                    (SHORT.len() - 1) as c_int,
                    SHORT.as_ptr().cast::<c_char>(),
                );
            }
            return;
        }
        buf.extend_from_slice(msg.as_bytes());
        buf.push(0);

        unsafe {
            handler(
                self.cookie,
                msg.len() as c_int,
                buf.as_ptr().cast::<c_char>(),
            );
        }
    }

    pub fn reply(&self, outcome: Result<Reply>) {
        match outcome {
            Ok(reply) => self.send(reply.as_str()),
            Err(e) => self.send(&e.wire()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_only_the_absent_members_in_order() {
        let got = missing_members(&[("a", true), ("b", false), ("c", true), ("d", false)]);
        assert_eq!(got, ["b", "d"]);
    }

    #[test]
    fn nothing_is_missing_when_all_are_present() {
        assert!(missing_members(&[("a", true), ("b", true)]).is_empty());
        assert!(missing_members(&[]).is_empty());
    }
}
