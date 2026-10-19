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
//! 꺼져 있다. 그래서 검색은 통째로 풀에서 돌고, upsert/setmeta는 워커가 쓰고
//! 난 뒤의 HNSW 삽입만 그 키의 레인이 한다.
//!
//! # 쿠키를 들고 있어도 되는 이유
//!
//! 대기 중인 연결은 이벤트 루프에서 빠져 있어, 클라이언트가 끊어도 그것을
//! 알아차릴 주체가 없다 -- `conn_close`에 닿는 길은 상태 기계뿐이고 그것이 멈춰
//! 있다. 그래서 `notify_io_complete`이 깨울 때까지 conn은 살아 있고, 그 주소를
//! 들고 있는 것이 안전하다.

use std::os::raw::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use crate::command::Request;
use crate::command::request::{Body, Line, SearchKey, SearchVector};
use crate::error::{Error, Reply, Result};
use crate::handler::access::pool::{self, Job};
use crate::handler::arcus::{Store, StoreError};
use crate::handler::{cmd, order};
use crate::server::{self, Responder, ResponseHandler};

/// 풀이 들고 갈 수 있는 형태의 명령.
///
/// `Request`를 그대로 넘길 수 없어서 있다 -- `Line`이 토큰 버퍼를 빌려 쓰고, 그
/// 버퍼는 `execute`가 돌아오면 다음 명령이 덮어쓴다. 검색 요청은 이미 소유
/// 타입이라 본문 버퍼와 함께 그대로 옮겨 온다.
pub(crate) enum Task {
    /// 워커가 이미 엔진에 쓴 upsert/setmeta. 레인은 그래프에 넣기만 한다.
    Insert(cmd::Added),
    SearchKey(SearchKey, Vec<u8>),
    SearchVector(SearchVector, Vec<u8>),
}

impl Task {
    /// 풀에 넘길 만한 명령이면 소유 형태로 옮긴다.
    ///
    /// 검색은 순회가 길다. 나머지는 상수 시간에 가까워 넘기는 값이 오히려 비싸다.
    ///
    /// upsert와 setmeta는 여기 없다. 요소 쓰기가 먼저여야 하고 그것은 엔진 쓰기라
    /// 워커 스레드의 일이다 -- [`Task::Insert`]가 그 뒤를 잇는다.
    // 본문 구간 크기를 든 요청 때문에 `Request`가 커졌다.
    #[allow(clippy::result_large_err)]
    pub(crate) fn from_request(request: Request) -> std::result::Result<Self, Request> {
        match request {
            Request::Body(Body::SearchKey(spec), bytes) => Ok(Task::SearchKey(spec, bytes)),
            Request::Body(Body::SearchVector(spec), bytes) => Ok(Task::SearchVector(spec, bytes)),
            Request::Line(Line::SearchKey(spec)) => Ok(Task::SearchKey(spec, Vec::new())),
            other => Err(other),
        }
    }

    /// 엔진을 **읽기만** 하는 부분. 풀 스레드에서 돈다.
    fn work(self, store: &Store) -> Done {
        match self {
            // 쓰기는 워커가 이미 했다. 레인 몫은 HNSW 삽입이다.
            Task::Insert(mut added) => {
                let inserted = cmd::upsert_insert(&mut added);
                Done::Inserted(added, inserted)
            }
            Task::SearchKey(spec, body) => Done::Answered(cmd::search_key(store, &spec, &body)),
            Task::SearchVector(spec, body) => {
                Done::Answered(cmd::search_vector(store, &spec, &body))
            }
        }
    }
}

/// 풀이 끝낸 일. 검색은 답까지, upsert/setmeta는 삽입까지.
pub(crate) enum Done {
    /// 답이 다 됐다. 워커는 문자열만 만들면 된다.
    Answered(Result<Reply>),
    /// 그래프에 넣었다. 콜백이 `upsert_reply`로 답한다.
    Inserted(cmd::Added, Result<()>),
}

/// 풀이 받아가는 일감.
pub(crate) struct Work {
    task: Task,
    cookie: usize,
    done: SyncSender<Done>,
    /// 그래프 작업의 `(index, vkey)`. 있으면 같은 키의 작업과 한 줄에 선다
    /// (`order`). 검색은 없다.
    key: Option<(String, String)>,
}

impl Work {
    /// **풀 스레드에서 돈다.** 엔진 쓰기를 부르지 않는다.
    ///
    /// 키 있는 일이면 끝낸 뒤 그 키에 줄 선 다음 일을 꺼내 이 스레드에서
    /// 이어서 한다. 줄이 빌 때까지.
    pub(crate) fn run(self) {
        relay(
            self,
            Work::deliver,
            |owner, (index, vkey)| {
                // 꺼낸 job은 대기자가 `Box::into_raw`로 줄에 맡긴 `Work`다. 줄에서
                // 꺼낸 쪽이 한 번 되찾는다.
                unsafe { order::finish(owner as *const c_void, index, vkey) }
                    .map(|(_, job)| *unsafe { Box::from_raw(job.cast::<Work>()) })
            },
            |cookie| unsafe { server::notify_io_complete(cookie as *const c_void, 0) },
        );
    }

    /// 일을 하고 결과를 채널에 넣는다. 깨우지는 않는다 -- [`relay`]가 줄을
    /// 넘긴 뒤에 깨운다.
    fn deliver(self) {
        // 읽기 전용이라 background 쿠키로 충분하다. 연결의 쿠키를 엔진에
        // 넘기지 않는 편이, 복제가 그것에 거는 스레드 가정과 엮이지 않아
        // 안전하다.
        let outcome = match Store::background() {
            Some(store) => self.task.work(&store),
            None => Done::Answered(Err(Error::Store(StoreError::Unavailable))),
        };
        let _ = self.done.send(outcome);
    }
}

/// [`relay`]가 일감에서 보는 것.
trait Relayed {
    fn cookie(&self) -> usize;
    fn take_key(&mut self) -> Option<(String, String)>;
}

impl Relayed for Work {
    fn cookie(&self) -> usize {
        self.cookie
    }
    fn take_key(&mut self) -> Option<(String, String)> {
        self.key.take()
    }
}

/// 일 하나를 하고, 키가 있으면 그 줄의 다음 일로 넘어간다.
///
/// 순서가 정확성이다.
///
/// 1. `run` -- 결과를 그 연결의 채널에 넣는다. 실패한 일도 결과다.
/// 2. `finish` -- 키를 다음 대기자에게 넘긴다. **결과와 상관없이** 언제나.
///    안 넘기면 뒤에 선 연결들이 오지 않을 notify를 기다린다.
/// 3. `notify` -- 끝낸 연결을 깨운다. 줄을 넘긴 **뒤**라 깨어난 연결의 줄은
///    비어 있고, 결과를 넣은 뒤라 콜백이 빈 채널을 보지 않는다.
///
/// `waitfor_io_complete` 하나에 `notify` 하나가 모든 길에서 맞는다.
fn relay<W: Relayed, R>(
    first: W,
    mut run: impl FnMut(W) -> R,
    mut finish: impl FnMut(usize, &(String, String)) -> Option<W>,
    mut notify: impl FnMut(usize),
) {
    let mut work = first;
    loop {
        let cookie = work.cookie();
        let key = work.take_key();
        // 결과는 이미 채널로 갔다. 여기서 볼 것은 없다. 일이 패닉해도 넘기고
        // 깨운다 -- 보낼 쪽이 풀리며 채널이 닫혀, 깨어난 콜백은 오류로 답한다.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| run(work)));
        let next = key.as_ref().and_then(|key| finish(cookie, key));
        notify(cookie);
        match next {
            Some(next) => work = next,
            None => return,
        }
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
///
/// 코어가 콜백을 받지 않으면 상자를 여기서 거두고 `None`. 부른 쪽이 그 자리에서
/// 답해야 한다 -- 콜백이 영영 안 돌아도 클라이언트는 답을 받는다.
unsafe fn register(cookie: *const c_void, waiting: Waiting) -> Option<*mut Waiting> {
    let raw = Box::into_raw(Box::new(waiting));
    if unsafe { server::on_io_complete(cookie, wake, raw.cast::<c_void>()) } {
        Some(raw)
    } else {
        drop(unsafe { Box::from_raw(raw) });
        None
    }
}

/// 지금 정해진 답을 콜백으로 쓰게 한다.
///
/// 등록이 안 되면 `handler`로 곧바로 쓴다.
pub(crate) unsafe fn reply(cookie: *const c_void, outcome: Result<Reply>, handler: ResponseHandler) {
    // 등록이 실패하면 `outcome`은 상자와 함께 사라지므로, 먼저 복사해 둘 수는
    // 없다. 대신 실패한 상자에서 꺼내 쓴다.
    let raw = Box::into_raw(Box::new(Waiting::Ready(outcome)));
    if unsafe { server::on_io_complete(cookie, wake, raw.cast::<c_void>()) } {
        return;
    }
    if let Waiting::Ready(outcome) = *unsafe { Box::from_raw(raw) } {
        Responder::new(handler, cookie).reply(outcome);
    }
}

/// 명령을 풀에 넘기고, 답을 받을 콜백을 건다.
///
/// `key`가 있으면 그 키의 레인으로, 같은 키의 앞선 그래프 작업이 있으면 그
/// 뒤에 줄 세운다. 없으면 아무 레인이나.
///
/// 대기열이 가득 차면 이 자리(워커)에서 끝까지 하고 답을 같은 콜백에 남긴다
/// -- 넘기지 못했다고 명령이 실패할 이유는 없다.
unsafe fn submit(
    cookie: *const c_void,
    task: Task,
    key: Option<(&str, &str)>,
    handler: ResponseHandler,
) {
    // 하나짜리 채널. 한 연결이 한 번에 한 명령이라 그 이상 필요 없고, 미리
    // 잡아두므로 풀 스레드가 보낼 때 할당하지 않는다.
    let (tx, rx) = sync_channel(1);

    // **제출보다 먼저.** 풀이 먼저 끝내 notify가 앞서도 코어가 그 수를 센다.
    // 줄에 설 때도 먼저다 -- 소유자가 이어서 하고 깨운다.
    unsafe { server::waitfor_io_complete(cookie) };
    let Some(waiting) = (unsafe { register(cookie, Waiting::Pool(rx)) }) else {
        // 콜백을 못 걸었다. 세운 기다림을 거두고 여기서 끝까지 해 바로 답한다.
        unsafe { server::notify_io_complete(cookie, 0) };
        let outcome = unsafe { run_here(task, cookie) };
        Responder::new(handler, cookie).reply(outcome);
        return;
    };

    let work = Work {
        task,
        cookie: cookie as usize,
        done: tx,
        key: key.map(|(index, vkey)| (index.to_owned(), vkey.to_owned())),
    };
    if let Some((index, vkey)) = key {
        return unsafe { submit_keyed(cookie, work, index, vkey) };
    }

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

/// 키 있는 일. 소유하면 그 키의 레인에, 아니면 소유자 뒤에.
///
/// 레인이 가득이면 레인 스레드가 할 일을 이 자리에서 그대로 한다 -- 키를 이미
/// 가졌으므로 뒤에 선 일까지 이어서 하고 각자 깨운다. 따로 거둘 기다림은 없다:
/// 이 연결의 notify도 [`relay`]가 한다(`execute`가 돌아오기 전이라 수만 줄어든다).
unsafe fn submit_keyed(cookie: *const c_void, work: Work, index: &str, vkey: &str) {
    // 레인도 같은 해시에서 고른다. 해시가 겹친 두 키는 줄도 레인도 같다.
    let lane = order::lane_of(index, vkey);

    // 줄에 서면 소유자의 레인 스레드가 `Box::from_raw`로 한 번 되찾는다. 그 뒤로
    // 이 상자를 만지지 않으므로, 키는 상자 안의 것이 아니라 요청의 것을 쓴다.
    let job = Box::into_raw(Box::new(work));
    match unsafe { order::begin(index, vkey, cookie, job.cast::<c_void>()) } {
        order::Begin::Queued => {}
        order::Begin::Own => {
            let work = *unsafe { Box::from_raw(job) };
            match pool::submit_to(lane, Job::Offload(work)) {
                None => {}
                Some(Job::Offload(work)) => work.run(),
                Some(_) => unreachable!("submit_to hands back the job it was given"),
            }
        }
    }
}

/// 쓰기가 끝난 upsert/setmeta를 마무리한다. 그래프에 넣을 것이 있으면 그 키의
/// 레인으로 보내고, 없으면 지금 정해진 답을 콜백에 맡긴다.
unsafe fn after_write(
    cookie: *const c_void,
    index: &str,
    vkey: &str,
    plan: Result<cmd::UpsertPlan>,
    handler: ResponseHandler,
) {
    match plan {
        Err(e) => unsafe { reply(cookie, Err(e), handler) },
        Ok(cmd::UpsertPlan::Answered(r)) => unsafe { reply(cookie, Ok(r), handler) },
        // 그래프 작업은 같은 (index, vkey)끼리 도착 순서대로.
        Ok(cmd::UpsertPlan::Insert(added)) => unsafe {
            submit(cookie, Task::Insert(added), Some((index, vkey)), handler)
        },
    }
}

/// `execute`가 명령 하나를 여기 맡긴다. 답은 언제나 [`wake`]가 쓴다.
///
/// **워커 스레드에서 돈다** -- 엔진 쓰기(create, upsert, setmeta, delete, drop)가
/// 모두 여기서 일어나야 한다. 쓰기가 복제 ack를 기다리게 했어도 답은 콜백이
/// 쓰므로 ack 뒤에 나간다.
pub(crate) unsafe fn begin(cookie: *const c_void, request: Request, handler: ResponseHandler) {
    // upsert와 setmeta는 엔진 쓰기가 먼저다. 쓰는 동안 링크 콜백이 주소를 이
    // 명령에게 돌려주고, 레인이 그것을 그래프에 넣은 뒤에 답한다.
    match &request {
        Request::Body(Body::Upsert(spec), bytes) => {
            let plan = match unsafe { Store::for_cookie(cookie) } {
                None => Err(Error::Store(StoreError::Unavailable)),
                Some(store) => spec
                    .segments
                    .split(bytes)
                    .and_then(|parts| cmd::upsert_write(&store, spec, parts)),
            };
            return unsafe { after_write(cookie, &spec.index, &spec.vkey, plan, handler) };
        }
        Request::Body(Body::SetMeta(spec), bytes) => {
            let plan = match unsafe { Store::for_cookie(cookie) } {
                None => Err(Error::Store(StoreError::Unavailable)),
                Some(store) => spec
                    .segments
                    .split(bytes)
                    .and_then(|parts| cmd::setmeta_write(&store, spec, parts)),
            };
            return unsafe { after_write(cookie, &spec.index, &spec.vkey, plan, handler) };
        }
        _ => {}
    }

    match Task::from_request(request) {
        // 가벼운 명령은 그 자리에서. 넘기는 값이 일보다 비싸다.
        Err(request) => unsafe { reply(cookie, crate::handler::run(cookie, request), handler) },
        Ok(task) => unsafe { submit(cookie, task, None, handler) },
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
            Ok(Done::Inserted(added, inserted)) => cmd::upsert_reply(added, inserted),
        },
    };

    Responder::new(handler, cookie).reply(outcome);
    true
}

/// 풀에 못 넘겼을 때, 이 스레드(`execute`)에서 끝까지 한다.
///
/// 검색만 여기 온다. 넣기는 키 있는 길(`submit_keyed`)이라 레인이 가득이어도
/// 같은 `relay`를 탄다.
pub(crate) unsafe fn run_here(task: Task, cookie: *const c_void) -> Result<Reply> {
    let store =
        unsafe { Store::for_cookie(cookie) }.ok_or(Error::Store(StoreError::Unavailable))?;
    match task.work(&store) {
        Done::Answered(reply) => reply,
        // 어느 길이든 같은 하나의 동작이다: 엔진에 쓰지 않고 로그만 남긴다.
        Done::Inserted(added, inserted) => cmd::upsert_reply(added, inserted),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// 가짜 일감. `fails`면 작업 결과가 `Err`다.
    struct Fake {
        cookie: usize,
        key: Option<(String, String)>,
        fails: bool,
    }

    impl Relayed for Fake {
        fn cookie(&self) -> usize {
            self.cookie
        }
        fn take_key(&mut self) -> Option<(String, String)> {
            self.key.take()
        }
    }

    fn keyed(cookie: usize, fails: bool) -> Fake {
        Fake {
            cookie,
            key: Some(("idx".into(), "v".into())),
            fails,
        }
    }

    #[test]
    fn a_failed_job_still_hands_the_key_on() {
        let log = RefCell::new(Vec::new());
        relay(
            keyed(1, true),
            |job| {
                log.borrow_mut().push(format!("run {}", job.cookie));
                if job.fails { Err(()) } else { Ok(()) }
            },
            |owner, _key| {
                log.borrow_mut().push(format!("finish {owner}"));
                None
            },
            |cookie| log.borrow_mut().push(format!("notify {cookie}")),
        );
        assert_eq!(*log.borrow(), ["run 1", "finish 1", "notify 1"]);
    }

    #[test]
    fn relay_runs_the_chain_on_one_thread_and_wakes_each_owner_after_handing_on() {
        let log = RefCell::new(Vec::new());
        let mut rest = vec![keyed(3, false), keyed(2, true)];
        relay(
            keyed(1, false),
            |job| {
                log.borrow_mut().push(format!("run {}", job.cookie));
                if job.fails { Err(()) } else { Ok(()) }
            },
            |owner, key| {
                assert_eq!(key, &("idx".to_owned(), "v".to_owned()));
                log.borrow_mut().push(format!("finish {owner}"));
                rest.pop()
            },
            |cookie| log.borrow_mut().push(format!("notify {cookie}")),
        );
        assert_eq!(
            *log.borrow(),
            [
                "run 1", "finish 1", "notify 1", "run 2", "finish 2", "notify 2", "run 3",
                "finish 3", "notify 3",
            ]
        );
    }

    #[test]
    fn a_job_that_panics_still_hands_the_key_on_and_wakes_its_connection() {
        let log = RefCell::new(Vec::new());
        relay(
            keyed(1, true),
            |_| -> () { panic!("the graph insert blew up") },
            |owner, _key| {
                log.borrow_mut().push(format!("finish {owner}"));
                None
            },
            |cookie| log.borrow_mut().push(format!("notify {cookie}")),
        );
        assert_eq!(*log.borrow(), ["finish 1", "notify 1"]);
    }

    #[test]
    fn an_unkeyed_job_is_only_woken() {
        let log = RefCell::new(Vec::new());
        relay(
            Fake {
                cookie: 9,
                key: None,
                fails: false,
            },
            |_| Ok::<(), ()>(()),
            |owner, _| {
                log.borrow_mut().push(format!("finish {owner}"));
                None
            },
            |cookie| log.borrow_mut().push(format!("notify {cookie}")),
        );
        assert_eq!(*log.borrow(), ["notify 9"]);
    }
}
