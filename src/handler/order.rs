//! 같은 (index, vkey)의 그래프 작업을 도착 순서대로 한 줄에 세운다.
//!
//! 엔진 쓰기는 `execute`에서 바로 하고(엔진 안에서 원자적), 줄을 서는 것은
//! 그 뒤의 **그래프 작업만**이다.
//!
//! ```text
//! [워커] begin    표가 비었으면 자기를 적고(Own) 그 키의 레인에 제출한다.
//!                 차 있으면 link_io_wait(owner, me, job)로 소유자 뒤에 잇는다(Queued).
//!                 풀에는 제출하지 않는다. waitfor/on_io_complete는 이미 해 둔 상태다
//! [풀]    finish  소유자의 작업을 끝내면 next_io_wait(owner)로 다음을 꺼내고,
//!                 남은 줄을 그쪽으로 옮기고, 소유를 넘긴다. 다음이 없으면 표에서 지운다
//! ```
//!
//! 두 단계 모두 샤드 락 안이다. 그래서 새로 온 대기자는 옮기기 전의 소유자에게
//! 이어져 함께 옮겨지거나, 옮긴 뒤의 새 소유자에게 이어진다 -- 중간에 빠지는
//! 일이 없다.
//!
//! 키는 `(index, vkey)`의 64비트 해시다. 두 키가 같은 해시를 가지면 같은 줄에
//! 서는 것뿐이라 정확성 문제는 없다(순서만 더 엄격하다). 레인도 같은 해시에서
//! 고르므로 그 둘은 레인도 같다.
//!
//! 엔진 이벤트와 그래프 작업 사이의 경합은 `waiting` 장부의 상태 전이가 처리한다.
//! 이 표는 순서를 정할 뿐 정확성을 대신하지 않는다.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::raw::c_void;
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use crate::handler::access::pool;
use crate::server;

/// 샤드 수. 키 해시의 아래 비트로 고른다.
const SHARDS: usize = 64;

/// [`begin`]의 결과.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Begin {
    /// 이 키의 그래프 작업을 이 연결이 가졌다. 호출자가 레인에 제출한다.
    Own,
    /// 소유자 뒤에 이었다. 소유자의 레인 스레드가 차례가 오면 이어서 한다.
    Queued,
}

/// 연결의 IO 대기 줄. 테스트가 FFI 대신 가짜를 넣을 수 있게 떼어 둔다.
///
/// 쿠키와 데이터는 주소를 정수로 들고 다닌다 -- 표가 `Send`/`Sync`여야 하고,
/// 그 주소를 이 모듈은 역참조하지 않는다.
pub(crate) trait Chain {
    /// `waiter`를 `owner`의 줄 끝에 잇는다. 잇지 못했으면 `false`.
    fn link(&self, owner: usize, waiter: usize, data: usize) -> bool;
    /// `owner`의 줄 맨 앞을 꺼낸다.
    fn next(&self, owner: usize) -> Option<(usize, usize)>;
}

/// 코어의 `link_io_wait`/`next_io_wait`.
struct Core;

impl Chain for Core {
    fn link(&self, owner: usize, waiter: usize, data: usize) -> bool {
        unsafe {
            server::link_io_wait(
                owner as *const c_void,
                waiter as *const c_void,
                data as *mut c_void,
            )
        }
    }

    fn next(&self, owner: usize) -> Option<(usize, usize)> {
        unsafe { server::next_io_wait(owner as *const c_void) }
            .map(|(waiter, data)| (waiter as usize, data as usize))
    }
}

/// 소유 표. `키 해시 → 지금 그 키의 그래프 작업을 가진 연결의 쿠키`.
pub(crate) struct Table {
    shards: Box<[Mutex<HashMap<u64, usize>>]>,
}

impl Table {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn shard(&self, key: u64) -> MutexGuard<'_, HashMap<u64, usize>> {
        self.shards[(key % SHARDS as u64) as usize]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 비었으면 `cookie`를 소유자로 적고, 차 있으면 소유자 뒤에 `job`과 함께 잇는다.
    ///
    /// 표가 자랄 자리를 못 잡거나 코어가 줄을 잇지 못하면 적지 않고 `Own`을 준다.
    /// 그 작업은 줄 없이 돌 뿐이고(뒤에 아무도 잇지 않는다), 정확성은 이벤트
    /// 장부가 지킨다 -- 순서를 못 지킨다고 데몬을 세우거나 연결을 재울 일은 아니다.
    pub(crate) fn begin<C: Chain>(&self, chain: &C, key: u64, cookie: usize, job: usize) -> Begin {
        let mut owners = self.shard(key);
        if let Some(&owner) = owners.get(&key) {
            return if chain.link(owner, cookie, job) {
                Begin::Queued
            } else {
                Begin::Own
            };
        }
        if owners.try_reserve(1).is_ok() {
            owners.insert(key, cookie);
        }
        Begin::Own
    }

    /// `owner`가 끝냈다. 다음 대기자와 그 job을 꺼내 소유를 넘긴다.
    ///
    /// 남은 줄은 다음 대기자에게 옮긴다 -- 줄은 연결 구조체에 붙어 있어,
    /// `owner`에 남겨 두면 깨어난 연결이 남의 대기자를 들고 다음 명령으로 간다.
    /// 그래서 이것이 돌아온 뒤 `owner`의 줄은 언제나 비어 있다.
    ///
    /// 표에 `owner`가 없으면(자리를 못 잡아 적지 않았던 경우) 아무것도 하지 않는다.
    pub(crate) fn finish<C: Chain>(
        &self,
        chain: &C,
        owner: usize,
        key: u64,
    ) -> Option<(usize, usize)> {
        let mut owners = self.shard(key);
        if owners.get(&key) != Some(&owner) {
            return None;
        }
        let Some((next, job)) = chain.next(owner) else {
            owners.remove(&key);
            return None;
        };
        while let Some((waiter, data)) = chain.next(owner) {
            // `next`가 나왔으니 코어에 줄이 있다. 잇기도 된다.
            let linked = chain.link(next, waiter, data);
            debug_assert!(linked, "a core that hands out waiters takes them too");
        }
        owners.insert(key, next);
        Some((next, job))
    }
}

static TABLE: LazyLock<Table> = LazyLock::new(Table::new);

/// `(index, vkey)`의 키 해시.
///
/// `DefaultHasher::new()`는 시드가 고정이라 프로세스 안에서 언제나 같은 값을
/// 준다. 문자열 해시는 끝에 구분 바이트를 넣으므로 `("ab", "c")`와 `("a", "bc")`가
/// 섞이지 않는다.
pub(crate) fn key_of(index: &str, vkey: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    index.hash(&mut hasher);
    vkey.hash(&mut hasher);
    hasher.finish()
}

/// 키 해시에서 고른 레인. 같은 키 해시는 언제나 같은 레인이다.
pub(crate) fn lane_for(key: u64) -> usize {
    (key % pool::lanes() as u64) as usize
}

/// 이 키의 그래프 작업이 도는 레인.
pub(crate) fn lane_of(index: &str, vkey: &str) -> usize {
    lane_for(key_of(index, vkey))
}

/// [`Table::begin`]을 전역 표와 코어의 줄로.
///
/// `job`은 대기자의 `Box<Work>`를 `into_raw`한 것이다. `Queued`이면 소유권이
/// 줄로 넘어가, 그것을 꺼낸 레인 스레드가 `from_raw`로 한 번 되찾는다.
///
/// # Safety
///
/// `cookie`는 이미 `waitfor_io_complete`와 `on_io_complete`를 마친, 살아 있는
/// 연결이어야 한다. 줄에 서면 그 연결은 소유자 쪽이 깨울 때까지 살아 있다.
pub(crate) unsafe fn begin(
    index: &str,
    vkey: &str,
    cookie: *const c_void,
    job: *mut c_void,
) -> Begin {
    TABLE.begin(&Core, key_of(index, vkey), cookie as usize, job as usize)
}

/// [`Table::finish`]를 전역 표와 코어의 줄로.
///
/// 이것이 돌아온 **뒤에** `owner`를 `notify_io_complete`로 깨워야 한다.
///
/// # Safety
///
/// `owner`는 이 키를 [`begin`]으로 가졌고 아직 깨우지 않은 연결이어야 한다.
pub(crate) unsafe fn finish(
    owner: *const c_void,
    index: &str,
    vkey: &str,
) -> Option<(*const c_void, *mut c_void)> {
    TABLE
        .finish(&Core, owner as usize, key_of(index, vkey))
        .map(|(next, job)| (next as *const c_void, job as *mut c_void))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /// 연결마다 줄 하나. 코어의 `io_wait_next` 목록 흉내.
    #[derive(Default)]
    struct Fake {
        lists: RefCell<HashMap<usize, VecDeque<(usize, usize)>>>,
        /// 코어에 `link_io_wait`가 없는 흉내.
        refuse: bool,
    }

    impl Fake {
        fn list(&self, owner: usize) -> Vec<usize> {
            self.lists
                .borrow()
                .get(&owner)
                .map(|l| l.iter().map(|(w, _)| *w).collect())
                .unwrap_or_default()
        }
    }

    impl Chain for Fake {
        fn link(&self, owner: usize, waiter: usize, data: usize) -> bool {
            if self.refuse {
                return false;
            }
            self.lists
                .borrow_mut()
                .entry(owner)
                .or_default()
                .push_back((waiter, data));
            true
        }

        fn next(&self, owner: usize) -> Option<(usize, usize)> {
            self.lists.borrow_mut().get_mut(&owner)?.pop_front()
        }
    }

    const KEY: u64 = 7;

    #[test]
    fn the_first_writer_owns_the_key() {
        let (table, chain) = (Table::new(), Fake::default());
        assert_eq!(table.begin(&chain, KEY, 100, 1100), Begin::Own);
        assert!(chain.list(100).is_empty());
    }

    #[test]
    fn a_second_writer_queues_behind_the_owner() {
        let (table, chain) = (Table::new(), Fake::default());
        assert_eq!(table.begin(&chain, KEY, 100, 1100), Begin::Own);
        assert_eq!(table.begin(&chain, KEY, 200, 1200), Begin::Queued);
        assert_eq!(chain.list(100), vec![200]);
    }

    #[test]
    fn finish_hands_the_key_to_the_next_in_arrival_order() {
        let (table, chain) = (Table::new(), Fake::default());
        table.begin(&chain, KEY, 100, 1100);
        for waiter in [1, 2, 3] {
            assert_eq!(
                table.begin(&chain, KEY, waiter, waiter + 1000),
                Begin::Queued
            );
        }

        assert_eq!(table.finish(&chain, 100, KEY), Some((1, 1001)));
        assert_eq!(table.finish(&chain, 1, KEY), Some((2, 1002)));
        assert_eq!(table.finish(&chain, 2, KEY), Some((3, 1003)));
        assert_eq!(table.finish(&chain, 3, KEY), None);

        // 다 끝나면 표에서 빠져, 다음 쓰기가 다시 소유한다.
        assert_eq!(table.begin(&chain, KEY, 400, 1400), Begin::Own);
    }

    #[test]
    fn the_rest_of_the_chain_moves_with_the_key() {
        let (table, chain) = (Table::new(), Fake::default());
        table.begin(&chain, KEY, 100, 1100);
        for waiter in [1, 2, 3] {
            table.begin(&chain, KEY, waiter, waiter + 1000);
        }

        assert_eq!(table.finish(&chain, 100, KEY), Some((1, 1001)));
        assert!(
            chain.list(100).is_empty(),
            "the finished owner keeps no waiters"
        );
        assert_eq!(chain.list(1), vec![2, 3]);

        // 넘긴 뒤에 온 대기자는 새 소유자 뒤에 선다.
        assert_eq!(table.begin(&chain, KEY, 4, 1004), Begin::Queued);
        assert_eq!(chain.list(1), vec![2, 3, 4]);
    }

    #[test]
    fn different_keys_do_not_wait_for_each_other() {
        let (table, chain) = (Table::new(), Fake::default());
        assert_eq!(table.begin(&chain, 1, 100, 1100), Begin::Own);
        assert_eq!(table.begin(&chain, 2, 200, 1200), Begin::Own);
        // 같은 샤드에 떨어지는 다른 키도.
        assert_eq!(
            table.begin(&chain, 1 + SHARDS as u64, 300, 1300),
            Begin::Own
        );
        assert!(chain.list(100).is_empty());
        assert_eq!(table.finish(&chain, 100, 1), None);
        assert_eq!(table.finish(&chain, 200, 2), None);
    }

    #[test]
    fn a_finish_from_a_connection_that_does_not_own_the_key_does_nothing() {
        let (table, chain) = (Table::new(), Fake::default());
        table.begin(&chain, KEY, 100, 1100);
        table.begin(&chain, KEY, 200, 1200);
        assert_eq!(table.finish(&chain, 999, KEY), None);
        assert_eq!(chain.list(100), vec![200]);
    }

    #[test]
    fn a_core_that_cannot_link_runs_the_writer_unordered_instead_of_parking_it() {
        let table = Table::new();
        let chain = Fake {
            refuse: true,
            ..Fake::default()
        };
        assert_eq!(table.begin(&chain, KEY, 100, 1100), Begin::Own);
        assert_eq!(table.begin(&chain, KEY, 200, 1200), Begin::Own);
        // 적힌 것은 처음 것뿐이다.
        assert_eq!(table.finish(&chain, 200, KEY), None);
        assert_eq!(table.finish(&chain, 100, KEY), None);
        assert_eq!(table.begin(&chain, KEY, 300, 1300), Begin::Own);
    }

    #[test]
    fn the_lane_follows_the_key_hash() {
        assert_eq!(key_of("idx", "a"), key_of("idx", "a"));
        assert_ne!(key_of("ab", "c"), key_of("a", "bc"));
        assert_eq!(lane_of("idx", "a"), lane_for(key_of("idx", "a")));
        assert!(lane_of("idx", "a") < pool::lanes());
    }
}
