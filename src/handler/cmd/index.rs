use std::fmt::Write as _;
use std::sync::Arc;

use crate::command::request::Create;
use crate::error::{Error, Reply, Result};
use crate::handler::access::resolve;
use crate::handler::arcus::element::{IndexConfig, Layout};
use crate::handler::arcus::engine::{self, Store, StoreError};
use crate::handler::quant::Quant;
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::AnnIndex;
use crate::trigger::key;

pub fn vcreate(store: &Store, spec: &Create) -> Result<Reply> {
    let Create {
        index: name,
        dim,
        metric,
        quant,
        ..
    } = *spec;

    let layout = Layout::new(dim, quant);
    check_dimension_fits(store, layout, quant)?;

    let ann = AnnIndex::new(
        layout,
        metric,
        spec.connectivity,
        spec.expansion_add,
        spec.expansion_search,
        std::sync::Arc::new(engine::MapElements),
    )?;

    // 메타가 같은 Map의 한 칸을 차지하므로 Map에는 하나 더 잡는다.
    let held = map_size_for(store, spec.maxcount);
    let maxcount = held - 1;
    let config = IndexConfig {
        dim,
        metric,
        quant,
        m: spec.connectivity,
        efc: spec.expansion_add,
        efs: spec.expansion_search,
    };
    let index = VectorIndex::building(name.to_owned(), ann, maxcount);

    let (registered, previous) = match registry::put(index) {
        Ok(pair) => pair,
        Err(e) => {
            return Err(Error::Index(format!(
                "the index registry could not grow: {e}"
            )));
        }
    };

    // 이 이름으로 먼저 도착해 기다리던 벡터가 있으면 여기서 그래프에 넣는다.
    // 보통은 없어서 곧장 지나간다. 메타를 쓰기 **전**에 하는 것은, 링크 콜백이
    // 곧바로 이 인덱스를 서빙으로 올리기 때문이다 -- 그 뒤에 넣으면 반쯤 찬
    // 그래프가 잠깐 조회에 보인다.
    crate::trigger::recover::adopt(&registered);

    // Map을 만들면서 메타를 넣는다. 한 번의 엔진 호출이라, 메타 없는 Map이
    // 복제본에 보이는 순간이 없다.
    let exptime = spec.exptime.map_or(0, crate::server::realtime);
    let settled = store.insert_elem(
        &key::map_key(name),
        key::META_ID,
        &config.encode(),
        false,
        Some(engine::index_attr(held, exptime)),
    );

    match settled {
        Ok(done) if done.created => {
            // 서빙으로 올리는 것은 `on_meta`가 한다. 복제본에는 `vcreate`가
            // 오지 않아 콜백이 유일한 길이므로, 마스터도 같은 길을 타게 둔다.
            if previous.is_some() {
                eprintln!("ArcVector: index '{name}' had no Map; released the graph it stood on");
                registry::keep_until_empty(previous);
            }
            Ok(Reply::Created)
        }

        // 그 키에 메타 없는 Map이 이미 있었다 -- 우리 것이 아니다. 방금 넣은
        // 메타를 도로 뺀다.
        Ok(_) => {
            registry::unput(&registered, previous);
            let _ = store.delete_elem(&key::map_key(name), key::META_ID);
            Ok(Reply::TypeMismatch)
        }

        Err(e) => {
            registry::unput(&registered, previous);
            match e {
                StoreError::ElemExists => already_there(store, name),
                StoreError::BadType => Ok(Reply::TypeMismatch),
                e => Err(e.into()),
            }
        }
    }
}

/// 그 Map에 메타가 이미 있다.
///
/// 우리가 아는 인덱스면 `EXISTS`다. 아니면 그 자리에 우리가 읽지 못한 것이
/// 있다는 뜻이라 `TYPE_MISMATCH`로 답한다.
fn already_there(store: &Store, name: &str) -> Result<Reply> {
    match resolve(store, name) {
        Ok(_) => Ok(Reply::Exists),
        Err(_) => Ok(Reply::TypeMismatch),
    }
}

/// Map에 잡을 칸 수. 메타 한 칸을 더한 것이고, 서버의 `max_map_size`를 넘지
/// 않는다. 이름을 안 했으면 그 상한이다.
fn map_size_for(store: &Store, maxcount: Option<u32>) -> u32 {
    let ceiling = store.max_map_size().max(2);
    maxcount
        .unwrap_or(ceiling)
        .saturating_add(1)
        .clamp(2, ceiling)
}

pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    // 살아 있는 엔트리만 센다. 앞서 드롭돼 비워지는 중인 그래프는 `known`에
    // 잡히지만, 그건 이미 없는 인덱스다 -- 두 번째 `vdrop`은 `NOT_FOUND`다.
    let known = registry::contains(name);

    // Map을 지운다. 그 unlink가 콜백을 부르고, 콜백이 인덱스를 레지스트리에서
    // 내린다 -- 여기서나 복제본에서나 같다. 복제본은 이 명령이 아니라 복제된
    // 삭제를 보기 때문이다.
    //
    // 그래프는 같이 버리지 않는다. 요소들은 엔진의 컬렉션 삭제 스레드가 나중에
    // 지우고, 그때마다 오는 unlink가 `registry::holding`으로 이 그래프를 다시
    // 찾아 참조를 돌려준다.
    let dropped = match store.drop_map(&key::map_key(name)) {
        Ok(()) => true,
        Err(StoreError::KeyGone) => false,
        Err(e) => return Err(e.into()),
    };
    // Map이 없었으면 콜백도 없었으니 엔트리가 아직 있다.
    registry::remove(name);

    Ok(if known || dropped {
        Reply::Dropped
    } else {
        Reply::NotFound
    })
}

fn check_dimension_fits(store: &Store, layout: Layout, quant: Quant) -> Result<()> {
    let limit = store.max_element_bytes() as usize;

    // 메타가 가장 작은 경우(빈 문서)로 잰다. 메타가 큰 요소는 쓸 때 다시 잰다.
    let stored = layout.stored_len(crate::bson::EMPTY.len());
    if stored <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element would be {stored} bytes, over max_element_bytes {limit} \
         (max dimension is {} for quant {quant})",
        Layout::max_dim_for(quant, limit),
    )))
}

/// 한 인덱스가 답에 더하는 줄들의 넉넉한 상한.
///
/// `vstats`는 네 줄, `vlist`는 한 줄인데 둘 다 이름이 네 번/한 번 들어간다.
/// 고정부는 필드 이름과 u64 열 자리 남짓이라, 줄당 96바이트면 남는다. 정확할
/// 필요는 없다 -- 모자라면 뒤의 `write!`가 더 늘릴 뿐이고, 여기서 보려는 것은
/// **이만한 답을 지을 메모리가 있느냐**다.
const PER_INDEX_LINE: usize = 96;

fn room_for_rows(out: &mut String, indexes: &[Arc<VectorIndex>], lines: usize) -> Result<()> {
    let want = indexes
        .iter()
        .map(|index| (index.name.len() + PER_INDEX_LINE) * lines)
        .try_fold(0usize, |sum, n| sum.checked_add(n))
        .ok_or_else(|| Error::Index("listing does not fit in memory".into()))?;
    crate::room::reserve_str(out, want)
}

pub fn vstats() -> Result<Reply> {
    let indexes = registry::snapshot().map_err(|_| Error::Index("no room to list".into()))?;

    let mut vectors = 0usize;
    let mut held_bytes = 0usize;
    let mut used_bytes = 0usize;
    for index in &indexes {
        vectors += index.ann.len();
        held_bytes += index.ann.held_bytes();
        used_bytes += index.ann.used_bytes();
    }

    let mut out = String::new();
    let _ = write!(
        out,
        "STAT indexes {}\r\n\
         STAT vectors {vectors}\r\n\
         STAT index_held_bytes {held_bytes}\r\n\
         STAT index_used_bytes {used_bytes}\r\n\
         STAT vectors_awaiting_metadata {}\r\n",
        indexes.len(),
        crate::trigger::waiting::count(),
    );

    // 인덱스별 줄을 **같은 버퍼에** 잇는다. 따로 지었다가 붙이면 두 벌이 동시에
    // 떠서 피크가 두 배가 된다. 자리는 먼저 잡고, 없으면 답을 짓지 않는다.
    room_for_rows(&mut out, &indexes, 4)?;
    for index in &indexes {
        let name = &index.name;
        let _ = write!(
            out,
            "STAT {name}:vectors {}\r\n\
             STAT {name}:index_held_bytes {}\r\n\
             STAT {name}:index_used_bytes {}\r\n\
             STAT {name}:reserved {}\r\n",
            index.ann.len(),
            index.ann.held_bytes(),
            index.ann.used_bytes(),
            index.ann.reserved()
        );
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

pub fn vlist() -> Result<Reply> {
    let indexes = registry::snapshot().map_err(|_| Error::Index("no room to list".into()))?;

    let mut out = String::new();
    room_for_rows(&mut out, &indexes, 1)?;
    for index in &indexes {
        let layout = &index.ann.layout;
        let _ = writeln!(
            out,
            "INDEX {} dim={} quant={} metric={} count={} maxcount={}\r",
            index.name,
            layout.dim,
            layout.quant,
            index.ann.metric,
            index.ann.len(),
            index.maxcount,
        );
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
