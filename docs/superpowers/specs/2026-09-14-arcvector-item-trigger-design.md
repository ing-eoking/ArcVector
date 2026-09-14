# 아이템 트리거: 복제를 arcus 자신에게 맡긴다

설계 기록. 상태: **제안**.

[2026-09-04-arcvector-index-replication-design.md](2026-09-04-arcvector-index-replication-design.md)를
**대체한다.** 그 설계가 만든 델타 채널(소켓·큐·라이터 스레드·프레임 코덱·역할 하트비트)은
전부 없어진다.

---

## 1. 무엇이 바뀌었나

arcus-memcached-EE에 `ON_ITEM_TRIGER` 콜백이 생겼다. 키가 `arcus trig:`로 시작하는
아이템이 link/unlink될 때 엔진이 확장을 불러준다.

```c
typedef enum {
    ...
    ON_ITEM_TRIGER = 5,     /**< An item was triggered */
} ENGINE_EVENT_TYPE;
```

이 한 지점이 이전 설계 전체를 불필요하게 만든다. 이전 설계는 "마스터가 복제본에게 바뀐 id를
알려주는 별도 채널"이었는데, **아이템 자체가 그 알림이 되기 때문이다.**

## 2. 착상 — 트리거가 곧 데이터

벡터를 arcus Map의 원소가 아니라 **KV 아이템 하나**로 저장하고, 그 키가
`arcus trig:`로 시작하게 한다.

그러면 arcus가 그 아이템을 복제본에 복제하는 순간, 복제본의 `do_item_link`가 콜백을 부른다.
콜백이 받는 `hash_item *`에는 **키(인덱스 이름과 id)와 값(벡터)이 둘 다 들어 있다.**
복제본은 아무것도 되읽을 필요가 없다.

이전 설계가 안고 있던 문제들이 존재 이유를 잃는다.

| 이전 설계의 장치 | 왜 있었나 | 왜 없어지나 |
|---|---|---|
| 델타 TCP 채널 | 그래프 변경을 알릴 방법이 없었다 | 아이템 자체가 알림이다 |
| `Pending` 지연 집합 | 델타가 원소보다 먼저 올 수 있었다 | 같은 아이템이라 순서가 성립하지 않는다 |
| `Resync` / `converge` | 델타가 유실될 수 있었다 | 유실 경로가 없다 |
| `Snapshot` | 복제본이 인덱스 이름을 몰랐다 | 메타 KV의 link가 이름을 알려준다 |
| 역할 하트비트 | 마스터만 델타를 보내야 했다 | 아무도 무엇을 보내지 않는다 |
| `AV OWNER` / `vowner` | 복제본이 마스터 주소를 찾아야 했다 | 찾아갈 곳이 없다 |
| 소유권 토큰(`owner`) | 재구축한 그래프가 Map을 주장해야 했다 | 주장할 Map이 없다 |

## 3. C 쪽 변경 (arcus-memcached-EE)

### 3.1 link 훅 — 역할 게이트를 뺀다

`do_item_link()`의 `assoc_insert` 직후. 지금 코드에서 `RP_ROLE_MODE == RP_MODE_SLAVE`
조건만 제거한다.

```c
#ifdef ENABLE_REPLICATION
    if (it->nkey > 11 && memcmp(key, "arcus trig:", 11) == 0) {
        engine->server.callback->perform_callbacks(ON_ITEM_TRIGER,
                                                   (const void *)&ret,
                                                   (const void *)it);
    }
#endif
```

게이트를 빼는 것이 이 설계의 핵심이다. 그러면 **세 가지 경로가 한 코드로 수렴한다.**

| 경로 | 무엇이 link를 부르나 |
|---|---|
| 클라이언트 쓰기 | `vadd`가 부른 `store` |
| 복제 적용 | `item_apply_kv_link` ([replication.c:3228](../../../../arcus-memcached-EE/engines/default/replication.c)) |
| persistence 복구 | `lrec_it_link_redo` → `item_apply_kv_link` ([cmdlogrec.c:314](../../../../arcus-memcached-EE/engines/default/cmdlogrec.c)) |

복제 적용과 복구가 **같은 함수**를 쓰기 때문에, 훅 하나가 세 경우를 다 덮는다. 그 결과
"저장소에 있는 벡터로 그래프를 다시 짓는" 코드(`recovery.rs`의 `hold_all` 경로)가
통째로 필요 없어진다.

### 3.2 unlink 훅 — 새로 넣는다

`do_item_unlink()`에서 아이템이 아직 살아 있는 동안. `event_data`가 `NULL`인 것이
unlink라는 뜻이다.

```c
#ifdef ENABLE_REPLICATION
    if (it->nkey > 11 && memcmp(key, "arcus trig:", 11) == 0) {
        engine->server.callback->perform_callbacks(ON_ITEM_TRIGER,
                                                   NULL,
                                                   (const void *)it);
    }
#endif
```

`cause`로 거르지 않는다. 축출(`ITEM_UNLINK_EVICT`)이든 만료든 `set` 덮어쓰기든,
**아이템이 사라지면 그래프에서도 사라지는 것이 맞다** — 이제 그래프의 벡터는 그 아이템의
복사본이지 그 아이템에 대한 참조가 아니고, 없는 아이템을 가리키는 검색 결과를 돌려주는 것이
축출된 벡터를 그래프에서 빼는 것보다 나쁘다. `set` 덮어쓰기는 unlink 직후 link가 따라오므로
순 효과가 갱신이 된다(§8).

### 3.3 `mc_engine` 대입을 한 줄 올린다

확장은 `load_extension`([memcached.c:16493](../../../../arcus-memcached-EE/memcached.c),
getopt 루프)에서 엔진보다 먼저 적재되므로 콜백 등록은 제때 끝난다. 그런데 persistence 복구는
`init_engine`([memcached.c:16793](../../../../arcus-memcached-EE/memcached.c)) 안의
`cmdlog_mgr_init`([default_engine.c:383](../../../../arcus-memcached-EE/engines/default/default_engine.c))에서
도는 반면 `mc_engine.v1`은 그 **다음 줄**에서 채워진다. `get_server_api()`의
`rv.engine = mc_engine.v0`([memcached.c:15963](../../../../arcus-memcached-EE/memcached.c))이
그때까지 NULL이라, 복구 중에 뜬 콜백은 엔진 vtable에 닿지 못해 `get_item_info`를 부를 수 없다.

```c
    if (!load_engine(settings.engine_path, get_server_api, mc_logger, &engine_handle)) {
        exit(EXIT_FAILURE);
    }
    mc_engine.v1 = (ENGINE_HANDLE_V1 *) engine_handle;   /* 여기로 올린다 */

    if (!init_engine(engine_handle, settings.engine_config, mc_logger)) {
        exit(EXIT_FAILURE);
    }
```

`load_engine`이 돌아온 시점에 핸들과 vtable은 이미 유효하다. `init_engine`은 그 vtable의
`initialize`를 부를 뿐이다.

## 4. 저장 구조

### 4.1 키

```
arcus trig:{<index>}:<id>      벡터 하나
arcus trig:{<index>}:          인덱스 메타 (id가 빈 문자열)
```

- 앞 11바이트 `arcus trig:`가 C 훅의 판별자다.
- `{}`는 arcus의 샤드키 구분자다
  ([cluster_config.c:125](../../../../arcus-memcached-EE/cluster_config.c)의 `get_shard_key`).
  인덱스 이름을 그 안에 넣으면 **한 인덱스의 모든 키가 같은 노드에 모인다.**
  `ARCUS_ENABLE_SHARD_KEY=1`이 필요하다
  ([cluster_config.c:668](../../../../arcus-memcached-EE/cluster_config.c)).
- 키에 공백이 있으므로 **어떤 ASCII 명령도 이 키를 지목할 수 없다.** 클라이언트가 벡터
  아이템을 직접 읽거나 망가뜨릴 수 없다. ArcVector는 엔진 vtable을 직접 부르므로 영향받지 않는다.
- 인덱스 이름은 `}`를 포함할 수 없다. id는 ASCII 키 규칙(공백·제어문자 금지)만 지키면 되고
  `:`를 포함해도 된다 — 파싱은 첫 `{`와 그 뒤 첫 `}`로 이름을 떼고, 이어지는 `:` 다음
  전부를 id로 본다.

### 4.2 값

| 키 | 값 |
|---|---|
| 벡터 KV | 지금의 `Layout` 인코딩 그대로 — 헤더 + attr 128바이트 + 양자화된 벡터 |
| 메타 KV | 지금의 `MetaRecord` — dim, quant, metric, maxcount. **소유자 필드는 뺀다**(§2) |

Map, `AV META` 원소, `probe_map`, `hold_all`, `getattr` 개수는 전부 없어진다.

### 4.3 크기 한계와 개수

값이 Map 원소가 아니라 아이템 본문이 되므로 **상한이 `max_element_bytes`에서
`max_item_size`로 바뀐다.** `Layout::max_dim_for`가 보는 예산이 달라지고, 같은 quant에서
담을 수 있는 차원 수도 달라진다. `Store::max_element_bytes`/`max_map_size`는 쓰이지 않는다.

인덱스의 원소 개수는 이제 **그래프 자신이 답한다**(`ann.len()`). `vstats`가 Map의
`getattr` 개수와 그래프 개수를 비교하던 항목은 비교 대상이 없어지므로 하나로 합친다.

## 5. 콜백 계약

```
on_item_trigger(cookie = hash_item*, type, event_data, cb_data)
    event_data != NULL  →  link
    event_data == NULL  →  unlink
```

콜백은 **`engine->cache_lock`을 쥔 채** 돈다 (`do_item_link`는 `do_store_item` →
`LOCK_CACHE()` 아래에 있다). 따라서 지켜야 할 규칙이 있다.

**허용되는 엔진 호출은 `get_item_info` 하나뿐이다.** 그 함수는 락을 잡지 않는 순수 필드
읽기이고([default_engine.c:2073](../../../../arcus-memcached-EE/engines/default/default_engine.c)),
키·nkey·값·nbytes·타입을 전부 준다. 나머지는 전부 `LOCK_CACHE()`를 지나므로 부르면
즉시 데드락이다.

`get_item_info`는 쿠키를 보지 않으므로 `Store::background()`로 충분하다 — 파킹 연결도
쿠키도 필요 없다.

콜백이 하는 일의 전부:

```
1. get_item_info(it)로 키와 값을 얻는다
2. 키를 파싱한다 → (index, id)
3. id가 비었으면 메타 이벤트:
     link   → MetaRecord를 읽어 registry에 인덱스를 만든다
     unlink → registry에서 빼고 그래프를 놓아준다
   id가 있으면 벡터 이벤트:
     link   → 값에서 벡터와 attr을 복사해 그래프에 넣는다
     unlink → 그래프에서 뺀다
```

큐도 스레드도 없다. HNSW 삽입이 그 자리에서 끝난다.

## 6. 잠금 순서 — 왜 데드락이 없나

콜백은 `cache_lock`을 쥔 채 **그래프 락**을 잡는다. 그러므로 반대 방향이 하나라도 있으면
AB-BA가 된다. 이 설계는 반대 방향을 **전부 없애서** 성립한다.

지금은 반대 방향이 있다. 검색이 `with_attr_at`/`with_vector_at`으로 저장소를 되읽고
([search.rs:57](../../../src/handler/cmd/search.rs),
[search.rs:195](../../../src/handler/cmd/search.rs)), 원소를 놓아주는 `release_held`는
`map_elem_release` → `cache_lock`이다.

**그래프가 자기 복사본을 갖게 하면 그 방향이 사라진다.** 콜백이 벡터와 attr 128바이트를
복사해 넣고, 검색은 엔진을 한 번도 부르지 않는다. 그러면:

```
cache_lock  →  그래프 락        (콜백. 유일한 방향)
그래프 락   →  (순수 계산)      (검색. 엔진을 안 부른다)
```

한 방향뿐이므로 인버전이 불가능하다.

이 결정이 `hold_addr` · `HeldAddr` · `HeldMap` · `release_held` · `id_at` ·
`held_addrs` · `reclaim` · `unclaimed` · `forget_unreadable`을 전부 지운다. 그래프는
주소를 들지 않으므로 놓아줄 것도 없다.

**대가: 벡터가 두 벌 상주한다** — arcus KV에 한 벌, 그래프에 한 벌.

## 7. 명령 경로

모든 쓰기가 같은 모양이다. **명령은 KV를 쓰거나 지울 뿐이고, 그래프는 트리거가 고친다.**
마스터도 예외가 아니다 — 자기 쓰기의 link도 자기 콜백을 부르므로, 명령 핸들러에 그래프를
만지는 코드가 없다.

| 명령 | 하는 일 | 그래프는 |
|---|---|---|
| `vcreate` | 메타 KV를 `add` 시맨틱으로 쓴다 | 그 link의 콜백이 인덱스를 만든다 |
| `vadd` | 양자화 → `Layout` 인코딩 → 벡터 KV를 `set` | 그 link의 콜백이 넣는다 |
| `vdel` | 벡터 KV를 `delete` | 그 unlink의 콜백이 뺀다 |
| `vdrop` | §7.1 | 콜백이 놓아준다 |
| `vsearch` | 그래프만 본다 | 엔진 호출 없음 |

`vadd`가 `STORED`를 돌려줄 때 그래프에는 이미 들어가 있다 — 삽입이 `set` 호출 **안에서**
동기로 끝나기 때문이다. `vadd` 직후 `vsearch`가 보이는 성질이 유지된다.

재진입은 안전하다. 핸들러는 `set`을 부를 때 그래프 락을 쥐고 있지 않다.

### 7.1 `vdrop` — 그래프가 id 목록이다

프리픽스 깊이가 1이라([prefix.c:34](../../../../arcus-memcached-EE/engines/default/prefix.c))
등록되는 프리픽스는 `arcus trig` 하나뿐이고, 따라서 `flush`로 인덱스 하나만 지울 수 없다.

그럴 필요가 없다. **그래프가 그 인덱스의 모든 id를 갖고 있다.** `vdrop`은 그래프를 걸어
id마다 KV를 지우고, 마지막에 메타 KV를 지운다. arcus 쪽 열거는 끝내 필요하지 않다.

```
vdrop idx
  for id in graph.ids():            // 그래프가 곧 목록
      delete_kv("arcus trig:{idx}:" + id)   // 각 unlink가 그래프에서 뺀다
  delete_kv("arcus trig:{idx}:")            // 메타. 인덱스를 놓아준다
```

큰 인덱스에서는 삭제가 원소 수만큼 걸린다. 지금의 `drop_map` 한 번과 달라지는 점이고,
`vdrop`이 드문 명령이라 받아들인다.

## 8. 순서

한 갈래뿐이라 순서 문제가 성립하지 않는다.

- 복제 적용은 cset 시퀀스 순, persistence 복구는 로그 순이다. 마스터에서 `vcreate`가
  `vadd`보다 먼저 커밋됐으면 복제본·복구에서도 그 순서로 link된다. **메타가 벡터보다
  먼저 온다는 것이 보장된다.**
- `set` 덮어쓰기는 `do_item_replace` → unlink(old) → link(new)이고 둘 다 `cache_lock`
  아래 연속으로 돈다. 콜백이 그 순서대로 불리므로 순 효과가 갱신이다.
- 같은 이유로 `vdel` 직후의 `vadd`, `vadd` 직후의 `vdel`도 도착 순서 그대로 적용된다.

## 9. 실패 처리

콜백은 `cache_lock`을 쥐고 있으므로 **길게 실패하지 않는다.** 그리고 이미 일어난 link를
되돌릴 수 없다.

| 상황 | 처리 |
|---|---|
| 알 수 없는 인덱스의 벡터 link | 버리고 로그. §8이 보장하므로 정상 동작에서는 오지 않는다 |
| 키 파싱 실패 | 버리고 로그. ArcVector의 키가 아니다 |
| 메타 디코드 실패 | 인덱스를 만들지 않는다. 이후 그 인덱스의 벡터는 위 첫 줄로 떨어진다 |
| 그래프 삽입 실패(메모리) | **데몬을 죽이지 않는다.** 로그를 남기고 그 벡터를 건너뛴다 |

마지막 줄에는 갚을 수 없는 빚이 있다. 화해 수단이 없으므로 **건너뛴 벡터는 영구히
그래프에 없다.** 클라이언트 경로(`vadd`)에서는 이를 갚을 수 있다 — 핸들러가 `set` 직후
그래프에 들어갔는지 확인하고, 아니면 방금 쓴 KV를 지우고 `SERVER_ERROR`로 답한다.
복제 적용과 복구 경로에는 갚을 상대가 없어 로그가 전부다(§11).

## 10. 사라지는 코드

| 파일 | 줄 | 왜 |
|---|---|---|
| `src/repl/master.rs` | 563 | 리스너·큐·라이터 스레드 |
| `src/repl/slave.rs` | 1072 | 연결 루프·지연 집합·converge·rebuild |
| `src/repl/wire.rs` | 362 | 프레임 코덱 |
| `src/repl/role.rs` | 988 | 역할 하트비트·주소 공표 |
| `src/attach.rs` | 571 | 파킹 연결. 남는 소비자가 없다 |
| `src/owner.rs` | 278 | 소유권 토큰·`vowner` |
| `src/handler/recovery.rs` | 대부분 | fill·rebuild·drain·claim 프로토콜 |
| `src/handler/arcus/engine/map.rs` | 전부 | Map |
| `src/handler/arcus/engine/elem.rs` | 대부분 | 원소 계열 전체 |
| `src/handler/access/sweep.rs` | 전부 | Map 개수와 그래프 개수의 비교 |

`cfg(parked_cookie)`와 `cfg(recovery)`도 함께 없어진다.

새로 생기는 것은 `src/repl/trigger.rs` 하나 — 콜백 등록, 키 파싱, 이벤트 적용.
**ArcVector에 백그라운드 스레드가 하나도 남지 않는다.**

## 11. 대가와 한계

1. **벡터가 두 벌 상주한다** (§6). 그래프가 주소 대신 복사본을 든다.
2. **복제 적용과 persistence 복구가 HNSW 삽입 시간만큼 `cache_lock`을 쥔다.** 평소 이 락은
   해시테이블 삽입 정도만 쥐는데, HNSW 삽입은 그래프 탐색을 수반해 자릿수가 다르다. 특히
   **복제본이 처음 붙어 풀싱크를 받는 동안** 아이템마다 이것이 걸리므로 그 데몬의 다른 요청이
   전부 뒤에 줄을 선다. 큐를 없앤 대가가 정확히 이것이고, 설계로 피할 수 없다.
3. **삽입 실패는 갚을 수 없다** (§9). 복제·복구 경로에서 메모리 부족으로 건너뛴 벡터는
   영구히 빠진다. 화해 수단이 없다.
4. **`vdrop`이 원소 수만큼 걸린다** (§7.1).
5. **클라이언트가 벡터 아이템을 볼 수 없다.** 키에 공백이 있어 `get`·`scan`·`stats prefix`
   어디에도 잡히지 않는다. 의도한 성질이지만, 운영 중 눈으로 확인할 길도 같이 막힌다.
6. **`ARCUS_ENABLE_SHARD_KEY=1`이 아니면** `{}`가 평범한 문자가 되어 한 인덱스의 키가
   여러 노드로 흩어진다. 배포 전제 조건이다.

## 12. 설정

| 항목 | 값 | 왜 |
|---|---|---|
| `ARCUS_ENABLE_SHARD_KEY` | `1` | `{index}`를 샤드키로 쓴다 (§4.1) |
| `ENABLE_REPLICATION` | 필요 | 훅이 이 매크로 안에 있다 (§3) |

---

관련 문서: [내부구조.md](../../내부구조.md), [복제.md](../../복제.md) (이 설계가 채택되면
다시 써야 한다).
