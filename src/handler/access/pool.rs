//! 요청 경로 밖의 일을 맡는 작은 스레드 풀.
//!
//! 지금 쓰는 곳은 복구 하나다. 메타 아이템이 링크되면 그 인덱스의 보류된 벡터를
//! 그래프에 넣어야 하는데, 그 콜백은 **엔진의 cache lock을 쥔 채** 불린다. 거기서
//! 수만 건을 삽입하면 데몬 전체가 그동안 선다. 그래서 콜백은 일감만 넘기고 곧장
//! 돌아오고, 삽입은 여기 스레드가 한다.
//!
//! 콜백에서 스레드를 그때그때 만들지 않는 이유도 같다 -- `pthread_create`는
//! 시스템 콜이고 실패할 수도 있어서, cache lock 아래에서 할 일이 아니다. 미리
//! 띄워두고 꺼내 쓴다.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};

use crate::handler::registry::VectorIndex;

/// 스레드 수.
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

/// 대기열 길이. 미리 잡아두고 자라지 않는다.
///
/// `submit`이 cache lock 아래에서 불리므로 여기서 `malloc`을 부르지 않는 것이
/// 목적이다 -- `retire.rs`의 링과 같은 이유다.
const QUEUE: usize = 256;

pub(crate) enum Job {
    /// 메타가 도착했다. 보류된 벡터를 그래프에 넣고 서빙으로 올린다.
    Recover(Arc<VectorIndex>),
    /// 서빙 중인 인덱스의 보류 목록을 비운다. 링크 콜백이 맡긴다.
    Adopt(Arc<VectorIndex>),
    /// 연결 하나가 답을 기다리는 명령. 끝나면 코어를 깨운다.
    Offload(crate::handler::offload::Work),
}

struct Pool {
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| {
    for n in 0..WORKERS {
        let spawned = std::thread::Builder::new()
            .name(format!("arcvector-work-{n}"))
            .spawn(run);
        if let Err(e) = spawned {
            eprintln!("ArcVector: could not spawn a worker thread: {e}");
        }
    }
    Pool {
        queue: Mutex::new(VecDeque::with_capacity(QUEUE)),
        wake: Condvar::new(),
    }
});

/// 풀이 안 떠 있으면 띄운다.
///
/// `ensure_sweeper`와 같은 이유로 적재 시점이 아니라 인덱스가 레지스트리에
/// 들어갈 때다: `-d`는 확장을 적재한 **뒤에** fork하므로 그 전에 만든 스레드는
/// 살아남지 못한다.
pub(crate) fn ensure_pool() {
    LazyLock::force(&POOL);
}

fn queue() -> MutexGuard<'static, VecDeque<Job>> {
    POOL.queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 일감을 맡긴다. 대기열이 가득 차 있으면 `false`.
///
/// 부른 쪽은 `false`를 흘려보내면 안 된다 -- 복구를 안 하면 그 인덱스는 `BUILDING`
/// 으로 남아 영영 안 보인다. sweeper가 매 틱 `BUILDING`을 다시 맡기는 것이 그
/// 안전망이다.
pub(crate) fn submit(job: Job) -> bool {
    try_submit(job).is_none()
}

/// [`submit`], but hands the job back when the queue is full.
///
/// A caller that can do the work itself needs the job returned rather than
/// dropped -- an offloaded command carries the only channel its connection
/// will ever be answered through, so losing it would park that connection
/// forever.
pub(crate) fn try_submit(job: Job) -> Option<Job> {
    let mut queue = queue();
    if queue.len() >= QUEUE {
        return Some(job);
    }
    queue.push_back(job);
    drop(queue);
    POOL.wake.notify_one();
    None
}

fn run() {
    loop {
        let job = {
            let mut queue = queue();
            loop {
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = POOL
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        match job {
            Job::Recover(index) => crate::trigger::recover::work(&index),
            Job::Adopt(index) => crate::trigger::recover::adopt_work(&index),
            Job::Offload(work) => work.run(),
        }
    }
}
