use std::collections::{HashMap, TryReserveError};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use crate::handler::access::sweep;
use crate::handler::usearch::AnnIndex;

pub const SERVING: u8 = 0;

pub const DRAINING: u8 = 1;

pub const COLD: u8 = 2;

pub const FILLING: u8 = 3;

pub const BUILDING: u8 = 4;

static CLOCK: AtomicU64 = AtomicU64::new(1);

pub fn now() -> u64 {
    CLOCK.load(Ordering::Acquire)
}

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,

    pub maxcount: u32,
    state: AtomicU8,

    published: AtomicU64,

    /// 복구가 이미 맡겨졌다. 두 번 맡기는 것을 막는 표다.
    recovering: AtomicBool,
}

impl VectorIndex {
    pub fn ours(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        let index = Self::new(name, ann, maxcount, SERVING);
        index.mark_serving();
        index
    }

    pub fn rebuilding(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        Self::new(name, ann, maxcount, COLD)
    }

    pub fn building(name: String, ann: AnnIndex, maxcount: u32) -> Self {
        Self::new(name, ann, maxcount, BUILDING)
    }

    fn new(name: String, ann: AnnIndex, maxcount: u32, state: u8) -> Self {
        Self {
            name,
            ann,
            maxcount,
            state: AtomicU8::new(state),
            published: AtomicU64::new(0),
            recovering: AtomicBool::new(false),
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub fn publish(&self) {
        self.published
            .store(CLOCK.fetch_add(1, Ordering::AcqRel), Ordering::Release);
    }

    fn published_at(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    pub fn is_rebuilding(&self) -> bool {
        self.state() != SERVING
    }

    /// Always `None`: a graph is never rebuilt any more, so a read is never
    /// partial. Kept so `Reply` still has the shape its callers expect.
    pub fn rebuilding_progress(&self) -> Option<crate::error::Rebuild> {
        None
    }

    /// Marks this index ready to serve.
    ///
    /// There is no ownership token any more: an index is in this registry
    /// because the trigger callback saw its metadata item linked, and no other
    /// process can have put it there.
    /// 복구 표를 끊는다. 이미 누가 끊었으면 `false`.
    ///
    /// 맡기는 쪽과 sweeper의 안전망이 동시에 같은 인덱스를 집을 수 있어서,
    /// 일감을 큐에 넣기 **전에** 여기서 하나만 통과시킨다.
    pub(crate) fn claim_recovery(&self) -> bool {
        self.recovering
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// 표를 돌려준다. 복구가 끝났거나 맡기지 못했을 때 부른다.
    pub(crate) fn release_recovery(&self) {
        self.recovering.store(false, Ordering::Release);
    }

    pub(crate) fn mark_serving(&self) {
        self.state.store(SERVING, Ordering::Release);
    }

    // The unit tests drive state transitions through this directly; nothing in
    // the request path does any more, now that an index is either registered by
    // the trigger or absent.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(super) fn mark_state(&self, now: u8) {
        self.state.store(now, Ordering::Release);
    }
}

static INDICES: LazyLock<RwLock<HashMap<String, Arc<VectorIndex>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// `vdrop`한 인덱스. 아직 버리지 않는다.
///
/// `flush`는 프리픽스의 아이템을 그 자리에서 다 빼지 않는다 -- LRU를 훑다가
/// 방금 건드린 것 너머에서 멈추고, 나머지는 누가 손댈 때 무효화된다
/// (`items.c`). 그 늦은 unlink가 올 때 콜백은 "이 주소를 내가 받았나"를 답해야
/// 하는데, usearch에는 키를 열거하는 방법이 없어서 그래프를 버리면 그 질문에
/// 답할 길이 사라진다. 장부를 따로 두는 대신 그래프를 남겨둔다.
///
/// 그래프가 비면 sweeper가 버린다. 그때까지 남아 있는 것은 그동안 실제로
/// 참조를 쥐고 있기 때문이라, 이 목록의 크기는 정확한 값이다.
static DROPPED: LazyLock<RwLock<Vec<Arc<VectorIndex>>>> = LazyLock::new(|| RwLock::new(Vec::new()));

fn read() -> std::sync::RwLockReadGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.read().unwrap_or_else(PoisonError::into_inner)
}

fn write() -> std::sync::RwLockWriteGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.write().unwrap_or_else(PoisonError::into_inner)
}

pub fn get(name: &str) -> Option<Arc<VectorIndex>> {
    read().get(name).cloned()
}

/// 명령이 볼 수 있는 인덱스.
///
/// 세우는 중(`BUILDING`)이거나 비워지는 중(`DRAINING`)이면 없는 것과 같다.
/// `get`은 그래도 찾아준다 -- 트리거 콜백은 그 사이에도 그래프에 물어봐야 한다.
/// 이 주소를 들고 있는 그래프. 버려지는 중인 것까지 본다.
///
/// 주소는 그래프 하나에만 있으므로 답은 유일하다. 같은 이름으로 새 인덱스가
/// 생긴 뒤에도 옛 그래프가 자기 몫을 정확히 답한다.
pub fn holding(name: &str, addr: u64) -> Option<Arc<VectorIndex>> {
    if let Some(index) = get(name)
        && index.ann.holds(addr)
    {
        return Some(index);
    }
    DROPPED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|index| index.name == name && index.ann.holds(addr))
        .cloned()
}

/// 이 이름을 살아 있는 것이든 비워지는 중이든 알고 있나.
pub fn known(name: &str) -> bool {
    contains(name)
        || DROPPED
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|index| index.name == name)
}

/// 살아 있는 인덱스와 비워지는 중인 그래프를 모두. sweeper가 훑는 목록이다.
pub fn all() -> Vec<Arc<VectorIndex>> {
    let mut all = indexes().unwrap_or_default();
    all.extend(
        DROPPED
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned(),
    );
    all
}

/// 다 비운 그래프를 버린다. sweeper가 매 틱 부른다.
pub fn reap_drained() {
    let mut dropped = DROPPED.write().unwrap_or_else(PoisonError::into_inner);
    dropped.retain(|index| !index.ann.is_empty());
}

pub fn serving(name: &str) -> Option<Arc<VectorIndex>> {
    get(name).filter(|index| index.state() == SERVING)
}

pub fn contains(name: &str) -> bool {
    read().contains_key(name)
}

pub fn put(
    index: VectorIndex,
) -> Result<(Arc<VectorIndex>, Option<Arc<VectorIndex>>), TryReserveError> {
    sweep::ensure_sweeper();
    crate::handler::access::pool::ensure_pool();
    index.ann.set_bell();
    let mut reg = write();
    reg.try_reserve(1)?;
    let index = Arc::new(index);
    let previous = reg.insert(index.name.clone(), Arc::clone(&index));
    Ok((index, previous))
}

pub fn unput(ours: &VectorIndex, previous: Option<Arc<VectorIndex>>) {
    let mut reg = write();
    let name = ours.name.as_str();
    if !reg
        .get(name)
        .is_some_and(|c| std::ptr::eq(Arc::as_ptr(c), ours))
    {
        return;
    }
    match previous {
        Some(index) => {
            if let Some(slot) = reg.get_mut(name) {
                *slot = index;
            }
        }
        None => {
            let evicted = reg.remove(name);
            drop(reg);
            keep_until_empty(evicted);
        }
    }
}

pub fn remove_if_stale(name: &str, stamp: u64) -> bool {
    let mut reg = write();
    match reg.get(name) {
        Some(current) => {
            if current.state() == BUILDING || current.published_at() >= stamp {
                return false;
            }
            let evicted = reg.remove(name);
            drop(reg);
            keep_until_empty(evicted);
            true
        }
        None => false,
    }
}

pub fn insert_or_get(index: VectorIndex) -> Result<(Arc<VectorIndex>, bool), TryReserveError> {
    sweep::ensure_sweeper();
    index.ann.set_bell();
    let mut reg = write();
    reg.try_reserve(1)?;
    let mut inserted = false;
    let entry = reg.entry(index.name.clone()).or_insert_with(|| {
        inserted = true;
        index.publish();
        Arc::new(index)
    });
    Ok((Arc::clone(entry), inserted))
}

pub fn remove(name: &str) -> bool {
    let evicted = write().remove(name);
    let had = evicted.is_some();
    keep_until_empty(evicted);
    had
}

/// 레지스트리를 떠난 그래프를 비워질 때까지 들고 있는다.
///
/// 버리면 안 된다. 이 그래프는 아직 자기가 쥔 아이템들의 참조를 들고 있고,
/// 그것들이 언제 빠질지는 `flush`가 아니라 누가 건드리느냐가 정한다. 그때
/// 콜백이 "이 주소를 내가 받았나"를 물어볼 곳이 여기다.
pub fn keep_until_empty(index: Option<Arc<VectorIndex>>) {
    let Some(index) = index else { return };
    let mut dropped = DROPPED.write().unwrap_or_else(PoisonError::into_inner);
    if dropped.try_reserve(1).is_ok() {
        dropped.push(index);
    }
}

pub fn remove_observed(name: &str, observed: &VectorIndex) -> bool {
    let mut reg = write();
    match reg.get(name) {
        Some(current) if std::ptr::eq(Arc::as_ptr(current), observed) => {
            let evicted = reg.remove(name);
            drop(reg);
            keep_until_empty(evicted);
            true
        }
        _ => false,
    }
}

/// `Err` means the listing could not be built under memory pressure -- not
/// that the registry is empty. Callers that would act on the difference
/// (the replication master, telling a replica what exists) must not collapse
/// the two; callers that treat "nothing to do this round" as harmless either
/// way (the sweeper) may flatten it with `unwrap_or_default`.
pub fn indexes() -> Result<Vec<Arc<VectorIndex>>, TryReserveError> {
    let reg = read();
    let mut all = Vec::new();
    all.try_reserve(reg.len())?;
    all.extend(reg.values().cloned());
    Ok(all)
}

pub fn snapshot() -> Vec<Arc<VectorIndex>> {
    let mut all: Vec<Arc<VectorIndex>> = read()
        .values()
        .filter(|index| !index.is_rebuilding())
        .cloned()
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::arcus::element::Layout;
    use crate::handler::quant::Quant;
    use crate::handler::usearch::Metric;

    fn index(name: &str) -> VectorIndex {
        let ann = AnnIndex::new(
            Layout::new(4, Quant::F32),
            Metric::L2,
            0,
            0,
            0,
            Arc::new(crate::handler::arcus::engine::ItemElements),
        )
        .expect("build the graph");
        VectorIndex::ours(name.to_owned(), ann, 8)
    }

    fn building(name: &str) -> VectorIndex {
        let made = index(name);
        made.mark_state(BUILDING);
        made
    }

    #[test]
    fn a_verdict_older_than_the_entry_releases_nothing() {
        let name = "registry-test-late-verdict";
        let stamp = now();

        let (registered, _) = put(index(name)).expect("claim the name");
        registered.publish();

        assert!(
            !remove_if_stale(name, stamp),
            "a verdict from before the entry existed must release nothing"
        );
        assert!(get(name).is_some(), "the new index survives");
        remove(name);
    }

    #[test]
    fn an_entry_still_being_built_is_never_released() {
        let name = "registry-test-unpublished";
        let (registered, _) = put(building(name)).expect("claim the name");

        assert!(
            !remove_if_stale(name, now()),
            "an entry whose Map is still being written must survive any verdict"
        );
        registered.publish();
        registered.mark_serving();
        assert!(
            remove_if_stale(name, now()),
            "once it stands on its Map it is releasable like any other"
        );
    }

    #[test]
    fn unput_leaves_a_displacing_entry_alone() {
        let name = "registry-test-unput-displaced";
        let (mine, _) = put(index(name)).expect("claim the name");
        let (theirs, displaced) = put(index(name)).expect("displace it");
        assert!(
            displaced.is_some_and(|d| Arc::ptr_eq(&d, &mine)),
            "the second put displaces the first"
        );

        unput(&mine, None);

        assert!(
            get(name).is_some_and(|live| Arc::ptr_eq(&live, &theirs)),
            "undoing the displaced entry must not remove the one that displaced it"
        );
        unput(&theirs, None);
        assert!(get(name).is_none(), "the owner of the entry can undo it");
    }

    #[test]
    fn a_stale_release_leaves_a_newly_registered_index_alone() {
        let name = "registry-test-stale-release";
        let (observed, _) = insert_or_get(index(name)).expect("register the first");

        assert!(remove(name));
        let (current, inserted) = insert_or_get(index(name)).expect("register the second");
        assert!(inserted, "the name was free");

        assert!(
            !remove_observed(name, &observed),
            "a stale release must report that it removed nothing"
        );
        assert!(
            get(name).is_some_and(|live| Arc::ptr_eq(&live, &current)),
            "the live index must survive a release meant for its predecessor"
        );
        remove(name);
    }

    #[test]
    fn a_release_for_the_live_index_removes_it() {
        let name = "registry-test-live-release";
        let (live, _) = insert_or_get(index(name)).expect("register");

        assert!(
            remove_observed(name, &live),
            "the observed index is the live one"
        );
        assert!(get(name).is_none(), "the entry is gone");
    }
}
