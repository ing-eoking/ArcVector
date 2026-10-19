//! 명령의 답을 `on_io_complete` 콜백으로 쓴다. 무거운 명령은 풀에 넘긴다.
//!
//! 세 스레드가 한 명령을 나눠 갖는다.
//!
//! ```text
//! [워커]  execute      엔진 쓰기까지 끝낸다
//!                      waitfor_io_complete → on_io_complete(cb, Waiting) → 풀에 제출
//!                      → true 반환. 코어가 conn_io_callback으로 보내고 재운다
//! [풀]    work         무거운 일. **엔진 쓰기는 하지 않는다.**
//!                      결과를 채널에 넣고 notify_io_complete
//! [워커]  wake         conn_io_callback에서. `Waiting`을 되찾아
//!                      response_handler로 응답을 만든다. **엔진 쓰기는 하지 않는다**
//! ```
//!
//! 넘길 것이 없는 명령도 답은 같은 콜백으로 쓴다(`waitfor` 없이 등록하면 코어가
//! 재우지 않고 곧바로 부른다). 엔진 쓰기가 복제 ack를 기다리게 했으면 그
//! 기다림까지 끝난 뒤에 답이 나간다 -- 답하는 길이 하나다.
//!
//! # 엔진 쓰기가 `execute`에 남는 이유
//!
//! 복제가 쓰기 훅에 **스레드 로컬**을 건다. `default_engine.c`의
//! `ACTION_BEFORE_WRITE`가 `rp_before_check`로 가고 그것이 `tls_cookie`를
//! 세우는데, 그 값을 보는 `rp_after_check`는 같은 스레드에서만 참이다. 풀
//! 스레드에서 `store`를 부르면 복제 훅을 그냥 지나친다. 콜백에서 쓰면 그 쓰기가
//! 다시 기다려야 할 수 있는데, `conn_io_callback`은 그것을 지원하지 않는다.
//!
//! 읽기는 자유롭다. `ACTION_BEFORE_READ`는 `ENABLE_MIGRATION` 안에만 있고 그건
//! 꺼져 있다. 그래서 검색은 통째로 풀에서 돌고, `vadd`는 워커가 쓰고 난 뒤의
//! HNSW 삽입만 풀이 한다.
//!
//! # 쿠키를 들고 있어도 되는 이유
//!
//! 대기 중인 연결은 이벤트 루프에서 빠져 있어, 클라이언트가 끊어도 그것을
//! 알아차릴 주체가 없다 -- `conn_close`에 닿는 길은 상태 기계뿐이고 그것이 멈춰
//! 있다. 그래서 `notify_io_complete`이 깨울 때까지 conn은 살아 있고, 그 주소를
//! 들고 있는 것이 안전하다.

use std::os::raw::c_void;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use crate::command::Request;
use crate::command::filter::Filter;
use crate::command::request::{Body, Line, Sim, SimKey};
use crate::error::{Error, Reply, Result};
use crate::handler::access::pool::{self, Job};
use crate::handler::arcus::{Store, StoreError};
use crate::handler::cmd;
use crate::server::{self, Responder, ResponseHandler};

/// 풀이 들고 갈 수 있는 형태의 명령.
///
/// `Request`를 그대로 넘길 수 없어서 있다 -- `Line::SimKey`가 토큰 버퍼를
/// 빌려 쓰고, 그 버퍼는 `execute`가 돌아오면 다음 명령이 덮어쓴다. `Add`와
/// `Sim`은 이미 소유 타입이라 그대로 옮겨 온다.
pub(crate) enum Task {
    /// 워커가 이미 엔진에 쓴 추가. 풀은 그래프에 넣기만 한다.
    Add(cmd::Added),
    Sim(Sim, Vec<u8>),
    SimKey {
        index: String,
        key: String,
        k: usize,
        filter: Option<Filter>,
        with_attr: bool,
    },
}

impl Task {
    /// 풀에 넘길 만한 명령이면 소유 형태로 옮긴다.
    ///
    /// 검색은 순회가 길고, 추가는 양자화와 본문 구성이 있다. 나머지는 상수
    /// 시간에 가까워 넘기는 값이 오히려 비싸다.
    ///
    /// `vadd`는 여기 없다. 요소 쓰기가 먼저여야 하고 그것은 엔진 쓰기라
    /// 워커 스레드의 일이다 -- [`Task::Add`]가 그 뒤를 잇는다.
    pub(crate) fn from_request(request: Request) -> std::result::Result<Self, Request> {
        match request {
            Request::Body(Body::Sim(spec), bytes) => Ok(Task::Sim(spec, bytes)),
            Request::Line(Line::SimKey(spec)) => Ok(Task::SimKey {
                index: spec.index.to_owned(),
                key: spec.key.to_owned(),
                k: spec.k,
                filter: spec.filter,
                with_attr: spec.with_attr,
            }),
            other => Err(other),
        }
    }

    /// 엔진을 **읽기만** 하는 부분. 풀 스레드에서 돈다.
    fn work(self, store: &Store) -> Done {
        match self {
            // 쓰기는 워커가 이미 했다. 풀 몫은 HNSW 삽입이다.
            Task::Add(mut added) => {
                let inserted = cmd::vadd_insert(&mut added);
                Done::Inserted(added, inserted)
            }
            Task::Sim(spec, bytes) => Done::Answered(cmd::vsim_vector(store, &spec, &bytes)),
            Task::SimKey {
                index,
                key,
                k,
                filter,
                with_attr,
            } => {
                let spec = SimKey {
                    index: &index,
                    key: &key,
                    k,
                    filter,
                    with_attr,
                };
                Done::Answered(cmd::vsim_key(store, &spec))
            }
        }
    }
}

/// 풀이 끝낸 일. 검색은 답까지, 추가는 삽입까지.
pub(crate) enum Done {
    /// 답이 다 됐다. 워커는 문자열만 만들면 된다.
    Answered(Result<Reply>),
    /// 그래프에 넣었다. 콜백이 `vadd_reply`로 답한다.
    Inserted(cmd::Added, Result<()>),
}

/// 풀이 받아가는 일감.
pub(crate) struct Work {
    task: Task,
    cookie: usize,
    done: SyncSender<Done>,
}

impl Work {
    /// **풀 스레드에서 돈다.** 엔진 쓰기를 부르지 않는다.
    pub(crate) fn run(self) {
        // 읽기 전용이라 background 쿠키로 충분하다. 연결의 쿠키를 엔진에
        // 넘기지 않는 편이, 복제가 그것에 거는 스레드 가정과 엮이지 않아
        // 안전하다.
        let outcome = match Store::background() {
            Some(store) => self.task.work(&store),
            None => Done::Answered(Err(Error::Store(StoreError::Unavailable))),
        };

        // 보내고 나서 깨운다. 순서가 뒤집히면 깨어난 워커가 빈 채널을 본다.
        let _ = self.done.send(outcome);
        unsafe { server::notify_io_complete(self.cookie as *const c_void, 0) };
    }
}

/// 연결 하나가 답을 기다리는 상태. `on_io_complete`의 `cb_data`로 넘어간다.
///
/// `execute`가 `Box::into_raw`로 넘기고 [`wake`]가 `Box::from_raw`로 한 번
/// 되찾는다. 연결 슬롯에 두지 않는 것은 그럴 이유가 없어서다 -- 코어가 콜백과
/// 함께 들고 있다가 정확히 한 번 돌려준다.
pub(crate) enum Waiting {
    /// 답이 이미 정해졌다.
    Ready(Result<Reply>),
    /// 풀이 채널로 보낸다.
    Pool(Receiver<Done>),
}

/// 답을 쓸 콜백을 건다. 명령 하나에 한 번.
///
/// 돌려주는 포인터는 콜백이 돌기 전까지, 즉 `execute`가 돌아오기 전까지는 이
/// 스레드만 만진다.
unsafe fn register(cookie: *const c_void, waiting: Waiting) -> *mut Waiting {
    let raw = Box::into_raw(Box::new(waiting));
    unsafe { server::on_io_complete(cookie, wake, raw.cast::<c_void>()) };
    raw
}

/// 지금 정해진 답을 콜백으로 쓰게 한다.
pub(crate) unsafe fn reply(cookie: *const c_void, outcome: Result<Reply>) {
    unsafe { register(cookie, Waiting::Ready(outcome)) };
}

/// 명령을 풀에 넘기고, 답을 받을 콜백을 건다.
///
/// 대기열이 가득 차면 이 자리(워커)에서 끝까지 하고 답을 같은 콜백에 남긴다
/// -- 넘기지 못했다고 명령이 실패할 이유는 없다.
unsafe fn submit(cookie: *const c_void, task: Task) {
    // 하나짜리 채널. 한 연결이 한 번에 한 명령이라 그 이상 필요 없고, 미리
    // 잡아두므로 풀 스레드가 보낼 때 할당하지 않는다.
    let (tx, rx) = sync_channel(1);

    // **제출보다 먼저.** 풀이 먼저 끝내 notify가 앞서도 코어가 그 수를 센다.
    unsafe { server::waitfor_io_complete(cookie) };
    let waiting = unsafe { register(cookie, Waiting::Pool(rx)) };

    let work = Work {
        task,
        cookie: cookie as usize,
        done: tx,
    };
    let work = match pool::try_submit(Job::Offload(work)) {
        None => return,
        Some(Job::Offload(work)) => work,
        Some(_) => unreachable!("try_submit hands back the job it was given"),
    };

    // 못 넘겼다. 세워둔 기다림을 도로 거둔다 -- 안 그러면 이 연결은 오지 않을
    // notify를 기다린다. 엔진 쓰기가 건 기다림(복제 ack)은 그대로 남는다.
    unsafe { server::notify_io_complete(cookie, 0) };
    let outcome = unsafe { run_here(work.task, cookie) };
    // 콜백은 `execute`가 돌아온 뒤에야 돈다. 그때까지 이 상자는 이 스레드 것이다.
    unsafe { *waiting = Waiting::Ready(outcome) };
}

/// `execute`가 명령 하나를 여기 맡긴다. 답은 언제나 [`wake`]가 쓴다.
///
/// **워커 스레드에서 돈다** -- `vadd`의 아이템 할당이 엔진 쓰기라 반드시
/// 여기서 일어나야 한다.
pub(crate) unsafe fn begin(cookie: *const c_void, request: Request) {
    // 추가는 엔진 쓰기가 먼저다. 쓰는 동안 링크 콜백이 주소를 이 명령에게
    // 돌려주고, 풀이 그것을 그래프에 넣은 뒤에 답한다.
    if let Request::Body(Body::Add(spec), bytes) = &request {
        let outcome = match unsafe { Store::for_cookie(cookie) } {
            None => Err(Error::Store(StoreError::Unavailable)),
            Some(store) => match cmd::vadd_write(&store, spec, bytes) {
                Err(e) => Err(e),
                Ok(cmd::AddPlan::Answered(reply)) => Ok(reply),
                Ok(cmd::AddPlan::Insert(added)) => {
                    return unsafe { submit(cookie, Task::Add(added)) };
                }
            },
        };
        return unsafe { reply(cookie, outcome) };
    }

    match Task::from_request(request) {
        // 가벼운 명령은 그 자리에서. 넘기는 값이 일보다 비싸다.
        Err(request) => unsafe { reply(cookie, crate::handler::run(cookie, request)) },
        Ok(task) => unsafe { submit(cookie, task) },
    }
}

/// **`conn_io_callback`에서, 워커 스레드에서 돈다.** 이 연결이 기다리던 IO가
/// 모두 notify된 뒤다.
///
/// `execute`가 넘긴 `Waiting`을 되찾아 `response_handler`로 응답을 만든다.
/// 엔진 쓰기는 하지 않는다. 언제나 `true` -- 실패도 답으로 보낸다.
unsafe extern "C" fn wake(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool {
    if data.is_null() {
        return true;
    }
    let waiting = unsafe { Box::from_raw(data.cast::<Waiting>()) };

    let outcome = match *waiting {
        Waiting::Ready(outcome) => outcome,
        // 풀은 보내고 나서 notify하므로 여기 오면 이미 와 있다. 보낼 쪽이
        // 사라졌으면(`Err`) 답할 것이 없다.
        Waiting::Pool(done) => match done.recv() {
            Err(_) => Err(Error::Store(StoreError::Unavailable)),
            Ok(Done::Answered(reply)) => reply,
            Ok(Done::Inserted(added, inserted)) => cmd::vadd_reply(added, inserted),
        },
    };

    Responder::new(handler, cookie).reply(outcome);
    true
}

/// 풀에 못 넘겼을 때, 이 스레드(`execute`)에서 끝까지 한다.
///
/// 워커 스레드이므로 읽기도 엔진 쓰기도 여기서 해도 된다 -- 넣기가 실패한
/// `vadd`는 저장소에서도 지워 맞춘다.
pub(crate) unsafe fn run_here(task: Task, cookie: *const c_void) -> Result<Reply> {
    let store =
        unsafe { Store::for_cookie(cookie) }.ok_or(Error::Store(StoreError::Unavailable))?;
    match task.work(&store) {
        Done::Answered(reply) => reply,
        Done::Inserted(added, inserted) => cmd::vadd_finish(&store, added, inserted),
    }
}
