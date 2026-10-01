use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use ::usearch::{Index, IndexOptions, ScalarKind, VectorType, b1x8, f16, ffi::Matches};

use super::held::Elements;
use super::metric::Metric;
use crate::error::Error;
use crate::handler::arcus::element::Layout;
use crate::handler::quant::Quant;
use crate::handler::usearch::halt::Halt;
use crate::handler::usearch::retire::{Progress, Retirement};

type Result<T> = std::result::Result<T, Error>;

fn usearch_err(e: impl std::fmt::Display) -> Error {
    Error::Index(e.to_string())
}

const fn scalar_kind(q: Quant) -> ScalarKind {
    match q {
        Quant::F32 => ScalarKind::F32,
        Quant::F16 => ScalarKind::F16,
        Quant::I8 => ScalarKind::I8,
        Quant::B1 => ScalarKind::B1,
    }
}

const MIN_CAPACITY: usize = 1024;

const THREAD_SLOTS: usize = 64;

/// 검색이 나가는 길에 큐에서 처리할 항목 수의 상한.
///
/// 이 비용은 요청 응답에 붙으므로 무제한이면 안 된다. 예전 `RECLAIM_BATCH`가
/// 같은 이유로 있었다.
const READER_DRAIN_BUDGET: usize = 8;

pub type Accept<'a> = &'a dyn Fn(u64) -> bool;

#[cfg(test)]
static NEXT_IDENTITY: AtomicUsize = AtomicUsize::new(1);

impl Drop for AnnIndex {
    /// `held`뿐 아니라 `retirement`에 아직 남아 있는 `Release`도 여기서 놓아준다.
    ///
    /// 이게 안전한 것은 오직 여기뿐이다: 검색은 `Arc<VectorIndex>`를 쥐고 도므로
    /// ([access/mod.rs] 참고) 이 `drop`이 불릴 수 있는 시점에는 어떤 검색도 존재할
    /// 수 없다. `retirement`에 남은 배리어가 무엇을 막고 있었든 더 이상 지킬
    /// 대상이 없다는 뜻이고, 그래서 큐 순서를 거치지 않고 곧장 반납해도 된다.
    /// `retirement`를 통째로 버리기만 하면(예전 코드) 이미 `held`에서 빠져
    /// `Release`로만 남아 있는 주소는 아무도 돌려주지 않아, 인덱스가 레지스트리를
    /// 떠난 뒤로는 sweeper도 닿을 수 없는 채로 프로세스 끝까지 붙잡힌다.
    fn drop(&mut self) {
        self.retirement.release_pending(&*self.elements);
    }
}

#[derive(Debug)]
pub enum Published {
    Indexed,

    Unindexed(u64),
}

#[derive(Debug)]
pub enum PublishError<E> {
    Store(E),

    Mapping(Error),
}

/// How many independent usearch graphs one index spreads its vectors over.
///
/// usearch takes a global level lock and a handful of per-structure mutexes on
/// every insertion, and past two concurrent writers a single graph spends more
/// of the insert waiting on those than computing distances: the scaling probes
/// below measure x1.5 over four threads on one shared graph, and x3.0 on four
/// separate ones -- the machine's own ceiling. Shards are the separate-graphs
/// case behind one face.
///
/// More shards buy insert concurrency and cost search fan-out, since a query
/// asks every shard for the full k, so the count follows the number of writers
/// the server realistically runs at once, not the core count.
const GRAPH_SHARDS: usize = 1;

/// The graph, as `GRAPH_SHARDS` usearch indexes behind the API of one.
///
/// A vector is added to the shard its key hashes to and never moves: the
/// rename from placeholder key to element address happens inside that shard,
/// so an address does not hash to the shard that holds it, and lookups by key
/// ask the routed shard first and then the rest. Each ask is one hash-table
/// probe, nothing next to the distance work it stands in front of.
///
/// A search asks every shard for the full k and keeps the k nearest of what
/// comes back. That answers exactly what the single graph answered: every
/// vector is in some shard, and a vector among the true k nearest is among its
/// own shard's k nearest, so the merge cannot have lost it.
struct Shards {
    shards: Vec<Index>,
    /// Set for a shard once that shard's first `add` has returned. Until then
    /// a search must not enter that graph at all.
    ///
    /// usearch's `add` reserves its slot by bumping `nodes_count_`
    /// (index.hpp:3222) fifteen lines before it stores the node
    /// (index.hpp:3237) and eighteen before the callback fills the vector
    /// pointer (index.hpp:3240). `search` guards on that same counter
    /// (index.hpp:3448), so it walks into the window and starts at
    /// `entry_slot_`, still its initial 0 -- the slot being filled.
    /// `vectors_lookup_` is never zeroed (`buffer_gt` skips `construct_at` for
    /// trivially constructible types, index.hpp:418), the distance kernel
    /// takes the pointer with no null check, and the process dies.
    /// `usearch_add_search_race` reproduces it on a raw index on 2.26.0,
    /// 2.26.1 and 2.26.2.
    ///
    /// Only the entry point is exposed this way. Every other slot is reached
    /// through links, and a link to a slot is formed after that slot's vector
    /// is in place, so one finished add per shard closes the window for good.
    /// The flag has to live here because usearch cannot answer the question:
    /// `size()` reads the very counter that is published too early.
    populated: Vec<AtomicBool>,
}

impl Shards {
    fn new(options: &IndexOptions) -> Result<Self> {
        let mut shards = Vec::with_capacity(GRAPH_SHARDS);
        for _ in 0..GRAPH_SHARDS {
            shards.push(Index::new(options).map_err(usearch_err)?);
        }
        let populated = (0..GRAPH_SHARDS).map(|_| AtomicBool::new(false)).collect();
        Ok(Self { shards, populated })
    }

    /// Where a new key goes. Placeholder keys count up and element addresses
    /// are aligned pointers; the Fibonacci multiply spreads either, and taking
    /// the high half keeps an address's trailing zero bits out of the pick.
    fn route_to(&self, key: u64) -> usize {
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % self.shards.len()
    }

    /// The shard that holds `key` now, or None. Renames keep a key in the
    /// shard its placeholder hashed to, so this cannot be recomputed -- it is
    /// asked, routed shard first since most keys were never renamed.
    fn holding(&self, key: u64) -> Option<&Index> {
        let routed = self.route_to(key);
        if self.shards.len() == 1 || self.shards[routed].contains(key) {
            return Some(&self.shards[routed]);
        }
        (0..self.shards.len())
            .filter(|i| *i != routed)
            .map(|i| &self.shards[i])
            .find(|shard| shard.contains(key))
    }

    /// 모든 샤드가 담고 있는 노드 수.
    fn size(&self) -> usize {
        self.shards.iter().map(|shard| shard.size()).sum()
    }

    fn add<T: VectorType>(&self, key: u64, vector: &[T]) -> Result<()> {
        let shard = self.route_to(key);
        self.shards[shard].add(key, vector).map_err(usearch_err)?;
        // Strictly after the add returns: a search that observes this has the
        // shard's entry point pointing at a node whose vector is written.
        self.populated[shard].store(true, Ordering::Release);
        Ok(())
    }

    fn get<T: VectorType>(&self, key: u64, buffer: &mut [T]) -> Result<usize> {
        match self.holding(key) {
            Some(shard) => shard.get(key, buffer).map_err(usearch_err),
            None => Ok(0),
        }
    }

    /// 이 키를 실제로 담고 있는 샤드가 있나.
    ///
    /// `holding()`으로 물으면 안 된다 -- 그쪽은 샤드가 하나일 때 담고 있는지
    /// 확인하지 않고 그 샤드를 돌려주므로, 없는 키에도 `Some`을 답한다. 키를
    /// 가지고 무언가 하려는 쪽에는 그래도 되지만(뒤이은 `remove`/`rename`이 0을
    /// 답한다) 존재를 묻는 데에는 못 쓴다.
    #[allow(dead_code)] // HeldSet이 빠지면 resolve()가 이걸 묻는다.
    fn contains(&self, key: u64) -> bool {
        self.shards.iter().any(|shard| shard.contains(key))
    }

    fn rename(&self, from: u64, to: u64) -> Result<usize> {
        match self.holding(from) {
            Some(shard) => shard.rename(from, to).map_err(usearch_err),
            None => Ok(0),
        }
    }

    fn remove(&self, key: u64) -> Result<usize> {
        match self.holding(key) {
            Some(shard) => shard.remove(key).map_err(usearch_err),
            None => Ok(0),
        }
    }

    /// Keys spread by hash, not by count, so a shard can run somewhat over an
    /// even split; an eighth on top covers that spread many times out, and the
    /// staged placeholders ride in the same slack they always did.
    fn reserve_capacity_and_threads(&self, capacity: usize, threads: usize) -> Result<()> {
        let n = self.shards.len();
        let per_shard = capacity / n + capacity / (8 * n) + THREAD_SLOTS;
        for shard in &self.shards {
            shard
                .reserve_capacity_and_threads(per_shard, threads)
                .map_err(usearch_err)?;
        }
        Ok(())
    }

    fn search<T: VectorType>(&self, query: &[T], count: usize) -> Result<Matches> {
        self.merged(count, |shard, want| {
            shard.search(query, want).map_err(usearch_err)
        })
    }

    fn filtered_search<T: VectorType, F>(
        &self,
        query: &[T],
        count: usize,
        filter: F,
    ) -> Result<Matches>
    where
        F: Fn(u64) -> bool,
    {
        self.merged(count, |shard, want| {
            shard
                .filtered_search(query, want, &filter)
                .map_err(usearch_err)
        })
    }

    /// 샤드마다 묻고, 거리순으로 추려 `count`개를 답한다.
    ///
    /// **샤드에 묻는 개수는 그 샤드가 담은 노드 수로 깎는다.** 검색이 노드보다
    /// 많이 답할 수는 없으니 결과는 그대로인데, 깎지 않으면 요청한 숫자가
    /// 그대로 할당이 된다 -- usearch의 Rust 바인딩은 `count`개짜리 결과 버퍼를
    /// 미리 잡고 0으로 채우고 나서야 검색을 부르고(`rust/lib.cpp`), 그 `reserve`는
    /// 실패하면 오류가 아니라 abort다. 상한을 주지 않은 `<num>`은 그래서 숫자
    /// 하나로 데몬을 눕힐 수 있었다. 노드 수로 깎으면 쓰는 메모리가 요청이 아니라
    /// **가진 데이터**에 비례하고, 그건 이미 그보다 훨씬 큰 벡터를 들고 있다는
    /// 뜻이라 따로 상한을 둘 일이 없어진다.
    ///
    /// 그래프 안쪽 버퍼(`top`/`next`)도 `max(efs, want)`로 같이 깎이는데, 그쪽은
    /// 원래 실패를 값으로 돌려주므로(`index.hpp`의 `result.failed("Out of memory!")`)
    /// 죽지 않는다.
    fn merged(
        &self,
        count: usize,
        one: impl Fn(&Index, usize) -> Result<Matches>,
    ) -> Result<Matches> {
        let mut all: Vec<(u64, f32)> = Vec::new();
        for (shard, populated) in self.shards.iter().zip(&self.populated) {
            // A shard nothing has landed in yet is skipped, not asked: see
            // `populated`. An empty shard has nothing to contribute anyway.
            if !populated.load(Ordering::Acquire) {
                continue;
            }
            // 담은 것보다 많이 답할 수는 없다. `populated`를 세운 직후라
            // `size()`가 아직 0일 수 있어, 최소 하나는 묻는다.
            let want = count.min(shard.size().max(1));

            // **묻기 전에 잡는다.** 담을 자리는 어차피 필요하고(`extend`는 못
            // 잡으면 abort다), 샤드가 돌려줄 수 있는 최대가 `want`이므로 지금
            // 잡아 두면 크기가 같다. 순서가 요점이다 -- 바인딩도 들어가자마자
            // 같은 크기를 잡는데 그쪽 `reserve`는 실패하면 abort라(`rust/lib.cpp`),
            // 자리가 없다는 것을 우리가 먼저 알면 `SERVER_ERROR`로 돌아설 수 있다.
            // 바인딩이 쓸 자리는 `probe_pair`가 따로 재므로, 둘이 동시에 떠
            // 있는 양을 그대로 확인하고 들어간다. 확인과 실제 할당 사이에는
            // 다른 요청이 끼어들 수 있으니 이것도 보장은 아니다.
            crate::room::reserve(&mut all, want)?;
            // 바인딩이 잡을 것도 같은 자리에서 미리 재본다. 그쪽은 실패하면
            // abort라, 돌아설 수 있는 곳이 여기뿐이다. 다만 이것이 거르는 것은
            // **할당 시점에 드러나는 실패뿐**이고 OOM은 못 막는다 -- 자세한
            // 것은 `room::probe_pair`에 적어 두었다.
            crate::room::probe_pair::<u64, f32>(want)?;

            let matches = one(shard, want)?;
            all.extend(matches.keys.into_iter().zip(matches.distances));
        }
        all.sort_by(|a, b| a.1.total_cmp(&b.1));
        all.truncate(count);

        let mut keys = crate::room::vec(all.len())?;
        let mut distances = crate::room::vec(all.len())?;
        keys.extend(all.iter().map(|(key, _)| *key));
        distances.extend(all.iter().map(|(_, distance)| *distance));
        Ok(Matches { keys, distances })
    }

    fn memory_usage(&self) -> usize {
        self.shards.iter().map(Index::memory_usage).sum()
    }

    fn memory_stats(&self) -> ::usearch::ffi::MemoryStats {
        let mut total: ::usearch::ffi::MemoryStats = self.shards[0].memory_stats();
        for shard in &self.shards[1..] {
            let s = shard.memory_stats();
            total.graph_allocated += s.graph_allocated;
            total.graph_wasted += s.graph_wasted;
            total.graph_reserved += s.graph_reserved;
            total.vectors_allocated += s.vectors_allocated;
            total.vectors_wasted += s.vectors_wasted;
            total.vectors_reserved += s.vectors_reserved;
        }
        total
    }
}
pub struct AnnIndex {
    pub layout: Layout,
    pub metric: Metric,
    threads: usize,

    inner: RwLock<Shards>,
    reserved: AtomicUsize,

    elements: Arc<dyn Elements>,

    /// 큐에 못 넣어 그래프가 저장소와 어긋났을 때 이 인덱스를 잠근다.
    halt: Halt,

    #[cfg(test)]
    identity: usize,

    stuck: std::sync::Mutex<Vec<u64>>,
    pub(super) retirement: Retirement,
}

impl AnnIndex {
    pub fn new(
        layout: Layout,
        metric: Metric,
        connectivity: usize,
        expansion_add: usize,
        expansion_search: usize,
        elements: Arc<dyn Elements>,
    ) -> Result<Self> {
        Self::with_threads(
            layout,
            metric,
            connectivity,
            expansion_add,
            expansion_search,
            THREAD_SLOTS,
            elements,
        )
    }

    fn with_threads(
        layout: Layout,
        metric: Metric,
        connectivity: usize,
        expansion_add: usize,
        expansion_search: usize,
        threads: usize,
        elements: Arc<dyn Elements>,
    ) -> Result<Self> {
        metric.check_quant(layout.quant)?;

        let options = IndexOptions {
            dimensions: layout.dim,
            metric: metric.kind(),
            quantization: scalar_kind(layout.quant),
            connectivity,
            expansion_add,
            expansion_search,
            multi: false,
        };
        let threads = threads.max(1);
        let index = Shards::new(&options)?;
        index.reserve_capacity_and_threads(MIN_CAPACITY, threads)?;

        Ok(Self {
            layout,
            metric,
            threads,
            inner: RwLock::new(index),
            reserved: AtomicUsize::new(MIN_CAPACITY),
            elements,
            halt: Halt::new(),
            #[cfg(test)]
            identity: NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed),
            stuck: std::sync::Mutex::new(Vec::new()),
            retirement: Retirement::new()?,
        })
    }

    /// 밀린 것을 충분히 비웠으면 잠금을 푼다. sweeper가 매 바퀴 부른다.
    ///
    /// 잠긴 인덱스는 조회를 전부 거절하고 있으므로, 푸는 것이 늦으면 그만큼
    /// 오래 못 쓴다. 반대로 자리가 하나 나자마자 풀면 다음 삭제에 바로 다시
    /// 막히므로, 점유율이 눈에 띄게 내려간 뒤에 받는다.
    pub fn resume_if_drained(&self) {
        if self.halt.is_halted() && self.retirement.drained_enough() {
            eprintln!("ArcVector: the retirement backlog has drained; taking reads again");
            self.halt.resume();
        }
    }

    /// 아이템 역참조를 시작한다. 잠겨 있으면 `None`.
    ///
    /// 가드가 살아 있는 동안 `halt()`가 돌아오지 않으므로, 엔진이 그 아이템을
    /// 해제하지 않는다. 필터가 순회 중에 쓴다.
    pub fn touch_for_deref(&self) -> Option<crate::handler::usearch::halt::Touching<'_>> {
        self.halt.touch()
    }

    /// 이 인덱스가 잠겨 있나. 모든 명령의 입구가 이것을 본다.
    pub fn is_halted(&self) -> bool {
        self.halt.is_halted()
    }

    /// 그래프에 선 노드 수. usearch가 세고 있으므로 따로 세지 않는다.
    ///
    /// 자리표(staged) 노드도 여기 포함된다 -- 잠깐이고, `maxcount` 게이트가
    /// 조금 보수적으로 도는 편이 넘치는 것보다 낫다.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .size()
    }

    /// 이 그래프가 그 주소를 들고 있나. unlink 콜백이 "내가 받은 것인가"를
    /// 이것으로 답한다.
    /// 아직 링크되지 않은 아이템의 주소에 `STAGED` 비트를 세운 키.
    ///
    /// 사용자 영역 포인터는 최상위 비트가 언제나 0이라, 이 비트가 선 값은
    /// 정상 주소가 될 수 없다. 그래서 역참조 없이 비트 하나로 "아직 링크 전"을
    /// 가려낼 수 있고, 링크가 끝나면 같은 주소로 되돌리기만 하면 된다.
    pub const STAGED: u64 = 1 << 63;

    /// 자리표 키인가.
    ///
    /// 검색이 노드마다 묻고 만료와 같은 자리에서 거절한다. 거절은 순회 안에서
    /// 일어나므로 usearch가 k를 채울 때까지 더 걸어간다 -- 자리표 수를 따로
    /// 세어 요청 개수에 더할 이유가 없다.
    pub fn is_staged(key: u64) -> bool {
        key & Self::STAGED != 0
    }

    /// 링크 전인 아이템의 벡터를 미리 그래프에 넣는다.
    ///
    /// **HNSW 삽입을 cache lock 밖으로 빼기 위한 것이다.** `vadd`의 무거운 일은
    /// 이 삽입 하나인데, 아이템을 링크하면 엔진이 cache lock을 쥔 채 콜백을
    /// 부르고 거기서 삽입하면 그동안 데몬 전체가 선다. 먼저 넣어 두면 콜백이
    /// 할 일은 [`AnnIndex::unstage`] 하나 -- 해시 항목을 옮기는 것뿐이라
    /// 그래프는 건드리지 않는다.
    pub fn stage_at(&self, addr: u64, vector: &[u8]) -> Result<()> {
        self.link_node(addr | Self::STAGED, vector)
    }

    /// 자리표를 실제 주소로 올린다. 링크 콜백이 부른다.
    pub fn unstage(&self, addr: u64) -> bool {
        self.rename_node(addr | Self::STAGED, addr)
    }

    /// 링크하지 못한 자리표를 그래프에서 거둔다.
    ///
    /// 놓아줄 참조가 없다 -- 그 아이템은 링크된 적이 없어 unlink도 오지
    /// 않는다. 호출자가 `discard_allocated`로 따로 돌려준다.
    pub fn drop_staged(&self, addr: u64) {
        self.drop_node(addr | Self::STAGED);
    }

    pub fn holds(&self, addr: u64) -> bool {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(addr)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn held_bytes(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .memory_usage()
    }

    pub fn used_bytes(&self) -> usize {
        let s = self
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .memory_stats();
        let tape = |allocated: usize, wasted: usize, reserved: usize| {
            allocated.saturating_sub(wasted).saturating_sub(reserved)
        };
        tape(s.graph_allocated, s.graph_wasted, s.graph_reserved)
            + tape(s.vectors_allocated, s.vectors_wasted, s.vectors_reserved)
    }

    pub fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Acquire)
    }

    fn ensure_capacity(&self, needed: usize) -> Result<()> {
        if needed <= self.reserved.load(Ordering::Acquire) {
            return Ok(());
        }
        let index = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let current = self.reserved.load(Ordering::Acquire);
        if needed <= current {
            return Ok(());
        }
        let target = (current * 2).max(needed).max(MIN_CAPACITY);
        index.reserve_capacity_and_threads(target, self.threads)?;
        self.reserved.store(target, Ordering::Release);
        Ok(())
    }

    /// 주소를 놓아줄 목록에 넣는다. 엔진을 건드리지 않는다.
    ///
    /// 호출 전에 그 주소가 `held`에서 빠져 있어야 한다 -- 그래야 이 뒤에 시작한
    /// 검색이 `resolve()`에서 걸러진다.
    fn retire(&self, addrs: &[u64]) {
        self.retirement.retire(addrs);
    }

    /// 큐의 항목 하나를 처리한다. 무언가 했으면 `true`.
    ///
    /// sweeper만 부른다. `release`가 cache lock을 잡으므로 그래프 락 아래에서
    /// 부르면 안 된다.
    pub fn drain_retired(&self) -> bool {
        matches!(
            self.retirement.drain_once(&*self.elements),
            Progress::Released(_)
        )
    }

    /// sweeper를 깨우는 종을 단다. 레지스트리에 들어갈 때 한 번 부른다.
    pub fn set_bell(&self) {
        self.retirement
            .set_bell(std::sync::Arc::new(crate::handler::access::sweep::wake));
    }

    /// 주소 하나를 sweeper의 목록에 넣는다. 자리가 없으면 `false`.
    ///
    /// **`held`에서 뺀 뒤에 부른다** -- 그래야 이 뒤에 시작한 검색이 그 주소에
    /// 닿지 못한다.
    pub fn retire_one(&self, addr: u64) -> bool {
        self.retirement.retire(&[addr])
    }

    /// 큐에 못 넣어 그래프가 저장소와 어긋났다. 인덱스를 잠근다.
    ///
    /// **진행 중인 역참조가 끝나기를 기다리고 돌아온다.** 그래서 이 함수가
    /// 돌아온 뒤에는 엔진이 그 아이템을 해제해도 읽고 있는 검색이 없다. 기다리는
    /// 것은 역참조 하나뿐이라 cache lock 아래에서도 짧다.
    pub fn halt_for_overflow(&self) {
        eprintln!(
            "ArcVector: the retirement queue is full; halting this index until the \
             sweeper works the backlog down"
        );
        self.halt.halt();
    }

    /// 아이템이 링크됐다. 그 주소를 키로 그래프에 넣는다.
    ///
    /// 엔진이 이미 우리 몫의 참조를 잡아뒀으므로, 넣는 데 성공하면 그대로 들고
    /// 있으면 된다. 실패하면 호출자가 거절해서 엔진이 회수하게 한다.
    ///
    /// **엔진의 cache lock 아래에서 불린다.** `ensure_capacity`가 usearch의
    /// 그래프를 늘릴 수 있고 삽입 자체도 탐색을 수반하므로, 이 함수가 도는 동안
    /// 데몬 전체가 선다. 큐를 없앤 대가이고 설계로는 피할 수 없다.
    pub fn link_node(&self, addr: u64, vector: &[u8]) -> Result<()> {
        self.check_vector(vector)?;
        self.ensure_capacity(self.live() + 1)?;
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        self.typed_add(&index, addr, vector)
    }

    /// 아이템이 빠진다. 그래프에서 노드를 뺀다.
    ///
    /// 뺀 뒤에 그 주소를 놓아줄 사람이 필요하다 -- 호출자가 큐에 넣거나, 못
    /// 넣으면 거절해서 엔진에게 넘긴다.
    pub fn unlink_at(&self, addr: u64) -> bool {
        self.unlink_node(addr)
    }

    /// 아이템이 새것으로 교체된다. 그래프를 `new`로 옮긴다.
    ///
    /// **언제나 이것이 먼저다.** 엔진이 해시테이블을 바꾸기 전에 그래프가 새
    /// 주소를 가리켜야, 그 사이의 조회가 사라질 주소를 안 따라간다.
    pub fn rename_node(&self, old: u64, new: u64) -> bool {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        match index.rename(old, new) {
            Ok(1) => true,
            Ok(0) => false,
            Ok(count) => {
                eprintln!("ArcVector: renaming a node moved {count} keys, not one");
                false
            }
            Err(e) => {
                eprintln!("ArcVector: could not rename a node: {e}");
                false
            }
        }
    }

    fn drop_node(&self, key: u64) -> bool {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        index.remove(key).is_ok()
    }

    fn unlink_node(&self, addr: u64) -> bool {
        if self.drop_node(addr) {
            return true;
        }
        let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
        if stuck.try_reserve(1).is_err() {
            eprintln!(
                "ArcVector: node {addr:#x} is still in the graph and cannot be queued for \
                 another attempt; its element stays held"
            );
            return false;
        }
        stuck.push(addr);
        false
    }

    pub fn retry_stuck(&self) {
        let pending = {
            let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
            if stuck.is_empty() {
                return;
            }
            std::mem::take(&mut *stuck)
        };
        for addr in pending {
            if self.unlink_node(addr) {
                self.retire(&[addr]);
            }
        }
    }

    fn check_vector(&self, vector: &[u8]) -> Result<()> {
        if vector.len() != self.layout.vector_bytes() {
            return Err(Error::bad_request(format!(
                "vector is {} bytes, expected {}",
                vector.len(),
                self.layout.vector_bytes()
            )));
        }
        Ok(())
    }

    fn live(&self) -> usize {
        self.len()
    }

    fn typed_add(&self, index: &Shards, key: u64, vector: &[u8]) -> Result<()> {
        match self.layout.quant {
            Quant::F32 => index.add(key, &to_f32(vector)),
            Quant::F16 => index.add(key, f16::from_i16s(&to_i16(vector))),
            Quant::I8 => index.add(key, &to_i8(vector)),
            Quant::B1 => index.add(key, b1x8::from_u8s(vector)),
        }
    }

    pub fn vector_of(&self, addr: u64) -> Result<Option<Vec<u8>>> {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let key = addr;
        let dim = self.layout.dim;

        let bytes = match self.layout.quant {
            Quant::F32 => {
                let mut out = vec![0f32; dim];
                if index.get(key, &mut out)? == 0 {
                    return Ok(None);
                }
                out.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            Quant::F16 => {
                let mut out = vec![0i16; dim];
                if index.get(key, f16::from_mut_i16s(&mut out))? == 0 {
                    return Ok(None);
                }
                out.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            Quant::I8 => {
                let mut out = vec![0i8; dim];
                if index.get(key, &mut out)? == 0 {
                    return Ok(None);
                }
                out.iter().map(|v| *v as u8).collect()
            }
            Quant::B1 => {
                let mut out = vec![0u8; dim];
                if index.get(key, b1x8::from_mut_u8s(&mut out))? == 0 {
                    return Ok(None);
                }
                out.truncate(self.layout.vector_bytes());
                out
            }
        };
        if bytes.len() != self.layout.vector_bytes() {
            return Err(Error::Index(format!(
                "the graph returned {} bytes for {addr:#x}, expected {}",
                bytes.len(),
                self.layout.vector_bytes()
            )));
        }
        Ok(Some(bytes))
    }

    pub fn reserve(&self, count: usize) -> Result<()> {
        self.ensure_capacity(count)
    }

    /// Turns the traversal's hits into answerable results, dropping the ones
    /// that stopped being answerable while the search ran.
    ///
    /// A delete that lands mid-search is the case this exists for. The search
    /// may have put that address in its candidate list before the delete, and
    /// usearch will still offer it afterwards -- `remove` only flips a key
    /// under a mutex the traversal does not take. The graph's own set is the
    /// authority on what is still there, and asking it costs one lock and a
    /// hash lookup per hit, with no engine call.
    ///
    /// The address itself stays valid throughout: the reference the graph took
    /// is not handed back until every search that could hold it has finished.
    /// So this is about answering a stale hit, never about reading freed
    /// memory.
    fn resolve(&self, hits: &[(u64, f32)]) -> Result<Vec<(u64, Arc<str>, f32)>> {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        // 걸러지는 것이 있어 넘치게 잡을 수 있지만, 자리를 못 잡으면 거절한다는
        // 것이 요점이다. 히트마다 조금씩 밀어 넣으면 그 push가 abort한다.
        let mut out = crate::room::vec(hits.len())?;
        out.extend(hits.iter().filter_map(|(key, distance)| {
            // 확인과 역참조를 한 가드 안에 둔다. 콜백이 거절하면 엔진이 그
            // 자리에서 아이템을 해제하는데, 가드가 살아 있는 동안에는
            // `halt()`가 돌아오지 않으므로 그 해제가 시작되지 않는다.
            // 인덱스가 이미 잠겼으면 답을 포기한다 -- 그게 도는 검색까지
            // 끊는다는 뜻이다.
            let _touching = self.halt.touch()?;
            if !index.contains(*key) {
                return None;
            }
            let id = self.elements.id_at(*key)?;
            Some((*key, id, *distance))
        }));
        Ok(out)
    }

    fn unfiltered(&self, index: &Shards, query: &[u8], k: usize) -> Result<Matches> {
        match self.layout.quant {
            Quant::F32 => index.search(&to_f32(query), k),
            Quant::F16 => index.search(f16::from_i16s(&to_i16(query)), k),
            Quant::I8 => index.search(&to_i8(query), k),
            Quant::B1 => index.search(b1x8::from_u8s(query), k),
        }
    }

    fn matches<P: Fn(u64) -> bool>(
        &self,
        index: &Shards,
        query: &[u8],
        k: usize,
        predicate: P,
    ) -> Result<Matches> {
        match self.layout.quant {
            Quant::F32 => index.filtered_search(&to_f32(query), k, predicate),
            Quant::F16 => index.filtered_search(f16::from_i16s(&to_i16(query)), k, predicate),
            Quant::I8 => index.filtered_search(&to_i8(query), k, predicate),
            Quant::B1 => index.filtered_search(b1x8::from_u8s(query), k, predicate),
        }
    }

    pub fn search(
        &self,
        query: &[u8],
        k: usize,
        accept: Option<Accept<'_>>,
    ) -> Result<Vec<(u64, Arc<str>, f32)>> {
        if query.len() != self.layout.vector_bytes() {
            return Err(Error::bad_request(format!(
                "query is {} bytes, expected {}",
                query.len(),
                self.layout.vector_bytes()
            )));
        }

        let reading = self.retirement.enter();

        let matches = {
            let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            match accept {
                None => self.unfiltered(&index, query, k),

                Some(matches) => self.matches(&index, query, k, matches),
            }?
        };

        let mut hits: Vec<(u64, f32)> = crate::room::vec(matches.keys.len())?;
        hits.extend(matches.keys.into_iter().zip(matches.distances));
        let answer = self.resolve(&hits)?;

        // 여기가 이 검색이 붙어 있던 배리어의 마지막이면, 그 뒤의 `Release`를
        // 막고 있던 것이 방금 사라진 것이다. sweeper를 기다리지 않고 그 자리에서
        // 놓아준다 -- 이 지점에서는 그래프 락도 `held` 락도 cache lock도 쥐고
        // 있지 않다.
        if reading.leave() {
            self.drain_bounded();
        }
        Ok(answer)
    }

    /// 큐를 조금 비운다. **어떤 락도 쥐지 않은 채** 불려야 한다.
    ///
    /// 상한이 있는 것은 이 비용이 요청 응답 시간에 그대로 붙기 때문이다. 상한에
    /// 걸리고도 남아 있으면 sweeper에게 넘긴다.
    fn drain_bounded(&self) {
        let mut budget = READER_DRAIN_BUDGET;
        while budget > 0 && self.drain_retired() {
            budget -= 1;
        }
        if budget == 0 {
            self.retirement.ring();
        }
    }
}

fn to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn to_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn to_i8(bytes: &[u8]) -> Vec<i8> {
    bytes.iter().map(|b| *b as i8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::quant::Quant;
    use crate::handler::usearch::metric::Metric;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeStore {
        by_addr: RwLock<std::collections::HashMap<u64, Arc<str>>>,
        by_id: RwLock<std::collections::HashMap<(usize, Arc<str>), u64>>,
        next: AtomicU64,
        released: AtomicUsize,
    }

    static FAKE: std::sync::LazyLock<Arc<FakeStore>> = std::sync::LazyLock::new(|| {
        Arc::new(FakeStore {
            by_addr: RwLock::new(std::collections::HashMap::new()),
            by_id: RwLock::new(std::collections::HashMap::new()),

            next: AtomicU64::new(0x7f00_0000_0000),
            released: AtomicUsize::new(0),
        })
    });

    impl FakeStore {
        fn link(&self, idx: &AnnIndex, id: &str) -> u64 {
            let addr = self.next.fetch_add(64, Ordering::Relaxed);
            let id: Arc<str> = Arc::from(id);
            self.by_addr.write().unwrap().insert(addr, Arc::clone(&id));
            self.by_id.write().unwrap().insert((map_of(idx), id), addr);
            addr
        }

        fn unlink(&self, idx: &AnnIndex, id: &str) -> Option<u64> {
            self.by_id
                .write()
                .unwrap()
                .remove(&(map_of(idx), Arc::from(id)))
        }
    }

    fn map_of(idx: &AnnIndex) -> usize {
        idx.identity
    }

    impl Elements for FakeStore {
        fn id_at(&self, addr: u64) -> Option<Arc<str>> {
            self.by_addr.read().unwrap().get(&addr).cloned()
        }

        fn release(&self, addrs: &[u64]) {
            let mut by_addr = self.by_addr.write().unwrap();
            for addr in addrs {
                by_addr.remove(addr);
                self.released.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn build(dim: usize, quant: Quant, metric: Metric, threads: usize) -> AnnIndex {
        AnnIndex::with_threads(
            Layout::new(dim, quant),
            metric,
            0,
            0,
            0,
            threads,
            Arc::clone(&FAKE) as Arc<dyn Elements>,
        )
        .unwrap()
    }

    fn add(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        put(idx, id, coords)
    }

    fn put(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        let encoded = crate::handler::quant::encode(coords, idx.layout.quant);
        publish(idx, id, &encoded)
    }

    fn search(idx: &AnnIndex, coords: &[f32], k: usize) -> Vec<String> {
        let q = crate::handler::quant::encode(coords, idx.layout.quant);
        idx.search(&q, k, None)
            .unwrap()
            .into_iter()
            .map(|(_, id, _)| id.to_string())
            .collect()
    }

    /// link 이벤트가 하는 일과 같다: 아이템이 생기고, 그 주소로 노드가 선다.
    fn publish(idx: &AnnIndex, id: &str, coords: &[u8]) -> u64 {
        let addr = FAKE.link(idx, id);
        idx.link_node(addr, coords).expect("그래프가 받는다");
        addr
    }

    /// unlink 이벤트가 하는 일과 같다: 노드를 빼고 주소를 큐에 넘긴다.
    fn remove(idx: &AnnIndex, id: &str) -> bool {
        let Some(addr) = FAKE.unlink(idx, id) else {
            return false;
        };
        idx.unlink_at(addr);
        idx.retire_one(addr);
        true
    }

    #[test]
    fn add_and_search_returns_the_nearest_neighbour() {
        let idx = build(4, Quant::F32, Metric::L2, 4);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "b", &[0.0, 1.0, 0.0, 0.0]);
        add(&idx, "c", &[0.0, 0.0, 1.0, 0.0]);
        assert_eq!(idx.len(), 3);

        let hits = search(&idx, &[0.9, 0.1, 0.0, 0.0], 1);
        assert_eq!(hits, vec!["a"]);
        assert_eq!(search(&idx, &[0.0, 0.0, 0.9, 0.0], 1), vec!["c"]);
    }

    #[test]
    fn every_quantization_round_trips_through_usearch() {
        for (q, m) in [
            (Quant::F32, Metric::Cos),
            (Quant::F16, Metric::Cos),
            (Quant::I8, Metric::Cos),
            (Quant::B1, Metric::Hamming),
        ] {
            let idx = build(8, q, m, 2);
            add(&idx, "x", &[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0]);
            add(&idx, "y", &[-1.0, -1.0, -1.0, -1.0, 1.0, 1.0, 1.0, 1.0]);

            let hits = search(&idx, &[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0], 1);
            assert_eq!(hits, vec!["x"], "quant {q:?}");
        }
    }

    #[test]
    fn a_halted_index_answers_nothing_even_mid_search() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5), vec!["a"]);

        // 큐에 못 넣어 잠긴 상태. resolve의 가드가 막아 hit이 하나도 안 나온다 --
        // 엔진이 아이템을 해제해도 역참조하지 않는다는 뜻이다.
        idx.halt_for_overflow();
        assert!(
            search(&idx, &[1.0, 0.0, 0.0, 0.0], 5).is_empty(),
            "잠긴 인덱스는 도는 검색도 답을 내지 않는다"
        );
    }

    #[test]
    fn a_halted_index_comes_back_once_the_sweeper_has_drained_it() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        // 콜백이 겪는 상황을 그대로 만든다: 큐를 자리가 없을 때까지 채운다.
        let mut fake = 0x1000_0000u64;
        while idx.retire_one(fake) {
            fake += 1;
        }
        idx.halt_for_overflow();
        assert!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5).is_empty(), "잠겼다");

        // 가득 찬 채로 풀면 다음 삭제에 곧바로 다시 막힌다.
        idx.resume_if_drained();
        assert!(idx.is_halted(), "비우기 전에는 계속 잠겨 있다");

        while idx.drain_retired() {}
        idx.resume_if_drained();

        assert!(!idx.is_halted(), "비웠으면 다시 받는다");
        assert_eq!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5), vec!["a"]);
    }

    #[test]
    fn linking_a_node_makes_it_findable_by_its_address() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let coords = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], idx.layout.quant);
        let addr = FAKE.link(&idx, "a");

        idx.link_node(addr, &coords).expect("그래프가 받는다");

        let index = idx.inner.read().unwrap();
        assert!(index.contains(addr), "주소가 키로 들어갔다");
    }

    #[test]
    fn unlinking_takes_the_node_out() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let coords = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], idx.layout.quant);
        let addr = FAKE.link(&idx, "a");
        idx.link_node(addr, &coords).unwrap();

        idx.unlink_at(addr);
        assert!(
            !idx.inner.read().unwrap().contains(addr),
            "그래프에 더는 없다"
        );
    }

    #[test]
    fn renaming_moves_the_node_to_the_new_address() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let coords = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], idx.layout.quant);
        let old = FAKE.link(&idx, "a");
        idx.link_node(old, &coords).unwrap();
        let new = FAKE.link(&idx, "a2");

        assert!(idx.rename_node(old, new));

        let index = idx.inner.read().unwrap();
        assert!(!index.contains(old), "옛 주소는 빠졌다");
        assert!(index.contains(new), "새 주소가 섰다");
    }

    #[test]
    fn renaming_an_address_the_graph_never_had_reports_it() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        assert!(
            !idx.rename_node(0xdead_beef, 0xfeed_face),
            "없던 것을 옮겼다고 답하면 안 된다"
        );
    }

    #[test]
    fn the_last_search_out_of_a_sealed_barrier_drains_on_its_way_out() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let doomed = add(&idx, "goes", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "stays", &[0.0, 1.0, 0.0, 0.0]);

        // 순회 중에 삭제가 들어온다. 그러면 이 검색이 붙어 있는 배리어가 봉인되고,
        // 이 검색이 그것을 0으로 만드는 마지막이 된다.
        let deleted = std::cell::Cell::new(false);
        let accept = |_key: u64| -> bool {
            if !deleted.replace(true) {
                remove(&idx, "goes");
            }
            true
        };

        let q = crate::handler::quant::encode(&[0.0, 1.0, 0.0, 0.0], idx.layout.quant);
        let answered = idx.search(&q, 5, Some(&accept)).unwrap();
        assert!(deleted.get(), "순회가 술어를 불렀다");
        assert!(!answered.is_empty());

        // sweeper는 돌지 않았다. 검색이 나가는 길에 직접 놓아준 것이다.
        assert!(
            FAKE.id_at(doomed).is_none(),
            "마지막 검색이 나가는 길에 큐를 비웠어야 한다"
        );
    }

    #[test]
    fn a_running_search_holds_back_the_address_it_could_have_seen() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let addr = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        let reading = idx.retirement.enter();
        assert!(remove(&idx, "a"), "그래프에서 뺐다");

        while idx.drain_retired() {}
        assert!(
            FAKE.id_at(addr).is_some(),
            "이 검색이 그 주소를 봤을 수 있으므로 아직 못 놓아준다"
        );

        drop(reading);
        while idx.drain_retired() {}
        assert!(FAKE.id_at(addr).is_none(), "검색이 끝났으니 돌려준다");
    }

    #[test]
    fn a_search_that_starts_after_the_delete_does_not_hold_it_back() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let addr = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        assert!(remove(&idx, "a"));
        let late = idx.retirement.enter();

        while idx.drain_retired() {}
        assert!(
            FAKE.id_at(addr).is_none(),
            "삭제 뒤에 시작한 검색은 그 주소에 닿을 수 없으므로 막지 않는다"
        );
        drop(late);
    }

    #[test]
    fn usearch_removes_a_node_while_our_callback_is_still_inside_it() {
        let idx = Arc::new(build(4, Quant::F32, Metric::L2, 4));
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "b", &[0.9, 0.1, 0.0, 0.0]);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<u64>();
        let (removed_tx, removed_rx) = std::sync::mpsc::channel::<usize>();

        let other = Arc::clone(&idx);
        let remover = std::thread::spawn(move || {
            let key = entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the callback never said which node it was inside");
            let index = other.inner.read().unwrap_or_else(PoisonError::into_inner);
            let removed = index.remove(key).expect("remove");
            removed_tx.send(removed).unwrap();
        });

        let query = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);
        let reported = std::cell::Cell::new(false);
        let witnessed = std::cell::Cell::new(0usize);
        let probe = |key: u64| {
            if !reported.replace(true) {
                entered_tx.send(key).unwrap();
                witnessed.set(
                    removed_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("the node was not removed while the callback was inside it"),
                );
            }
            true
        };
        let _ = idx.search(&query, 2, Some(&probe as Accept)).unwrap();
        remover.join().unwrap();

        assert_eq!(
            witnessed.get(),
            1,
            "usearch took the node out with the callback for that same node still running, \
             which is why the held set is read-locked across the dereference"
        );
    }

    #[test]
    fn the_predicate_excludes_what_it_rejects() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "keep", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "skip", &[1.0, 0.0, 0.0, 0.0]);

        let q = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);

        let keep = |key: u64| FAKE.id_at(key).is_some_and(|id| &*id == "keep");
        let hits = idx.search(&q, 10, Some(&keep as Accept)).unwrap();
        let ids: Vec<String> = hits.iter().map(|(_, id, _)| id.to_string()).collect();
        assert_eq!(ids, vec!["keep"]);
    }

    #[test]
    fn churn_does_not_grow_the_index() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        for round in 0..200 {
            let keys: Vec<u64> = (0..5)
                .map(|i| add(&idx, &format!("r{round}-{i}"), &[i as f32, 1.0, 2.0, 3.0]))
                .collect();
            for (i, _) in keys.into_iter().enumerate() {
                assert!(remove(&idx, &format!("r{round}-{i}")));
            }
            assert_eq!(idx.len(), 0, "round {round} leaked live entries");
        }

        assert_eq!(idx.len(), 0);
        assert_eq!(
            idx.reserved.load(Ordering::Acquire),
            MIN_CAPACITY,
            "the reservation grew with keys rather than with members"
        );
    }

    #[test]
    fn growth_past_the_initial_reservation_succeeds() {
        let idx = build(4, Quant::F32, Metric::L2, 4);
        for i in 0..1100 {
            add(&idx, &format!("v{i}"), &[i as f32, 0.0, 0.0, 0.0]);
        }
        assert_eq!(idx.len(), 1100);
        assert_eq!(search(&idx, &[1099.0, 0.0, 0.0, 0.0], 1), vec!["v1099"]);
    }

    #[test]
    fn concurrent_searchers_all_succeed() {
        let idx = Arc::new(build(8, Quant::F32, Metric::Cos, 16));
        for i in 0..200 {
            add(
                &idx,
                &format!("v{i}"),
                &[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
            );
        }

        let failures = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for t in 0..16 {
            let idx = Arc::clone(&idx);
            let failures = Arc::clone(&failures);
            handles.push(std::thread::spawn(move || {
                let q = crate::handler::quant::encode(
                    &[t as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
                    Quant::F32,
                );
                for _ in 0..50 {
                    if idx.search(&q, 5, None).is_err() {
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[ignore]
    fn scaling_probe_raw_usearch() {
        const DIM: usize = 1024;
        const PER_THREAD: usize = 2_000;

        fn run(threads: usize) -> f64 {
            let options = ::usearch::IndexOptions {
                dimensions: DIM,
                metric: Metric::Cos.kind(),
                quantization: ScalarKind::F32,
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let index = Index::new(&options).unwrap();
            index
                .reserve_capacity_and_threads(threads * PER_THREAD + 16, THREAD_SLOTS)
                .unwrap();
            let start = std::time::Instant::now();
            std::thread::scope(|scope| {
                for t in 0..threads {
                    let index = &index;
                    scope.spawn(move || {
                        for i in 0..PER_THREAD {
                            let coords: Vec<f32> = (0..DIM)
                                .map(|d| ((t * 7919 + i * 31 + d) % 997) as f32)
                                .collect();
                            index.add((t * PER_THREAD + i) as u64, &coords).unwrap();
                        }
                    });
                }
            });
            (threads * PER_THREAD) as f64 / start.elapsed().as_secs_f64()
        }

        let one = run(1);
        let four = run(4);
        println!(
            "raw usearch    1 thread: {one:8.0}/s   4 threads: {four:8.0}/s   scaling x{:.2}",
            four / one
        );
    }

    /// Raw usearch, adds racing searches on one small graph -- the shape one
    /// shard sees. If this crashes, the null-vector segfault is usearch's own
    /// add/search race and not this crate's bookkeeping.
    #[test]
    #[ignore]
    fn usearch_add_search_race() {
        for round in 0..200 {
            let options = ::usearch::IndexOptions {
                dimensions: 4,
                metric: Metric::L2.kind(),
                quantization: ScalarKind::F32,
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let index = Index::new(&options).unwrap();
            index.reserve_capacity_and_threads(4096, 16).unwrap();
            std::thread::scope(|scope| {
                for t in 0..4u64 {
                    let index = &index;
                    scope.spawn(move || {
                        for i in 0..200u64 {
                            index
                                .add(t * 1000 + i, &[t as f32, i as f32, 0.0, 0.0])
                                .unwrap();
                        }
                    });
                }
                for _ in 0..4 {
                    let index = &index;
                    scope.spawn(move || {
                        for i in 0..200u64 {
                            let _ = index.search(&[i as f32, 1.0, 0.0, 0.0], 3).unwrap();
                        }
                    });
                }
            });
            if round % 50 == 0 {
                println!("round {round}");
            }
        }
        println!("no crash in 200 rounds");
    }

    /// What sharding costs in memory: the same vectors in one raw usearch
    /// graph, then in Shards, with both allocators reported side by side.
    #[test]
    fn searches_racing_the_first_add_do_not_crash() {
        for _ in 0..200 {
            let idx = Arc::new(build(4, Quant::F32, Metric::L2, 16));
            let mut handles = Vec::new();

            for t in 0..2u64 {
                let idx = Arc::clone(&idx);
                handles.push(std::thread::spawn(move || {
                    for i in 0..50 {
                        add(&idx, &format!("k{t}-{i}"), &[t as f32, i as f32, 0.0, 0.0]);
                    }
                }));
            }
            for _ in 0..4 {
                let idx = Arc::clone(&idx);
                handles.push(std::thread::spawn(move || {
                    for i in 0..50 {
                        let q =
                            crate::handler::quant::encode(&[i as f32, 0.0, 0.0, 0.0], Quant::F32);
                        idx.search(&q, 3, None).unwrap();
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        }
    }

    /// Adds and searches overlapping under a load that never lets up: a bulk
    /// insert running flat out must not hold queries up, and the two must not
    /// crash when they overlap. Nothing serialises them any more, so this is
    /// the soak that says so.
    #[test]
    fn a_bulk_load_does_not_starve_searches() {
        let idx = Arc::new(build(4, Quant::F32, Metric::L2, 16));
        for i in 0..200 {
            add(&idx, &format!("seed{i}"), &[i as f32, 0.0, 0.0, 0.0]);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let searched = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for t in 0..4 {
            let idx = Arc::clone(&idx);
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                let mut i = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    add(
                        &idx,
                        &format!("bulk{t}-{i}"),
                        &[t as f32, i as f32, 1.0, 0.0],
                    );
                    i += 1;
                }
            }));
        }

        for _ in 0..2 {
            let idx = Arc::clone(&idx);
            let searched = Arc::clone(&searched);
            handles.push(std::thread::spawn(move || {
                for i in 0..300 {
                    let q = crate::handler::quant::encode(&[i as f32, 0.0, 0.0, 0.0], Quant::F32);
                    idx.search(&q, 5, None).unwrap();
                    searched.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // the searchers finish on their own; the adders run until told
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while searched.load(Ordering::Relaxed) < 600 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            searched.load(Ordering::Relaxed),
            600,
            "searches did not get through a bulk load"
        );
    }

    /// What sharding costs a search: one raw graph against
    /// Shards, single-threaded latency and four-thread throughput.
    #[test]
    #[ignore]
    fn search_cost() {
        const DIM: usize = 1024;
        const N: usize = 60_000;
        const K: usize = 100;
        const QUERIES: usize = 400;

        fn vector(i: usize) -> Vec<f32> {
            (0..DIM).map(|d| ((i * 31 + d) % 997) as f32).collect()
        }
        fn query(i: usize) -> Vec<f32> {
            (0..DIM).map(|d| ((i * 71 + d * 3) % 997) as f32).collect()
        }

        let options = ::usearch::IndexOptions {
            dimensions: DIM,
            metric: Metric::Cos.kind(),
            quantization: ScalarKind::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            multi: false,
        };

        let one = Index::new(&options).unwrap();
        one.reserve_capacity_and_threads(N + THREAD_SLOTS, THREAD_SLOTS)
            .unwrap();
        for i in 0..N {
            one.add(i as u64, &vector(i)).unwrap();
        }
        let many = Shards::new(&options).unwrap();
        many.reserve_capacity_and_threads(N, THREAD_SLOTS).unwrap();
        for i in 0..N {
            many.add(i as u64, &vector(i)).unwrap();
        }

        let run = |threads: usize, sharded: bool| -> f64 {
            let start = std::time::Instant::now();
            std::thread::scope(|scope| {
                for t in 0..threads {
                    let one = &one;
                    let many = &many;
                    scope.spawn(move || {
                        for i in 0..QUERIES {
                            let q = query(t * QUERIES + i);
                            if sharded {
                                many.search(&q, K).unwrap();
                            } else {
                                one.search(&q, K).unwrap();
                            }
                        }
                    });
                }
            });
            (threads * QUERIES) as f64 / start.elapsed().as_secs_f64()
        };

        for (label, sharded) in [("one graph", false), ("sharded ", true)] {
            let s1 = run(1, sharded);
            let s4 = run(4, sharded);
            println!(
                "{label}   1 thread: {s1:8.0} q/s ({:6.2} ms)   4 threads: {s4:8.0} q/s   scaling x{:.2}",
                1000.0 / s1,
                s4 / s1
            );
        }
    }

    /// Four threads, each with an index of its own -- no shared state at all.
    /// What this measures is the machine: if even fully independent indexes
    /// stop at x1.5, the ceiling is memory bandwidth / cores, not locks.
    #[test]
    #[ignore]
    fn scaling_probe_independent_indexes() {
        const DIM: usize = 1024;
        const PER_THREAD: usize = 2_000;

        fn make() -> Index {
            let options = ::usearch::IndexOptions {
                dimensions: DIM,
                metric: Metric::Cos.kind(),
                quantization: ScalarKind::F32,
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let index = Index::new(&options).unwrap();
            index
                .reserve_capacity_and_threads(PER_THREAD + 16, 4)
                .unwrap();
            index
        }

        // one thread, one index: the baseline
        let index = make();
        let start = std::time::Instant::now();
        for i in 0..PER_THREAD {
            let coords: Vec<f32> = (0..DIM).map(|d| ((i * 31 + d) % 997) as f32).collect();
            index.add(i as u64, &coords).unwrap();
        }
        let one = PER_THREAD as f64 / start.elapsed().as_secs_f64();

        // four threads, four indexes: nothing shared
        let indexes: Vec<Index> = (0..4).map(|_| make()).collect();
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            for (t, index) in indexes.iter().enumerate() {
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        let coords: Vec<f32> = (0..DIM)
                            .map(|d| ((t * 7919 + i * 31 + d) % 997) as f32)
                            .collect();
                        index.add(i as u64, &coords).unwrap();
                    }
                });
            }
        });
        let four = (4 * PER_THREAD) as f64 / start.elapsed().as_secs_f64();
        println!(
            "independent    1 thread: {one:8.0}/s   4 threads: {four:8.0}/s   scaling x{:.2}",
            four / one
        );
    }
}
