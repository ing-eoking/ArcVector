use std::cell::RefCell;
use std::fmt::Write as _;

use super::coords::coord_vectors;
use crate::command::filter::Filter;
use crate::command::request::{Sim, SimKey};
use crate::error::{Error, Reply, Result};
use crate::handler::access::for_read;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry::VectorIndex;
use crate::handler::usearch::{Accept, AnnIndex};

/// 검색 한 건이 훑기에 넘기는 만료 키의 상한.
///
/// 쌓이는 것은 순회가 실제로 밟은 것뿐이라 보통 0이다. 상한은 오래 놀던
/// 인덱스의 첫 검색이 수백 개를 만났을 때 그 한 건이 목록을 통째로 떠안지
/// 않게 한다. 남은 것은 그래프에 그대로이니 다음 검색이 다시 올린다.
const REAP_BATCH: usize = 64;

/// 나중에 물어볼 키를 적어둔다.
///
/// 자리를 못 잡으면 그냥 지나간다. 정리는 최선을 다하는 일이지 검색이 실패할
/// 이유가 아니고, 메모리가 없다는 것이야말로 여기서 더 할당하면 안 된다는 뜻이다.
///
/// UTF-8이 아니면 적지 않는다. 그래프에 선 노드의 키가 그럴 수는 없다 --
/// `key::parse`가 UTF-8이 아닌 키를 못 읽고, 그러면 링크 콜백이 그 아이템을
/// 거절해 애초에 그래프에 들어가지 않는다. 그래도 `from_utf8_lossy`로 때우지
/// 않는 것은, 그 함수가 읽을 수 없는 바이트를 U+FFFD로 바꿔 **원래와 다른 키**를
/// 만들어 내기 때문이다. 정리하려다 엉뚱한 키를 묻느니 그 하나를 빠뜨리는 편이
/// 낫다.
fn note_expired(keys: &RefCell<Vec<String>>, key: &[u8]) {
    let Ok(key) = std::str::from_utf8(key) else {
        return;
    };
    let mut keys = keys.borrow_mut();
    if keys.len() >= REAP_BATCH || keys.try_reserve(1).is_err() {
        return;
    }
    keys.push(key.to_owned());
}

/// accept 콜백이 한 노드에 내린 판정.
///
/// `Option<Option<String>>`이 아니라 이것인 이유는, 바깥 `None`이 이미
/// [`Store::with_item_as_of`]의 "읽을 수 없다"로 쓰이고 있어서다.
enum Verdict {
    /// 받는다. 필터가 읽어 둔 attr이 있으면 들려 보낸다 -- 렌더가 같은 값을
    /// 다시 읽지 않도록.
    Take(Option<String>),
    /// 필터가 걸렀다.
    Drop,
}

fn judged_attr(judged: &RefCell<Vec<(u64, String)>>, key: u64) -> Option<String> {
    let judged = judged.borrow();
    judged
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, a)| a.clone())
}

/// The line that opens one query's hits.
///
/// A graph still being rebuilt answers from the part of itself that exists,
/// which is not the nearest neighbours — it is the nearest of what has been
/// added so far. `PARTIAL_VECTORS` names that, and carries the counts so the
/// caller can judge how far off the answer may be.
fn query_header(hits: usize, rebuild: Option<crate::error::Rebuild>) -> String {
    match rebuild {
        None => format!("VECTORS {hits}\r\n"),
        Some(p) => format!("PARTIAL_VECTORS {hits} {}/{}\r\n", p.done, p.total),
    }
}

#[allow(clippy::too_many_arguments)]
fn similar(
    store: &Store,
    index: &VectorIndex,
    query: &[u8],
    k: usize,
    filter: Option<&Filter>,
    with_attr: bool,
    out: &mut String,
) -> Result<()> {
    let layout = index.ann.layout;
    // 그래프에 있는 것보다 많이 돌려줄 수는 없다. 상한이 없으면
    // `vsearch ... 2147483647`이 그만한 자리를 잡으라고 시킨다.
    let k = k.min(index.ann.len().max(1));

    // 쿼리 하나에 한 번. 노드마다 다시 물으면 순회 중에 시계가 움직여, 앞에서
    // 받은 벡터와 같은 것을 뒤에서 거절할 수 있다.
    let now = crate::server::current_time();

    let judged: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
    // 순회가 밟고 지나간 만료 벡터의 키. 검색이 끝나면 훑기에 넘긴다.
    let expired: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let accepted = |key: u64| -> bool {
        // **역참조보다 먼저.** 자리표 키는 아이템 주소가 아니라 그저 홀수인
        // 표식이라, 포인터로 알고 따라가면 그 자리에서 죽는다. 아직 쓰이지
        // 않은 벡터이므로 답에서도 빠지는 것이 맞다.
        if AnnIndex::is_staged(key) {
            return false;
        }

        // 순회 중 방문하는 노드마다 엔진 메모리를 따라간다. 콜백이 거절하면
        // 엔진이 그 자리에서 해제하므로, 가드 없이 읽으면 해제된 메모리를 본다.
        let Some(_touching) = index.ann.touch_for_deref() else {
            return false;
        };

        // 만료는 게으르다. 아무도 그 키를 찾지 않으면 `do_item_get`이 돌지
        // 않아 unlink도 안 되고, 아이템은 그래프에 그대로 남는다. 여기서
        // 거르지 않으면 vsim이 만료된 벡터를 돌려준다.
        let Some(Verdict::Take(attr)) =
            store.with_item_as_of(key, now, |is_expired, item_key, value| {
                if is_expired {
                    // 답에서 빼는 것으로 끝내지 않는다. 키를 적어 두면 훑기가
                    // 엔진에 물어보고, 거기서 나는 unlink가 노드도 참조도
                    // 정리한다. **조회 스레드는 키를 적기만 한다.**
                    note_expired(&expired, item_key);
                    return Verdict::Drop;
                }
                let Some(filter) = filter else {
                    // 필터가 없으면 살아 있다는 것이 곧 판정이다.
                    return Verdict::Take(None);
                };
                match layout.attr_of(value) {
                    Ok(attr) if filter.matches(attr) => {
                        Verdict::Take(Some(String::from_utf8_lossy(attr).into_owned()))
                    }
                    _ => Verdict::Drop,
                }
            })
        else {
            // 읽을 수 없거나, 만료됐거나, 필터가 걸렀다.
            return false;
        };

        let Some(attr) = attr else {
            // 필터가 없어 렌더에 넘겨줄 attr이 없다.
            return true;
        };

        let mut judged = judged.borrow_mut();

        if judged.try_reserve(1).is_err() {
            return false;
        }
        judged.push((key, attr));
        true
    };

    // 필터가 없어도 건다. 만료 판정이 필터와 같은 자리에서 나야, usearch가
    // 거절당한 만큼 더 방문해 k를 채운다 -- 결과를 받은 뒤에 걸러내면 그 자리를
    // 메울 방법이 없다.
    let accept: Option<Accept> = Some(&accepted as Accept);

    let named = index.ann.search(query, k, accept)?;

    // 순회가 끝났으니 넘긴다. 보통 비어 있다.
    crate::handler::access::sweep::reap_later(std::mem::take(&mut *expired.borrow_mut()));

    let mut rendered = Vec::with_capacity(named.len().min(k));
    for (key, id, distance) in named {
        if rendered.len() == k {
            break;
        }
        // accept가 이미 거르지만, 그것이 안 불리는 길이 있으면 여기가 마지막
        // 문이다. 자리표의 `id`는 아이템에서 읽은 것이 아니라 뜻이 없다.
        if AnnIndex::is_staged(key) {
            continue;
        }

        let attr = if !with_attr {
            None
        } else if let Some(attr) = judged_attr(&judged, key) {
            Some(attr)
        } else {
            let stored = match store
                .get_kv(&crate::trigger::key::vector_key(&index.name, &id))
                .and_then(|body| {
                    layout
                        .attr_of(&body)
                        .map(<[u8]>::to_vec)
                        .map_err(|_| StoreError::CorruptElement)
                }) {
                Ok(stored) => stored,

                // This vector's key, not the index's: a delete that landed
                // between `resolve` and here. One hit goes, the query stands.
                Err(StoreError::KeyGone) => continue,

                Err(StoreError::CorruptElement) => {
                    // 그래프에서 빼지 않는다. 이 아이템이 실제로 사라질 때
                    // unlink 이벤트가 오고, 그때 콜백이 노드를 뺀다.
                    eprintln!(
                        "ArcVector: element '{id}' of index '{}' is unreadable",
                        index.name
                    );
                    return Err(StoreError::CorruptElement.into());
                }
                Err(_) => continue,
            };
            Some(String::from_utf8_lossy(&stored).into_owned())
        };
        rendered.push((
            id.to_owned(),
            index.ann.metric.score(distance, layout.dim),
            attr,
        ));
    }

    out.push_str(&query_header(rendered.len(), index.rebuilding_progress()));
    for (id, score, attr) in rendered {
        match attr {
            // attr은 공백 없는 JSON 한 덩어리라 길이를 앞세울 필요가 없다.
            Some(attr) => {
                let _ = write!(out, "{id} {score} {attr}\r\n");
            }
            None => {
                let _ = write!(out, "{id} {score}\r\n");
            }
        }
    }
    Ok(())
}

pub fn vsim_vector(store: &Store, spec: &Sim, body: &[u8]) -> Result<Reply> {
    let Sim {
        index: name,
        k,
        filter,
        with_attr,
    } = spec;
    let (k, with_attr) = (*k, *with_attr);
    let filter = filter.as_ref();

    let index = for_read(store, name)?;
    let layout = index.ann.layout;

    let mut out = String::new();
    for query in coord_vectors(body, layout.dim, "query")?.iter() {
        let quantized = quant::encode(query, layout.quant);
        similar(store, &index, &quantized, k, filter, with_attr, &mut out)?;
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

pub fn vsim_key(store: &Store, spec: &SimKey) -> Result<Reply> {
    let SimKey {
        index: name,
        key,
        k,
        filter,
        with_attr,
    } = spec;
    let (k, with_attr, filter) = (*k, *with_attr, filter.as_ref());

    let index = for_read(store, name)?;
    let addr = match store.hold_kv(&crate::trigger::key::vector_key(name, key)) {
        Ok(addr) => addr,
        // The query vector's own key. Gone means gone: this is a NOT_FOUND for
        // the query, never a verdict on the index.
        Err(StoreError::KeyGone) => return Ok(Reply::NotFound),
        Err(e) => return Err(e.into()),
    };

    // The graph may not hold this key yet, but the item always carries the
    // vector: read it from there so `vsim KEY` works either way.
    let layout = index.ann.layout;
    let query = match index.ann.vector_of(addr)? {
        Some(query) => query,
        None => store
            .with_item_at(addr, |_key, value| {
                layout.vector_of(value).map(<[u8]>::to_vec)
            })
            .flatten()
            .ok_or(Error::Unreadable)?,
    };
    // Only taken to read the query out; the graph keeps its own.
    store.release_items(&[addr]);

    let mut out = String::new();
    similar(store, &index, &query, k, filter, with_attr, &mut out)?;
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Rebuild;

    #[test]
    fn a_search_notes_at_most_one_batch_of_expired_keys() {
        let keys = RefCell::new(Vec::new());
        for i in 0..REAP_BATCH + 10 {
            note_expired(&keys, format!("arcus_event{{ix}}:v{i}").as_bytes());
        }

        // 남은 것은 그래프에 그대로이니 다음 검색이 다시 올린다. 한 건이
        // 목록을 통째로 떠안는 것보다 낫다.
        assert_eq!(keys.borrow().len(), REAP_BATCH);
        assert_eq!(keys.borrow()[0], "arcus_event{ix}:v0", "앞에서부터 담는다");
    }

    #[test]
    fn a_key_that_is_not_utf8_is_left_alone_rather_than_mangled() {
        // 닿을 수 없는 자리다 -- `key::parse`가 UTF-8이 아닌 키를 거부하므로
        // 그런 아이템은 링크에서 거절돼 그래프에 없다. 고정해 두는 것은
        // `from_utf8_lossy`로 되돌아가지 않기 위해서다: 그것은 못 읽은 바이트를
        // U+FFFD로 바꿔, 있지도 않은 키를 `get`하게 만든다.
        let keys = RefCell::new(Vec::new());
        note_expired(&keys, b"arcus_event{ix}:\xff\xfe");
        assert!(keys.borrow().is_empty());
    }

    #[test]
    fn a_whole_index_writes_the_plain_header() {
        assert_eq!(query_header(5, None), "VECTORS 5\r\n");
    }

    #[test]
    fn a_half_built_index_says_what_it_answered_from() {
        let half = Rebuild {
            done: 693_000,
            total: 999_000,
        };
        assert_eq!(
            query_header(5, Some(half)),
            "PARTIAL_VECTORS 5 693000/999000\r\n"
        );
    }

    #[test]
    fn a_partial_query_that_found_nothing_still_reports_progress() {
        let just_begun = Rebuild {
            done: 0,
            total: 999_000,
        };
        assert_eq!(
            query_header(0, Some(just_begun)),
            "PARTIAL_VECTORS 0 0/999000\r\n"
        );
    }
}
