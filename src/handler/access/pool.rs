//! 요청 경로 밖의 일을 맡는 작은 스레드 풀.
//!
//! 처음 쓰인 곳은 복구다. 복제·재생으로 링크된 벡터를 그래프에 넣어야 하는데,
//! 그 콜백은 **엔진의 cache lock을 쥔 채** 불린다. 거기서 수만 건을 삽입하면
//! 데몬 전체가 그동안 선다. 그래서 콜백은 일감만 넘기고 곧장 돌아오고, 삽입은
//! 여기 스레드가 한다.
//!
//! 콜백에서 스레드를 그때그때 만들지 않는 이유도 같다 -- `pthread_create`는
//! 시스템 콜이고 실패할 수도 있어서, cache lock 아래에서 할 일이 아니다. 미리
//! 띄워두고 꺼내 쓴다.
//!
//! 스레드마다 대기열(레인)을 따로 둔다. 한 레인의 일은 넣은 순서대로 돌고,
//! 같은 (index, vkey)의 그래프 작업은 언제나 같은 레인으로 간다(`order`). 키가
//! 없는 일(검색, 복구)은 가장 한가한 레인을 쓴다.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};

use crate::handler::registry::VectorIndex;

/// 스레드 수. 스레드마다 레인(대기열) 하나다.
///
/// 복구는 인덱스 하나당 하나의 순차 작업이라 이 수가 곧 동시에 복구할 수 있는
/// 인덱스 수다. 인덱스가 많은 노드가 기동하거나 복제를 따라잡을 때 그 전부가
/// 한 줄로 서지 않도록 넉넉히 잡는다.
///
/// 노는 동안에는 전부 condvar에서 자고 있어 CPU를 쓰지 않는다. 값이 드러나는
/// 때는 실제로 그만큼의 인덱스가 동시에 복구될 때이고, 그때는 memcached 워커와
/// CPU를 다툰다 -- 복구 중인 인덱스는 아직 조회를 받지 않으므로 그 대가로 각
/// 인덱스가 더 빨리 서빙에 올라온다.
const WORKERS: usize = 32;

/// 레인 하나의 대기열 길이. 미리 잡아두고 자라지 않는다.
///
/// `submit`이 cache lock 아래에서 불리므로 여기서 `malloc`을 부르지 않는 것이
/// 목적이다 -- `retire.rs`의 링과 같은 이유다.
const QUEUE: usize = 256;

pub(crate) enum Job {
    /// 서빙 중인 인덱스의 보류 목록을 비운다. 링크 콜백이 맡긴다.
    Adopt(Arc<VectorIndex>),
    /// 연결 하나가 답을 기다리는 명령. 끝나면 코어를 깨운다.
    Offload(crate::handler::offload::Work),
    /// 테스트가 레인의 순서를 보려고 넣는 일.
    #[cfg(test)]
    Probe(Box<dyn FnOnce() + Send>),
}

/// 스레드 하나와 그 대기열.
///
/// 한 레인의 일은 그 스레드 하나가 넣은 순서대로 한다. 같은 키의 그래프 작업을
/// 한 레인에 모으는 것이 그 순서를 쓰는 길이다(`order`).
struct Lane {
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
    /// 대기 중인 일 + 도는 중인 일. 아무 레인이나 고를 때 가장 한가한 곳을
    /// 고르려고 락 없이 읽는다. 순간값이라 정확할 필요는 없다.
    load: AtomicUsize,
    /// 스레드가 떴는지. 안 뜬 레인에 넣으면 영영 돌지 않으므로 받지 않는다.
    alive: AtomicBool,
}

struct Pool {
    lanes: Box<[Lane]>,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| {
    let lanes: Box<[Lane]> = (0..WORKERS)
        .map(|_| Lane {
            queue: Mutex::new(VecDeque::with_capacity(QUEUE)),
            wake: Condvar::new(),
            load: AtomicUsize::new(0),
            alive: AtomicBool::new(false),
        })
        .collect();
    // 스레드는 POOL이 다 만들어질 때까지 `lane()`에서 기다렸다 들어온다.
    for (n, lane) in lanes.iter().enumerate() {
        let spawned = std::thread::Builder::new()
            .name(format!("arcvector-work-{n}"))
            .spawn(move || run(n));
        match spawned {
            Ok(_) => lane.alive.store(true, Ordering::Release),
            Err(e) => eprintln!("ArcVector: could not spawn a worker thread: {e}"),
        }
    }
    Pool { lanes }
});

/// 풀이 안 떠 있으면 띄운다.
///
/// `ensure_sweeper`와 같은 이유로 적재 시점이 아니라 인덱스가 레지스트리에
/// 들어갈 때다: `-d`는 확장을 적재한 **뒤에** fork하므로 그 전에 만든 스레드는
/// 살아남지 못한다.
pub(crate) fn ensure_pool() {
    LazyLock::force(&POOL);
}

/// 레인 수. 키 있는 일은 `해시 % lanes()`의 레인으로 간다.
pub(crate) const fn lanes() -> usize {
    WORKERS
}

fn lane(n: usize) -> &'static Lane {
    &POOL.lanes[n]
}

fn queue(lane: &Lane) -> MutexGuard<'_, VecDeque<Job>> {
    lane.queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 일감을 맡긴다. 대기열이 가득 차 있으면 `false`.
///
/// 부른 쪽은 `false`를 흘려보내면 안 된다 -- 맡기지 못한 adopt는 그 벡터들을
/// 그래프 밖에 둔다. sweeper가 매 틱 보류 목록이 남은 인덱스를 다시 맡기는 것이
/// 그 안전망이다.
pub(crate) fn submit(job: Job) -> bool {
    try_submit(job).is_none()
}

/// [`submit`], but hands the job back when the queue is full.
///
/// A caller that can do the work itself needs the job returned rather than
/// dropped -- an offloaded command carries the only channel its connection
/// will ever be answered through, so losing it would park that connection
/// forever.
///
/// 아무 레인이나 쓴다: 가장 한가한 레인을 고른다. 그 레인이 가득이면 나머지도
/// 거의 가득이라 돌려준다.
pub(crate) fn try_submit(job: Job) -> Option<Job> {
    let lanes = &POOL.lanes;
    let idlest = (0..lanes.len())
        .filter(|&n| lanes[n].alive.load(Ordering::Acquire))
        .min_by_key(|&n| lanes[n].load.load(Ordering::Relaxed));
    match idlest {
        Some(n) => submit_to(n, job),
        None => Some(job),
    }
}

/// `lane` 레인에 맡긴다. 가득 찼거나 그 레인의 스레드가 없으면 돌려준다.
///
/// 한 레인에 넣은 일은 넣은 순서대로 돈다.
pub(crate) fn submit_to(lane_no: usize, job: Job) -> Option<Job> {
    let Some(lane) = POOL.lanes.get(lane_no) else {
        return Some(job);
    };
    if !lane.alive.load(Ordering::Acquire) {
        return Some(job);
    }
    let mut queue = queue(lane);
    if queue.len() >= QUEUE {
        return Some(job);
    }
    queue.push_back(job);
    lane.load.fetch_add(1, Ordering::Relaxed);
    drop(queue);
    lane.wake.notify_one();
    None
}

fn run(n: usize) {
    let lane = lane(n);
    loop {
        let job = {
            let mut queue = queue(lane);
            loop {
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = lane
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // 패닉한 일 하나 때문에 레인 스레드가 죽으면 그 레인에 들어오는 일이 영영
        // 돌지 않는다. 일은 버리고 스레드는 산다.
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| match job {
            Job::Adopt(index) => crate::trigger::recover::adopt_work(&index),
            // 키 있는 일이면 그 키에 줄 선 일까지 이 스레드에서 잇는다.
            Job::Offload(work) => work.run(),
            #[cfg(test)]
            Job::Probe(f) => f(),
        }));
        if ran.is_err() {
            eprintln!("ArcVector: a job on worker lane {n} panicked; the lane keeps running");
        }
        lane.load.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;

    #[test]
    fn jobs_on_one_lane_run_in_submission_order() {
        ensure_pool();
        let (tx, rx) = channel();
        for n in 0..100 {
            let tx = tx.clone();
            let job = Job::Probe(Box::new(move || {
                // 순서가 섞일 틈을 준다. 다른 레인이 가져가면 여기서 앞지른다.
                if n % 7 == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let _ = tx.send(n);
            }));
            assert!(submit_to(3, job).is_none(), "lane 3 has room for 100");
        }
        let got: Vec<i32> = (0..100)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        assert_eq!(got, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn a_full_lane_hands_the_job_back() {
        ensure_pool();
        let (hold_tx, hold_rx) = channel::<()>();
        let (ran_tx, ran_rx) = channel::<()>();
        // 레인 스레드를 붙잡아 대기열이 줄지 않게 한다.
        let busy = Job::Probe(Box::new(move || {
            let _ = ran_tx.send(());
            let _ = hold_rx.recv();
        }));
        assert!(submit_to(5, busy).is_none());
        ran_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        for _ in 0..QUEUE {
            assert!(submit_to(5, Job::Probe(Box::new(|| {}))).is_none());
        }
        assert!(submit_to(5, Job::Probe(Box::new(|| {}))).is_some());
        drop(hold_tx);
    }

    #[test]
    fn there_are_as_many_lanes_as_workers() {
        assert_eq!(lanes(), WORKERS);
        assert!(submit_to(lanes(), Job::Probe(Box::new(|| {}))).is_some());
    }
}
