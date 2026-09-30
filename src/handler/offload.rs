//! 무거운 명령을 풀에 넘기고, 답이 되면 코어가 깨워 마저 하게 한다.
//!
//! 세 스레드가 한 명령을 나눠 갖는다.
//!
//! ```text
//! [워커]  execute      waitfor_io_complete → 풀에 제출 → 응답 안 쓰고 반환
//!         block        대기 중이면 콜백을 돌려준다 → 코어가 conn_waking으로
//!                      상태를 바꾸고 연결을 이벤트 루프에서 뺀다
//! [풀]    work         무거운 일. **엔진 쓰기는 하지 않는다.**
//!                      결과를 채널에 넣고 notify_io_complete
//! [워커]  wake         conn_waking에서. 결과를 회수해 엔진 쓰기를 마치고
//!                      response_handler로 응답을 만든다
//! ```
//!
//! # 엔진 쓰기가 워커에 남는 이유
//!
//! 복제가 쓰기 훅에 **스레드 로컬**을 건다. `default_engine.c`의
//! `ACTION_BEFORE_WRITE`가 `rp_before_check`로 가고 그것이 `tls_cookie`를
//! 세우는데, 그 값을 보는 `rp_after_check`는 같은 스레드에서만 참이다. 풀
//! 스레드에서 `store`를 부르면 복제 훅을 그냥 지나친다.
//!
//! 읽기는 자유롭다. `ACTION_BEFORE_READ`는 `ENABLE_MIGRATION` 안에만 있고 그건
//! 꺼져 있다. 그래서 검색은 통째로 풀에서 돌고, `vadd`는 준비까지만 풀이 한다.
//!
//! # 쿠키를 들고 있어도 되는 이유
//!
//! 대기 중인 연결은 이벤트 루프에서 빠져 있어, 클라이언트가 끊어도 그것을
//! 알아차릴 주체가 없다 -- `conn_close`에 닿는 길은 상태 기계뿐이고 그것이 멈춰
//! 있다. 그래서 `notify_io_complete`이 깨울 때까지 conn은 살아 있고, 그 주소를
//! 표에 들고 있는 것이 안전하다.

use std::collections::HashMap;
use std::os::raw::c_void;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

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
    /// 워커가 이미 할당까지 마친 추가. 풀은 자리표만 넣는다.
    Add(cmd::AddPlan),
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
    /// `vadd`는 여기 없다. 아이템 할당이 먼저여야 하고 그것은 엔진 쓰기라
    /// 워커 스레드의 일이다 -- [`Task::adding`]이 그 뒤를 잇는다.
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
            // 할당은 워커가 이미 했다. 풀 몫은 자리표 삽입 하나다.
            Task::Add(mut plan) => {
                let staged = cmd::vadd_stage(&mut plan);
                Done::Prepared(staged.map(|()| plan))
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

impl Task {
    /// 워커가 할당을 마친 뒤, 풀에 넘길 추가 작업을 만든다.
    pub(crate) fn adding(plan: cmd::AddPlan) -> Self {
        Task::Add(plan)
    }
}

/// 풀이 끝낸 일. 검색은 답까지, 추가는 엔진 직전까지.
pub(crate) enum Done {
    /// 답이 다 됐다. 워커는 문자열만 만들면 된다.
    Answered(Result<Reply>),
    /// 엔진에 쓸 준비가 끝났다. 워커가 `vadd_commit`으로 마친다.
    Prepared(Result<cmd::AddPlan>),
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

/// 답을 기다리는 연결 하나.
struct Pending {
    handler: ResponseHandler,
    done: Receiver<Done>,
}

static WAITING: LazyLock<Mutex<HashMap<usize, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn waiting() -> MutexGuard<'static, HashMap<usize, Pending>> {
    WAITING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 명령을 풀에 넘긴다.
///
/// 못 넘기면 일감을 돌려주므로 호출자가 그 자리에서 처리하면 된다 -- 대기열이
/// 가득 찼다고 명령이 실패할 이유는 없다.
pub(crate) fn submit(
    cookie: *const c_void,
    handler: ResponseHandler,
    task: Task,
) -> std::result::Result<(), Task> {
    let key = cookie as usize;
    // 하나짜리 채널. 한 연결이 한 번에 한 명령이라 그 이상 필요 없고, 미리
    // 잡아두므로 풀 스레드가 보낼 때 할당하지 않는다.
    let (tx, rx) = sync_channel(1);

    {
        let mut waiting = waiting();
        if waiting.contains_key(&key) {
            // 이 연결은 이미 답을 기다리는 중이다. 프로토콜상 있을 수 없는데,
            // 덮어쓰면 앞의 것을 영영 못 깨우므로 여기서 처리하게 돌려준다.
            return Err(task);
        }
        if waiting.try_reserve(1).is_err() {
            return Err(task);
        }

        // **제출보다 먼저.** 이것이 세워져야 코어가 연결을 루프에서 뺀다.
        unsafe { server::waitfor_io_complete(cookie) };

        let work = Work {
            task,
            cookie: key,
            done: tx,
        };
        match pool::try_submit(Job::Offload(work)) {
            None => {
                waiting.insert(key, Pending { handler, done: rx });
                Ok(())
            }
            Some(Job::Offload(work)) => {
                // 못 넘겼다. 세워둔 대기를 도로 거둔다 -- 안 그러면 이 연결은
                // 오지 않을 notify를 기다린다.
                unsafe { server::notify_io_complete(cookie, 0) };
                Err(work.task)
            }
            Some(_) => unreachable!("try_submit hands back the job it was given"),
        }
    }
}

/// `execute`가 명령 하나를 여기 맡긴다.
///
/// `None`이면 풀에 넘겨 답을 나중에 준다는 뜻이고, `Some`이면 지금 답할
/// 결과다. **워커 스레드에서 돈다** -- `vadd`의 아이템 할당이 엔진 쓰기라
/// 반드시 여기서 일어나야 한다.
pub(crate) unsafe fn begin(
    cookie: *const c_void,
    handler: ResponseHandler,
    request: Request,
) -> Option<Result<Reply>> {
    // 추가는 할당이 먼저다. 주소가 정해져야 풀이 그 주소에 자리표를 넣는다.
    if let Request::Body(Body::Add(spec), bytes) = &request {
        let store = match unsafe { Store::for_cookie(cookie) } {
            Some(store) => store,
            None => return Some(Err(Error::Store(StoreError::Unavailable))),
        };
        return Some(match cmd::vadd_allocate(&store, spec, bytes) {
            Err(e) => Err(e),
            Ok(plan) => match submit(cookie, handler, Task::adding(plan)) {
                Ok(()) => return None,
                // 대기열이 가득 찼다. 자리표까지 이 스레드에서 넣는다.
                Err(task) => unsafe { run_here(task, cookie) },
            },
        });
    }

    match Task::from_request(request) {
        // 가벼운 명령은 그 자리에서. 넘기는 값이 일보다 비싸다.
        Err(request) => Some(unsafe { crate::handler::run(cookie, request) }),
        Ok(task) => match submit(cookie, handler, task) {
            Ok(()) => None,
            Err(task) => Some(unsafe { run_here(task, cookie) }),
        },
    }
}

/// 이 연결이 답을 기다리는 중인가. `block` 훅이 묻는다.
pub(crate) fn is_waiting(cookie: *const c_void) -> bool {
    waiting().contains_key(&(cookie as usize))
}

/// 기다리던 것을 거둔다. 깨어난 워커와 `abort`가 부른다.
fn take(cookie: *const c_void) -> Option<Pending> {
    waiting().remove(&(cookie as usize))
}

/// 연결이 사라졌다. 들고 있던 것을 버린다.
///
/// 풀이 아직 돌고 있을 수 있는데, 그쪽은 채널에 넣고 `notify_io_complete`을
/// 부를 뿐이다. 받는 쪽이 없으면 `send`가 조용히 실패하고, notify는 코어가
/// 닫힌 연결을 알아보고 넘긴다.
pub(crate) fn forget(cookie: *const c_void) {
    drop(take(cookie));
}

/// **`conn_waking`에서, 워커 스레드에서 돈다.**
///
/// 풀 결과를 회수하고, 엔진에 쓸 것이 남았으면 여기서 쓴다. 그다음
/// `response_handler`로 응답 문자열을 만든다.
pub(crate) unsafe fn wake(cookie: *const c_void) {
    let Some(pending) = take(cookie) else {
        // 코어가 콜백을 들고 있는 한 여기까지 오는데, 그 사이 `abort`가
        // 거둬갔다면 답할 것이 없다.
        return;
    };

    let outcome = match pending.done.recv() {
        Err(_) => Err(Error::Store(StoreError::Unavailable)),
        Ok(done) => unsafe { finish(done, cookie) },
    };

    Responder::new(pending.handler, cookie).reply(outcome);
}

/// 풀이 남긴 것을 응답으로 만든다. **엔진 쓰기가 여기서 일어난다.**
///
/// 워커 스레드에서만 불린다 -- 깨어난 `wake`가 부르거나, 풀에 못 넘겨
/// 그 자리에서 처리하는 `run_here`가 부른다.
unsafe fn finish(done: Done, cookie: *const c_void) -> Result<Reply> {
    match done {
        Done::Answered(reply) => reply,
        Done::Prepared(Err(e)) => Err(e),
        Done::Prepared(Ok(plan)) => match unsafe { Store::for_cookie(cookie) } {
            // 엔진 쓰기는 반드시 이 스레드에서. 모듈 주석의 `tls_cookie`.
            Some(store) => cmd::vadd_commit(&store, plan),
            None => Err(Error::Store(StoreError::Unavailable)),
        },
    }
}

/// 풀에 못 넘겼을 때, 이 스레드에서 끝까지 한다.
///
/// 워커 스레드이므로 읽기도 엔진 쓰기도 여기서 해도 된다.
pub(crate) unsafe fn run_here(task: Task, cookie: *const c_void) -> Result<Reply> {
    let store =
        unsafe { Store::for_cookie(cookie) }.ok_or(Error::Store(StoreError::Unavailable))?;
    let done = task.work(&store);
    unsafe { finish(done, cookie) }
}
