//! 아이템 참조를 언제 엔진에 돌려줘도 되는지를 큐 순서로 답한다.
//!
//! 검색과 retire를 같은 FIFO에 넣으면 "누가 먼저였나"가 큐 순서 그 자체가 된다.
//! `Release`보다 앞에 있는 배리어는 그 주소가 `held`에서 빠지기 전에 시작한
//! 검색이므로 막아야 하고, 뒤에 있는 배리어는 빠진 뒤에 시작해 `resolve()`의
//! `held.contains()`에 걸리므로 막을 이유가 없다.
//!
//! 자세한 논증은 `docs/superpowers/specs/2026-09-16-arcvector-retire-barrier-queue-design.md`.

use crate::handler::usearch::held::Elements;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crate::error::{Error, Result};

/// 놓아줄 주소를 담는 링의 크기.
///
/// 기동 시 한 번 잡고 다시는 자라지 않는다. `retire`는 unlink 콜백에서 엔진의
/// cache lock을 쥔 채 불리므로, 그 경로에서 `malloc`을 부르지 않는 것이 목적이다.
///
/// 차는 것은 sweeper가 맨 앞 배리어에서 막혀 있는 동안 delete가 몰릴 때뿐이다.
/// 넘치면 그 배치를 포기하고 로그를 남긴다 -- 예전 `try_reserve` 실패와 같은
/// 처리다.
pub(super) const RING: usize = 4096;

/// 이벤트 칸을 `CAP`보다 이만큼 더 잡아둔다.
///
/// 게이트가 `CAP`에서 닫히면 새 검색이 안 붙으므로 그 뒤 retire는 빈 배리어를
/// 봉인하고 `Release`는 꼬리에 합쳐진다. 칸이 더 느는 것은 게이트 확인을 이미
/// 통과한 검색들 몫뿐이라, 이 여유면 실제로 닿지 않는다.
const EVENT_MARGIN: usize = 256;

/// `release`에 한 번에 넘기는 주소 수. 스택 버퍼라 힙을 안 쓴다.
const RELEASE_CHUNK: usize = 64;

/// 큐가 이만큼 차면 게이트를 닫는다. 고수위 표시이지 상한이 아니다.
pub(super) const CAP: usize = 1024;

/// 배리어 슬롯 수.
///
/// `wait_for_gate`는 `gated`를 확인하고 뮤텍스를 놓은 **뒤에** `attach`를 부른다.
/// 그 확인을 통과했지만 아직 붙지 않은 검색은 게이트가 그 사이 닫혀도 물러나지
/// 않으므로, 게이트가 닫힌 뒤에도 배리어가 하나만 더 생긴다는 보장은 없다 --
/// 실제 상한은 **그 순간 확인을 통과하고 아직 슬롯에 붙지 않은, 동시에 살아
/// 있는 검색의 수**다.
///
/// 이 코드베이스에서 그 수는 워커 스레드 수(`THREAD_SLOTS`, 64) 근방을 넘지
/// 않는다. `POOL - CAP - 1`칸의 여유를 다 태워 아직 큐에 있는 배리어를 감아서
/// 덮어쓰려면, 그 여유만큼의 검색이 정확히 이 좁은 창에 동시에 몰려야 한다 --
/// 수백에서 천 단위인데, 지금 스레드 수로는 닿지 않는다.
///
/// 그래도 이건 증명이 아니라 여유고, `retire()`의 `debug_assert`가 실제
/// tripwire다. 그게 걸리면 이 상수를 늘리거나 왜 배리어가 안 빠지는지부터 본다.
pub(super) const POOL: usize = CAP + 2;

/// 최상위 비트. 이 배리어는 봉인되어 새 검색을 받지 않는다.
const SEALED: usize = 1 << (usize::BITS - 1);

/// 나머지 비트. 붙어 있는 검색 수.
const COUNT: usize = !SEALED;

/// 큐에 들어가는 두 가지.
enum Event {
    /// 이 슬롯의 배리어가 풀릴 때까지 sweeper가 선다.
    Barrier(usize),
    /// 놓아줄 주소들이 링의 `[from, to)`에 있다. 위치는 단조 증가하고
    /// `% RING`으로 칸을 고른다.
    Release { from: usize, to: usize },
}

#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    /// 링에서 아직 안 놓아준 구간의 시작. `drain_once`가 다 놓은 뒤에 올린다.
    head: usize,
    /// 링에 다음으로 쓸 위치. `retire`가 올린다.
    tail: usize,
    gated: bool,
    /// 누군가 뮤텍스를 놓고 `release`를 부르는 중이다.
    ///
    /// 그 구간의 칸은 아직 큐에 남아 있으므로, 이 표시가 없으면 두 번째 배출자가
    /// 같은 칸을 집어 이중 해제를 낸다.
    draining: bool,
}

impl Queue {
    /// 자리가 났으면 게이트를 연다. 푸는 것은 배출자가 한다.
    fn reopen_if_room(&mut self, cap: usize) -> bool {
        if self.gated && self.events.len() < cap {
            self.gated = false;
            return true;
        }
        false
    }
}

/// 검색 하나를 슬롯에 붙인다. 봉인돼 있었으면 물러나고 `false`.
///
/// `fetch_add`의 반환값에 SEALED 비트가 원자적으로 실려 오는 것이 요점이다.
/// 플래그와 카운트를 다른 워드에 두면 이 확인과 `seal`의 확인이 서로를 못 보는
/// 실행이 허용되어(Dekker) `SeqCst`가 필요해진다.
fn attach(owner: &Retirement, cell: &AtomicUsize) -> bool {
    let prev = cell.fetch_add(1, Ordering::AcqRel);
    if prev & SEALED != 0 {
        let before_backout = cell.fetch_sub(1, Ordering::AcqRel);
        if before_backout & COUNT == 1 && before_backout & SEALED != 0 {
            // 내가 붙었다 물러나는 것으로 이 배리어의 카운트를 0으로 만들었다.
            // `Reading::drop`과 같은 모양의 검사다 -- sweeper가 이 배리어를
            // 기다리고 있을 수 있으므로 종을 울린다. 아무도 안 기다리고 있으면
            // (배리어가 아예 큐에 없으면) 헛울림이지만, `run()`의 다음 바퀴가
            // 놀 뿐 틀리지는 않는다.
            owner.ring();
        }
        return false;
    }
    true
}

/// 슬롯을 봉인하고, 봉인된 그 순간에 붙어 있던 검색 수를 돌려준다.
fn seal(cell: &AtomicUsize) -> usize {
    cell.fetch_or(SEALED, Ordering::AcqRel) & COUNT
}

/// `drain_once` 한 번이 무엇을 했는지.
pub(super) enum Progress {
    /// 항목 하나를 처리했다. 값은 놓아준 주소 수(배리어면 0).
    ///
    /// 운영 경로(`AnnIndex::drain_retired`)는 `Released(_)`로 값을 버린다 --
    /// 몇 개를 놓아줬는지가 아니라 뭔가 놓아줬다는 사실만 있으면 된다. 값을
    /// 들여다보는 것은 테스트뿐이라 `cfg(test)`가 아닌 빌드에서는 죽은
    /// 필드로 보인다.
    Released(#[cfg_attr(not(test), expect(dead_code))] usize),
    /// 머리의 배리어가 아직 안 풀렸다.
    Blocked,
    /// 큐가 비었다.
    Empty,
}

/// 게이트가 닫혀 있을 때 검색이 기다리는 시간. 넘으면 거절한다.
///
/// sweeper의 `TICK`(`sweep.rs`, 1초)의 두 배다. `retire()`가 게이트를 닫을 때마다
/// 이제 종을 울리므로(아래) 보통은 그걸로 sweeper가 바로 깨지만, 이 값이 `TICK`과
/// 같으면 그 종과 sweeper가 `wait_timeout(TICK)`에 막 들어가는 순간이 겹쳤을 때
/// 대기 중인 검색이 sweeper의 다음 바퀴를 한 번도 못 얻고 시간 초과로 `Busy`를
/// 받을 수 있었다. 두 배로 두면 종이 어떻게 엇갈리든 최소 한 바퀴는 보장된다.
const GATE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Retirement {
    slots: Box<[AtomicUsize]>,
    /// 놓아줄 주소의 링. 기동 시 한 번 잡고 다시 자라지 않는다.
    ///
    /// `AtomicU64`인 것은 배출자가 뮤텍스를 놓은 채 읽는 동안 `retire`가 다른
    /// 구간에 쓰기 때문이다. 겹치지는 않는다 -- 배출자가 읽는 `[head, to)`는
    /// `head`를 올리기 전이라 `retire`가 덮어쓰지 못한다.
    addrs: Box<[AtomicU64]>,
    /// 지금 열린 슬롯 번호. retire만 쓰고 검색은 읽기만 한다.
    open: AtomicUsize,
    queue: Mutex<Queue>,
    gate: Condvar,
    cap: usize,
    bell: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl Retirement {
    pub(super) fn new() -> Self {
        Self::with_cap_inner(CAP)
    }

    #[cfg(test)]
    pub(super) fn with_cap(cap: usize) -> Self {
        Self::with_cap_inner(cap)
    }

    fn with_cap_inner(cap: usize) -> Self {
        Self {
            slots: (0..POOL).map(|_| AtomicUsize::new(0)).collect(),
            addrs: (0..RING).map(|_| AtomicU64::new(0)).collect(),
            open: AtomicUsize::new(0),
            queue: Mutex::new(Queue {
                events: VecDeque::with_capacity(cap + EVENT_MARGIN),
                ..Queue::default()
            }),
            gate: Condvar::new(),
            cap,
            bell: OnceLock::new(),
        }
    }

    /// sweeper를 깨우는 방법을 알려준다. 등록 전에는 아무것도 하지 않는다.
    pub(super) fn set_bell(&self, bell: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.bell.set(bell);
    }

    /// 슬롯에서 떨어진다. 봉인된 배리어를 0으로 만들었으면 `true`.
    fn detach(&self, slot: usize) -> bool {
        let prev = self.slots[slot].fetch_sub(1, Ordering::AcqRel);
        prev & COUNT == 1 && prev & SEALED != 0
    }

    /// 종을 울린다. 봉인된 배리어를 0으로 만든 검색만 부른다.
    ///
    /// 알림 유실을 막는 것은 여기가 아니라 종 자체의 몫이다 -- sweeper가 기다리는
    /// 뮤텍스는 이 큐의 것이 아니라 sweeper 자신의 것이라, 여기서 큐 락을 거쳐봐야
    /// 아무것도 지켜주지 않는다. `sweep::wake`가 그 뮤텍스 안에서 플래그를 세운다.
    pub(super) fn ring(&self) {
        let Some(bell) = self.bell.get() else { return };
        bell();
    }

    /// 검색 하나를 지금 열린 배리어에 붙인다.
    ///
    /// 봉인 중이면 물러났다가 `open`을 다시 읽는다. 물러나는 것은 `held`를 읽기
    /// 전이므로 그 검색은 아무 주소도 보지 못했다.
    pub(super) fn enter(&self) -> Result<Reading<'_>> {
        self.enter_deadline(GATE_TIMEOUT)
    }

    pub(super) fn enter_deadline(&self, timeout: Duration) -> Result<Reading<'_>> {
        self.wait_for_gate(timeout)?;
        let mut spins = 0u32;
        loop {
            let slot = self.open.load(Ordering::Acquire);
            if attach(self, &self.slots[slot]) {
                return Ok(Reading { owner: self, slot });
            }
            spins += 1;
            if spins > 4 {
                // 봉인 중인 슬롯에 걸렸다. `open`을 넘기는 쪽은 이 큐 뮤텍스를
                // 쥔 채로 할당까지 할 수 있으므로(push_addrs), 몇 바퀴 돌고도
                // 안 풀리면 그 스레드를 놓아준다.
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
    }

    /// 게이트가 열릴 때까지 기다린다. 기다리는 동안 이 검색은 어떤 배리어에도
    /// 붙어 있지 않으므로, 진행 중인 검색들이 끝나 큐가 빠지는 것을 막지 않는다.
    fn wait_for_gate(&self, timeout: Duration) -> Result<()> {
        let q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if !q.gated {
            return Ok(());
        }
        let (q, wait) = self
            .gate
            .wait_timeout_while(q, timeout, |q| q.gated)
            .unwrap_or_else(PoisonError::into_inner);
        if wait.timed_out() && q.gated {
            return Err(Error::Busy);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn is_gated(&self) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .gated
    }

    #[cfg(test)]
    pub(super) fn open_slot(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn slot_count(&self, slot: usize) -> usize {
        self.slots[slot].load(Ordering::Acquire) & COUNT
    }

    /// 열린 슬롯을 봉인하고 다음 칸을 연다. 테스트에서 retire 없이 쓴다.
    ///
    /// 슬롯 넘김의 진짜 규칙은 `retire()`에 있다.
    #[cfg(test)]
    pub(super) fn seal_open(&self) -> usize {
        let slot = self.open.load(Ordering::Acquire);
        let live = seal(&self.slots[slot]);
        self.open.store((slot + 1) % POOL, Ordering::Release);
        live
    }

    /// 큐의 머리 하나를 처리한다.
    ///
    /// `release`는 **뮤텍스를 놓은 뒤에** 부른다. `release` → `do_item_release`가
    /// unlink 콜백을 재진입시킬 수 있고, 그 콜백이 같은 뮤텍스를 다시 잡는다.
    ///
    /// 그 창 동안 칸은 큐에 남아 있으므로 `draining`으로 다른 배출자를 막는다.
    /// 막지 않으면 둘이 같은 구간을 놓아 이중 해제가 난다.
    pub(super) fn drain_once(&self, elements: &dyn Elements) -> Progress {
        let (from, to) = {
            let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            if q.draining {
                return Progress::Blocked;
            }
            match q.events.front() {
                None => return Progress::Empty,

                Some(Event::Barrier(slot)) => {
                    let slot = *slot;
                    if self.slots[slot].load(Ordering::Acquire) & COUNT > 0 {
                        return Progress::Blocked;
                    }
                    // 봉인만 푼다. `store(0)`이면 물러나는 중인 검색의 `+1`을
                    // 덮어써 그 `-1`에서 언더플로가 난다.
                    self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
                    q.events.pop_front();
                    if q.reopen_if_room(self.cap) {
                        self.gate.notify_all();
                    }
                    return Progress::Released(0);
                }

                Some(Event::Release { to, .. }) => {
                    let span = (q.head, *to);
                    q.draining = true;
                    span
                }
            }
        };

        self.release_span(elements, from, to);

        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        q.draining = false;
        q.head = to;
        // 놓는 사이에 `retire`가 같은 칸에 더 붙였을 수 있다. 그러면 칸을 빼지
        // 않고 시작만 옮긴다 -- 빼면 아직 안 놓은 주소를 잃는다.
        match q.events.front_mut() {
            Some(Event::Release { from, to: tail_to }) if *tail_to > to => *from = to,
            _ => {
                q.events.pop_front();
            }
        }
        if q.reopen_if_room(self.cap) {
            self.gate.notify_all();
        }
        drop(q);

        Progress::Released(to - from)
    }

    /// 링의 `[from, to)`를 놓아준다. **어떤 락도 쥐지 않은 채** 불려야 한다.
    ///
    /// `head`가 아직 `from`이라 `retire`가 이 구간을 덮어쓰지 못한다. 스택 버퍼로
    /// 쪼개는 것은 힙을 안 쓰기 위해서다.
    fn release_span(&self, elements: &dyn Elements, from: usize, to: usize) {
        let mut chunk = [0u64; RELEASE_CHUNK];
        let mut pos = from;
        while pos < to {
            let n = RELEASE_CHUNK.min(to - pos);
            for (i, cell) in chunk[..n].iter_mut().enumerate() {
                *cell = self.addrs[(pos + i) % RING].load(Ordering::Acquire);
            }
            elements.release(&chunk[..n]);
            pos += n;
        }
    }

    /// 큐에 남은 `Release`를 전부 즉시 놓아주고 큐를 비운다. `Barrier`는 버린다.
    ///
    /// **`Drop for AnnIndex`에서만 부른다.** 검색은 `Arc<VectorIndex>`를 쥐고
    /// 도므로 그 `drop`이 불릴 수 있는 시점에는 어떤 검색도 존재하지 않는다 --
    /// 그래서 배리어가 무엇을 막고 있었든 더 이상 지킬 대상이 없고, 큐 순서를
    /// 거치지 않고 곧장 반납해도 §2의 순서 논증이 깨지지 않는다. 다른 어떤
    /// 자리에서도 이 가정은 성립하지 않는다.
    ///
    /// 큐에 이미 들어 있던 `Vec`들을 그대로 넘긴다 -- 새로 이어 붙이는 할당을
    /// 하지 않는다. `release`는 큐 뮤텍스를 놓은 뒤에 부른다: 재진입한 unlink
    /// 콜백이 같은 뮤텍스를 다시 잡을 수 있어서다(drain_once와 같은 이유).
    pub(super) fn release_pending(&self, elements: &dyn Elements) {
        let (from, to) = {
            let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            let span = (q.head, q.tail);
            q.events.clear();
            q.head = q.tail;
            span
        };
        self.release_span(elements, from, to);
    }

    /// 주소를 놓아줄 목록에 넣는다. 엔진을 건드리지 않는다.
    ///
    /// **호출 전에 그 주소가 `held`에서 빠져 있어야 한다.** 그래야 이 뒤에 시작한
    /// 검색이 `resolve()`에서 걸러져 그 주소에 닿지 못한다.
    ///
    /// 봉인과 push가 한 덩어리인 것은 retire 둘이 겹칠 때 순서가 뒤집히지 않게
    /// 하기 위해서다. `[B(b1), R(y), B(b), R(x)]`가 되면 b에 붙은 검색이 y를 들고
    /// 있을 수 있는데 `R(y)`를 막지 못한다.
    pub(super) fn retire(&self, addrs: &[u64]) {
        if addrs.is_empty() {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);

        // 자리부터 본다. 봉인한 뒤에 포기하면 되돌릴 것이 늘어난다.
        //
        // 칸은 둘까지 필요하다 -- 배리어 하나와 새 `Release` 하나. 꼬리 합치기가
        // 되면 덜 쓰지만, 여기서는 넉넉하게 잡는다.
        if q.tail - q.head + addrs.len() > RING || q.events.len() + 2 > q.events.capacity() {
            eprintln!(
                "ArcVector: the retirement queue is full; {} item reference(s) stay \
                 pinned until this process ends",
                addrs.len()
            );
            return;
        }

        let slot = self.open.load(Ordering::Acquire);
        // `seal()`을 단일 RMW로 두려고 이 확인은 별도의 `load`로 한다. 그 사이의
        // TOCTOU는 이 tripwire의 정밀도만 떨어뜨린다 -- 놓치는 쪽으로만 어긋나고,
        // 놓친 경우에도 뒤따르는 로직(언더플로, 이중 Barrier)이 그대로 문제를
        // 드러내므로 검증 목적을 해치지 않는다.
        let already_sealed = self.slots[slot].load(Ordering::Acquire) & SEALED != 0;
        let live = seal(&self.slots[slot]);
        debug_assert!(
            !already_sealed,
            "POOL이 감아서 아직 큐에 배리어가 남아 있는 슬롯 {slot}을 다시 봉인했다 \
             -- POOL을 늘리거나 그 배리어가 왜 안 빠졌는지부터 본다"
        );
        if live == 0 {
            // 아무도 안 붙었다. 봉인만 풀고 슬롯을 그대로 쓴다.
            //
            // `store(0)`이 아니라 `fetch_and`인 이유: 봉인과 이 줄 사이에 붙으려다
            // 물러나는 검색이 있을 수 있고, 그 `fetch_add`와 `fetch_sub` 사이에
            // 0을 덮어쓰면 뺄 때 언더플로가 난다. 플래그만 끈다.
            self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
        } else {
            // 배리어가 큐에 들어갔을 때만 슬롯이 넘어간다. 그래야 살아 있는
            // 배리어 수가 큐 안의 배리어 수와 같아져 슬롯이 감기지 않는다.
            q.events.push_back(Event::Barrier(slot));
            self.open.store((slot + 1) % POOL, Ordering::Release);
        }

        // 주소를 링에 쓴다.
        let from = q.tail;
        for (i, addr) in addrs.iter().enumerate() {
            self.addrs[(from + i) % RING].store(*addr, Ordering::Release);
        }
        let to = from + addrs.len();
        q.tail = to;

        // 꼬리 합치기. 링은 `tail`에 이어 붙으므로 방금 쓴 것이 앞엣것과 연속이다
        // -- 복사도 새 칸도 없이 `to`만 올리면 된다.
        match q.events.back_mut() {
            Some(Event::Release { to: tail_to, .. }) => *tail_to = to,
            _ => q.events.push_back(Event::Release { from, to }),
        }

        q.gated |= q.events.len() >= self.cap;
        drop(q);
        self.ring();
    }

    /// 링에서 아직 안 놓아준 주소 수.
    #[cfg(test)]
    pub(super) fn pending_addrs(&self) -> usize {
        let q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        q.tail - q.head
    }

    /// 배출이 진행 중인 것처럼 표시한다. `draining`이 실제로 막는지 보려고 쓴다.
    #[cfg(test)]
    pub(super) fn mark_draining(&self) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .draining = true;
    }

    #[cfg(test)]
    pub(super) fn queue_len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .len()
    }

    #[cfg(test)]
    pub(super) fn queue_shape(&self) -> Vec<char> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .iter()
            .map(|e| match e {
                Event::Barrier(_) => 'B',
                Event::Release { .. } => 'R',
            })
            .collect()
    }
}

/// 검색이 도는 동안 살아 있는 가드.
///
/// `slot`을 들고 다니는 이유: 봉인된 배리어는 큐로 가고 새 배리어가 열리므로,
/// 종료할 때 "지금 열린 배리어"에 내리면 엉뚱한 칸을 내리게 된다. 합계로 갈음할
/// 수도 없다 -- 검색은 끝나는 순서가 제각각이라, 나중 배리어의 검색이 앞 배리어의
/// 빚을 갚아버린다.
#[must_use = "the search is only counted while this is alive"]
pub(super) struct Reading<'a> {
    owner: &'a Retirement,
    slot: usize,
}

impl<'a> Reading<'a> {
    /// 검색이 끝났음을 알린다. 봉인된 배리어를 0으로 만들었으면 `true`.
    ///
    /// `true`를 받은 호출자는 큐를 조금 비우면 된다 -- sweeper를 기다리지 않고
    /// 그 자리에서 메모리가 돌아간다. 그래서 `Drop`과 달리 종을 울리지 않는다.
    /// 호출자가 비우다 상한에 걸리면 그때 울리면 된다.
    pub(super) fn leave(self) -> bool {
        let cleared = self.owner.detach(self.slot);
        std::mem::forget(self);
        cleared
    }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        if self.owner.detach(self.slot) {
            // 내가 마지막이고, 이 배리어는 큐에서 sweeper를 세우고 있다.
            self.owner.ring();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::Ordering;

    use crate::handler::usearch::held::Elements;

    #[derive(Default)]
    struct FakeElements {
        freed: StdMutex<Vec<u64>>,
    }

    impl Elements for FakeElements {
        fn id_at(&self, _addr: u64) -> Option<Arc<str>> {
            None
        }
        fn release(&self, addrs: &[u64]) {
            self.freed.lock().unwrap().extend_from_slice(addrs);
        }
    }

    impl FakeElements {
        fn freed(&self) -> Vec<u64> {
            self.freed.lock().unwrap().clone()
        }
    }

    #[test]
    fn an_unresolved_barrier_blocks_everything_behind_it() {
        let r = Retirement::new();
        let e = FakeElements::default();

        let reading = r.enter().unwrap();
        r.retire(&[0x10]);

        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        assert!(e.freed().is_empty(), "앞선 검색이 살아 있으면 못 놓아준다");

        drop(reading);
        assert!(
            matches!(r.drain_once(&e), Progress::Released(0)),
            "배리어를 버린다"
        );
        assert!(
            matches!(r.drain_once(&e), Progress::Released(1)),
            "그 다음이 릴리스"
        );
        assert_eq!(e.freed(), vec![0x10]);
    }

    #[test]
    fn a_full_address_ring_drops_the_batch_rather_than_growing() {
        let r = Retirement::new();
        let full: Vec<u64> = (0..RING as u64).collect();

        r.retire(&full);
        assert_eq!(r.pending_addrs(), RING, "링을 정확히 채웠다");
        assert_eq!(r.queue_len(), 1);

        r.retire(&[0xdead_beef]);
        assert_eq!(r.pending_addrs(), RING, "넘치는 배치는 포기한다");
        assert_eq!(r.queue_len(), 1, "칸도 안 늘어난다");
    }

    #[test]
    fn a_second_drainer_backs_off_while_one_is_releasing() {
        let r = Retirement::new();
        let e = FakeElements::default();
        r.retire(&[0x10]);

        // 첫 배출은 뮤텍스를 놓고 release를 부른다. 그 창에 다른 스레드가
        // 같은 칸을 또 놓아주면 이중 해제다.
        r.mark_draining();
        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        assert!(e.freed().is_empty(), "다른 쪽이 놓는 중이면 손대지 않는다");
    }

    #[test]
    fn an_empty_queue_reports_empty() {
        let r = Retirement::new();
        let e = FakeElements::default();
        assert!(matches!(r.drain_once(&e), Progress::Empty));
    }

    #[test]
    fn a_resolved_barrier_gives_its_slot_back() {
        let r = Retirement::new();
        let e = FakeElements::default();

        let reading = r.enter().unwrap();
        let slot = r.open_slot();
        r.retire(&[0x10]);
        drop(reading);

        assert!(matches!(r.drain_once(&e), Progress::Released(0)));

        // 카운트가 0인 것만으로는 부족하다 -- slot_count가 SEALED를 가리므로,
        // fetch_and를 아예 안 해도 통과한다. 봉인이 실제로 풀렸는지 본다.
        assert!(
            attach(&r, &r.slots[slot]),
            "봉인이 안 풀렸다. 이 칸이 다시 열리면 enter()가 영영 돈다"
        );
        r.slots[slot].fetch_sub(1, Ordering::AcqRel);
        assert_eq!(r.slot_count(slot), 0, "깨끗하게 되돌아왔다");
    }

    #[test]
    fn a_barrier_behind_a_release_does_not_block_it() {
        let r = Retirement::new();
        let e = FakeElements::default();

        // 먼저 붙은 검색이 첫 배리어를 만든다.
        let first = r.enter().unwrap();
        r.retire(&[0x10]);

        // 이 검색은 0x10이 held에서 빠진 뒤에 시작했으므로 그 주소에 닿을 수 없고,
        // 그래서 두 번째 배리어에 붙는다 -- Rel(0x10) 뒤에.
        let late = r.enter().unwrap();
        r.retire(&[0x20]);
        assert_eq!(r.queue_shape(), vec!['B', 'R', 'B', 'R']);

        drop(first);
        assert!(
            matches!(r.drain_once(&e), Progress::Released(0)),
            "첫 배리어가 풀렸다"
        );
        assert!(matches!(r.drain_once(&e), Progress::Released(1)));
        assert_eq!(
            e.freed(),
            vec![0x10],
            "늦게 시작한 검색이 아직 도는데도 첫 릴리스는 통과한다"
        );

        // 그 뒤의 것은 late가 막고 있다.
        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        drop(late);
        assert!(matches!(r.drain_once(&e), Progress::Released(0)));
        assert!(matches!(r.drain_once(&e), Progress::Released(1)));
        assert_eq!(e.freed(), vec![0x10, 0x20]);
    }

    #[test]
    fn a_search_holds_the_open_slot_until_it_ends() {
        let r = Retirement::new();
        let slot = r.open_slot();
        assert_eq!(r.slot_count(slot), 0);

        let reading = r.enter().unwrap();
        assert_eq!(r.slot_count(slot), 1, "검색이 붙었다");

        drop(reading);
        assert_eq!(r.slot_count(slot), 0, "검색이 떨어졌다");
    }

    #[test]
    fn two_searches_share_one_slot() {
        let r = Retirement::new();
        let slot = r.open_slot();

        let a = r.enter().unwrap();
        let b = r.enter().unwrap();
        assert_eq!(r.slot_count(slot), 2);

        drop(a);
        assert_eq!(r.slot_count(slot), 1);
        drop(b);
        assert_eq!(r.slot_count(slot), 0);
    }

    #[test]
    fn sealing_returns_the_count_at_that_moment() {
        let r = Retirement::new();
        let a = r.enter().unwrap();
        let b = r.enter().unwrap();

        assert_eq!(r.seal_open(), 2, "봉인 순간의 카운트를 정확히 돌려준다");
        drop(a);
        drop(b);
    }

    #[test]
    fn a_search_that_meets_a_sealed_slot_goes_to_the_next_one() {
        let r = Retirement::new();
        let first = r.open_slot();

        let held_open = r.enter().unwrap(); // 봉인이 0을 돌려주지 않도록 하나 붙여둔다
        assert_eq!(r.seal_open(), 1);

        let late = r.enter().unwrap();
        assert_ne!(
            r.open_slot(),
            first,
            "봉인된 슬롯에는 못 붙으므로 다음 칸으로 넘어갔다"
        );
        assert_eq!(
            r.slot_count(first),
            1,
            "봉인된 칸에는 먼저 붙은 하나만 남는다"
        );
        assert_eq!(
            r.slot_count(r.open_slot()),
            1,
            "늦게 온 검색은 새 칸에 붙었다"
        );

        drop(late);
        drop(held_open);
    }

    #[test]
    fn attaching_to_a_sealed_slot_backs_out_without_disturbing_the_count() {
        let r = Retirement::new();
        let slot = r.open_slot();

        let holder = r.enter().unwrap(); // 먼저 붙은 검색 하나
        assert_eq!(seal(&r.slots[slot]), 1, "봉인 순간의 카운트");

        // 봉인된 칸에 붙으려는 검색은 물러난다. enter()로는 이 경로를 못 밟는다 --
        // open이 안 넘어간 채로 부르면 영영 돈다.
        assert!(!attach(&r, &r.slots[slot]), "봉인돼 있으면 붙지 못한다");
        assert_eq!(
            r.slot_count(slot),
            1,
            "물러난 검색이 카운트를 남기지도, 먼저 붙은 하나를 지우지도 않았다"
        );

        drop(holder);
        assert_eq!(r.slot_count(slot), 0, "언더플로 없이 0으로 돌아온다");
    }

    #[test]
    fn a_retire_with_no_search_attached_queues_no_barrier() {
        let r = Retirement::new();
        let slot = r.open_slot();

        r.retire(&[0x10]);
        r.retire(&[0x20]);

        assert_eq!(
            r.queue_shape(),
            vec!['R'],
            "붙은 검색이 없으면 배리어가 없다"
        );
        assert_eq!(r.open_slot(), slot, "슬롯도 넘어가지 않았다");
    }

    #[test]
    fn consecutive_releases_coalesce_into_the_tail() {
        let r = Retirement::new();
        for addr in 0..50u64 {
            r.retire(&[addr]);
        }
        assert_eq!(r.queue_len(), 1, "꼬리에 주소만 붙는다");
    }

    #[test]
    fn slots_do_not_wrap_when_no_search_is_running() {
        let r = Retirement::new();
        let slot = r.open_slot();
        for addr in 0..(POOL as u64 * 3) {
            r.retire(&[addr]);
        }
        assert_eq!(
            r.open_slot(),
            slot,
            "배리어가 큐에 안 들어가면 슬롯은 제자리다"
        );
    }

    #[test]
    fn a_retire_with_a_search_attached_queues_a_barrier_first() {
        let r = Retirement::new();
        let slot = r.open_slot();
        let reading = r.enter().unwrap();

        r.retire(&[0x10]);

        assert_eq!(
            r.queue_shape(),
            vec!['B', 'R'],
            "배리어가 릴리스보다 앞이다"
        );
        assert_ne!(
            r.open_slot(),
            slot,
            "배리어가 큐에 들어갔으므로 슬롯이 넘어갔다"
        );
        drop(reading);
    }

    #[test]
    fn concurrent_retires_never_put_two_barriers_side_by_side() {
        // 봉인과 push가 한 덩어리가 아니면 [B, B, R, R]처럼 배리어가 붙어 나오고,
        // 그러면 두 번째 배리어 뒤의 릴리스를 첫 배리어가 안 막게 된다.
        let r = Arc::new(Retirement::new());
        let _reading = r.enter().unwrap(); // 모든 retire가 배리어를 만들도록 하나 붙여둔다

        std::thread::scope(|s| {
            for t in 0..4u64 {
                let r = Arc::clone(&r);
                s.spawn(move || {
                    for i in 0..50u64 {
                        let _hold = r.enter().unwrap();
                        r.retire(&[t * 1000 + i]);
                    }
                });
            }
        });

        let shape = r.queue_shape();
        for pair in shape.windows(2) {
            assert_ne!(pair, ['B', 'B'], "배리어 둘이 붙어 나왔다: {shape:?}");
        }
    }

    #[test]
    fn a_search_that_starts_after_a_retire_joins_the_next_barrier() {
        let r = Retirement::new();
        let first = r.enter().unwrap();
        r.retire(&[0x10]);
        let second = r.enter().unwrap();

        r.retire(&[0x20]);

        assert_eq!(
            r.queue_shape(),
            vec!['B', 'R', 'B', 'R'],
            "두 번째 검색은 두 번째 배리어에 붙어 첫 릴리스를 막지 않는다"
        );
        drop(first);
        drop(second);
    }

    #[test]
    fn the_gate_closes_when_the_queue_reaches_the_cap() {
        let r = Retirement::with_cap(2);
        let a = r.enter().unwrap();
        r.retire(&[0x10]); // [B, R]  = 2칸
        assert!(r.is_gated(), "캡에 닿으면 게이트가 닫힌다");
        drop(a);
    }

    #[test]
    fn a_drained_queue_opens_the_gate_again() {
        let r = Retirement::with_cap(2);
        let e = FakeElements::default();
        let a = r.enter().unwrap();
        r.retire(&[0x10]);
        assert!(r.is_gated());

        drop(a);
        while !matches!(r.drain_once(&e), Progress::Empty) {}
        assert!(!r.is_gated(), "큐가 빠지면 게이트가 열린다");
    }

    #[test]
    fn a_gated_search_gives_up_rather_than_waiting_forever() {
        let r = Retirement::with_cap(2);
        let a = r.enter().unwrap();
        r.retire(&[0x10]);
        assert!(r.is_gated());

        // a가 끝나지 않으므로 큐가 빠지지 않는다. 대기는 타임아웃으로 끝난다.
        let refused = r.enter_deadline(std::time::Duration::from_millis(50));
        assert!(matches!(refused, Err(crate::error::Error::Busy)));
        drop(a);
    }

    #[test]
    fn the_last_search_of_a_sealed_barrier_rings_the_bell() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        let a = r.enter().unwrap();
        let b = r.enter().unwrap();
        r.retire(&[0x10]); // 봉인(2). retire() 자신도 이제 한 번 울린다 (아래 별도 테스트)

        let after_retire = rings.load(Ordering::Relaxed);
        drop(a);
        assert_eq!(
            rings.load(Ordering::Relaxed),
            after_retire,
            "아직 마지막이 아니다"
        );
        drop(b);
        assert_eq!(
            rings.load(Ordering::Relaxed),
            after_retire + 1,
            "마지막이 울린다"
        );
    }

    #[test]
    fn retire_itself_rings_the_bell() {
        use std::sync::atomic::AtomicUsize as Counter;

        // IMPORTANT 1: retire()가 끝나면서 종을 울리지 않으면, 게이트가 막 닫힌
        // 뒤 대기를 시작한 검색이 sweeper의 TICK 만큼 잠들어 있다가 시간 초과로
        // Busy를 받을 수 있다. 배리어가 생기지 않는 경우(검색이 없을 때)도 포함해
        // retire() 호출 자체가 항상 울려야 한다.
        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        r.retire(&[0x10]);

        assert_eq!(
            rings.load(Ordering::Relaxed),
            1,
            "retire()는 큐에 뭔가 넣을 때마다 sweeper를 깨워야 한다"
        );
    }

    #[test]
    fn an_empty_retire_does_not_ring() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        r.retire(&[]);

        assert_eq!(
            rings.load(Ordering::Relaxed),
            0,
            "아무것도 큐에 들어가지 않았으니 깨울 이유가 없다"
        );
    }

    #[test]
    fn a_backing_out_search_that_clears_the_last_count_rings_the_bell() {
        use std::sync::atomic::AtomicUsize as Counter;

        // MINOR 4: `attach`의 봉인-슬롯 backout 경로도 `Reading::drop`과 같은
        // 조건으로 울려야 한다. 슬롯이 봉인된 채 카운트가 이미 0인 상태를
        // 직접 흉내낸다 -- 마지막 리더가 막 떨어졌지만 sweeper가 아직 그
        // 배리어를 못 지운 순간과 같은 비트 모양이다.
        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        let slot = r.open_slot();
        r.slots[slot].store(SEALED, Ordering::Release);

        assert!(!attach(&r, &r.slots[slot]), "봉인돼 있으면 붙지 못한다");
        assert_eq!(
            rings.load(Ordering::Relaxed),
            1,
            "물러나면서 카운트를 도로 0으로 만들었으니 sweeper를 깨워야 한다"
        );
        assert_eq!(r.slot_count(slot), 0, "언더플로 없이 0으로 돌아온다");
    }

    #[test]
    fn a_backing_out_search_that_is_not_last_does_not_ring() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        let slot = r.open_slot();
        let holder = r.enter().unwrap(); // 진짜 리더 하나가 이 배리어를 붙잡고 있다
        assert_eq!(seal(&r.slots[slot]), 1);

        assert!(!attach(&r, &r.slots[slot]), "봉인돼 있으면 붙지 못한다");
        assert_eq!(
            rings.load(Ordering::Relaxed),
            0,
            "물러난 뒤에도 진짜 리더가 남아 있으니 아직 안 울린다"
        );

        drop(holder);
    }

    #[test]
    fn gated_is_or_ed_not_overwritten() {
        // MINOR 5: `q.gated = ...`가 아니라 `|=`여야 한다. `gated`가 (미래의
        // 다른 이유로) 이미 켜져 있는데 이 호출의 큐 길이만 보고 그대로
        // 대입하면, 대기 중인 검색을 깨우지도 않고 게이트를 꺼 버릴 수 있다.
        let r = Retirement::with_cap(1024);
        {
            let mut q = r.queue.lock().unwrap_or_else(PoisonError::into_inner);
            q.gated = true;
        }

        r.retire(&[0x10]); // 캡(1024)에는 한참 못 미치는 길이

        assert!(
            r.is_gated(),
            "retire()가 자기 계산만으로 이미 켜져 있던 게이트를 꺼서는 안 된다"
        );
    }

    #[test]
    fn leaving_reports_only_when_it_cleared_a_sealed_barrier() {
        let r = Retirement::new();

        // 봉인 안 된 배리어 -- 아무도 안 기다리므로 알릴 것이 없다.
        assert!(!r.enter().unwrap().leave(), "봉인 전이면 false");

        let a = r.enter().unwrap();
        let b = r.enter().unwrap();
        r.retire(&[0x10]); // 봉인(2)

        assert!(!a.leave(), "아직 b가 남았다");
        assert!(b.leave(), "내가 마지막이다 -- 호출자가 비우면 된다");
    }

    #[test]
    fn an_unsealed_barrier_does_not_ring() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        drop(r.enter().unwrap());
        assert_eq!(
            rings.load(Ordering::Relaxed),
            0,
            "봉인 안 된 배리어는 아무도 안 기다린다"
        );
    }
}
