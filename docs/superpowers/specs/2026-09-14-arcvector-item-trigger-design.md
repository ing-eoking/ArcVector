# 아이템 트리거: 복제를 arcus 자신에게 맡긴다

설계 기록. 상태: **제안**.

[2026-09-04-arcvector-index-replication-design.md](2026-09-04-arcvector-index-replication-design.md)를
**대체한다.** 그 설계가 만든 델타 채널(소켓·큐·라이터 스레드·프레임 코덱·역할 하트비트)은
전부 없어진다.

---

## 1. 무엇이 바뀌었나

arcus-memcached-EE에 `ON_ITEM_TRIGER` 콜백이 생겼다. 키가 `arcus_trig{`로 시작하는
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
`arcus_trig{`로 시작하게 한다.

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
    if (it->nkey > 10 && memcmp(key, "arcus_trig", 10) == 0 && key[10] == '{') {
        engine->server.callback->perform_callbacks(ON_ITEM_TRIGER,
                                                   (const void *)&ret,
                                                   (const void *)it);
    }
#endif
```

`key[10] == '{'`까지 보는 이유는 10바이트 비교만으로는 `arcus_trigger:foo` 같은 평범한
클라이언트 키도 걸리기 때문이다. 우리 키는 이름 다음이 반드시 `{`다.

**훅은 `perform_callbacks` 전에 `it`의 refcount를 올려 확장에 소유권을 넘긴다.** 그래프가
아이템 메모리를 포인터로 들고 있고 검색이 그걸 락 없이 읽기 때문에(§6), 아이템이 확장보다
먼저 free되면 안 된다. `do_item_get`이 하는 그 증가를 여기서 미리 해 주는 것이다 — 이미
`cache_lock` 아래이므로 안전하고, 확장이 락을 잡을 방법도 없다.

확장은 그 참조의 주인이 되고, 반드시 둘 중 하나를 해야 한다: 그래프에 넣어 보관하거나,
해제 목록에 넘기거나(§6). unlink 훅은 refcount를 올리지 않는다 — 그 아이템의 참조는
link 때 이미 받았다.

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
    if (it->nkey > 10 && memcmp(key, "arcus_trig", 10) == 0 && key[10] == '{') {
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

### 3.3 `mc_engine` 대입을 한 줄 올린다 *(적용됨)*

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
arcus_trig{<index>}:<id>      벡터 하나
arcus_trig{<index>}:          인덱스 메타 (id가 빈 문자열)
```

**공백을 쓸 수 없다.** `prefix_link`는 키에 구분자 `:`가 있으면 그 앞을 프리픽스 이름으로
보고 `mc_isvalidname`으로 검사하는데
([prefix.c:373](../../../../arcus-memcached-EE/engines/default/prefix.c)), 허용 문자가
영숫자와 `_ - + . { }`뿐이고 공백이 없다
([util.c:164](../../../../arcus-memcached-EE/util.c)). 공백이 들어가면
`ENGINE_PREFIX_ENAME`이 나고 `do_item_link`는 콜백에 닿기도 전에 되돌아간다
([item_base.c:1080](../../../../arcus-memcached-EE/engines/default/item_base.c)) —
즉 그런 키는 **저장 자체가 안 된다.** (`AV OWNER`가 되던 것은 `:`가 없어 널 프리픽스였기
때문이다.)

`arcus_trig{idx}`는 `_`, `{`, `}`가 모두 허용 문자라 통과한다. 그래서:

- **프리픽스가 인덱스별로 생긴다.** 깊이가 1이므로
  ([prefix.c:34](../../../../arcus-memcached-EE/engines/default/prefix.c)) 첫 `:` 앞
  전체가 프리픽스 이름이 되고, 그게 곧 `arcus_trig{idx}`다. `flush`로 인덱스 하나만
  지울 수 있고(§7.1), `stats prefix`에 인덱스별 아이템 개수가 공짜로 잡힌다.
- **`{}`는 arcus의 샤드키 구분자다**
  ([cluster_config.c:125](../../../../arcus-memcached-EE/cluster_config.c)의
  `get_shard_key`). 인덱스 이름을 그 안에 넣으면 한 인덱스의 모든 키가 같은 노드에 모인다.
  `ARCUS_ENABLE_SHARD_KEY=1`이 필요하다
  ([cluster_config.c:668](../../../../arcus-memcached-EE/cluster_config.c)).
- 프리픽스 이름은 250자까지이므로(`PREFIX_MAX_LENGTH`,
  [types.h:54](../../../../arcus-memcached-EE/include/memcached/types.h)) **인덱스 이름은
  238자 이하**여야 한다. 이름에 `{`, `}`, `:`, 공백을 넣을 수 없다.
- id는 ASCII 키 규칙(공백·제어문자 금지)만 지키면 되고 `:`를 포함해도 된다. 파싱은 첫 `{`와
  그 뒤 첫 `}`로 이름을 떼고, 이어지는 `:` 다음 전부를 id로 본다.
- 프리픽스 이름이 `arcus`가 아니므로 이 아이템들은 `ITEM_INTERNAL`이 **아니다.** 복제본에서
  클라이언트 쓰기는 `ACTION_BEFORE_WRITE` 게이트가 막고, 복제 적용은 그 게이트를 지나지
  않는다 — 둘 다 원하는 동작이다.
- 대신 **클라이언트가 이 키를 ASCII로 지목할 수 있다.** `delete arcus_trig{idx}:v42`를 치면
  unlink가 떠서 그래프에서도 빠진다. 공백으로 가려 두던 보호막이 없어진 것이고, 프리픽스별
  flush와 프리픽스 통계를 얻은 대가다(§11).

### 4.2 값

| 키 | 값 |
|---|---|
| 벡터 KV | 지금의 `Layout` 인코딩 — 헤더 + attr 128바이트 + 양자화된 벡터 |
| 메타 KV | 지금의 `MetaRecord` — dim, quant, metric, maxcount. **소유자 필드는 뺀다**(§2) |

Map, `AV META` 원소, `probe_map`, `hold_all`, `getattr` 개수는 전부 없어진다.

**벡터는 이제 항상 값 안에 들어간다.** 지금은 `cfg(not(recovery))` 빌드가 벡터를 빼고
헤더와 attr만 저장하는데([element.rs:67](../../../src/handler/arcus/element.rs)), 그 벡터
사본이 복제본에게 실려 가는 유일한 통로가 되므로 더는 뺄 수 없다. `Layout::element_len`의
`cfg` 분기가 없어진다 — arcus 쪽 사본의 목적이 "재구축을 위한 것"에서 "복제를 위한 것"으로
바뀔 뿐, 메모리는 `recovery` 빌드와 같다.

### 4.3 크기 한계와 개수

값이 Map 원소가 아니라 아이템 본문이 되므로 **상한이 `max_element_bytes`에서
`max_item_size`로 바뀐다.** `Layout::max_dim_for`가 보는 예산이 달라지고, 같은 quant에서
담을 수 있는 차원 수도 달라진다. `Store::max_element_bytes`/`max_map_size`는 쓰이지 않는다.

인덱스의 원소 개수는 이제 **그래프 자신이 답한다**(`ann.len()`). `vstats`가 Map의
`getattr` 개수와 그래프 개수를 비교하던 항목은 비교 대상이 없어지므로 하나로 합친다.
저장소 쪽 개수가 필요하면 `stats prefix`가 `arcus_trig{idx}` 단위로 답한다(§4.1).

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
     unlink → registry에서 빼고, 그래프를 sweep::retire에 넘긴다
   id가 있으면 벡터 이벤트:
     link   → 값에서 벡터를 읽어 usearch에 넣고, 아이템 포인터를 HeldSet에 등록한다
     unlink → HeldSet에서 빼고 해제 목록에 넘긴다
```

link에서 받은 참조(§3.1)는 **반드시 처분한다.** 그래프에 넣었으면 HeldSet이 주인이고,
넣지 못했으면(알 수 없는 인덱스, 디코드 실패, 메모리 부족) 그 자리에서 해제 목록에 넘긴다.
어느 쪽도 아니면 아이템이 영원히 free되지 않는다.

**그래프를 free하는 일도 콜백 안에서 하지 않는다.** 메타 unlink는 수 GB짜리 HNSW를
놓아주는 일인데, `cache_lock`을 쥔 채 그걸 하면 그동안 데몬의 모든 요청이 멈춘다.
레지스트리에서 빼고 `sweep::retire`에 넘기기만 한다.

큐는 없다. HNSW 삽입이 그 자리에서 끝난다.

## 6. 잠금 순서 — 왜 데드락이 없나

콜백은 `cache_lock`을 쥔 채 **그래프 락**을 잡는다. 그러므로 반대 방향이 하나라도 있으면
AB-BA가 된다.

### 6.1 검색은 반대 방향이 아니다

그래프는 아이템 포인터를 들고, 검색은 그걸 따라가 **락 없이** 엔진 메모리를 읽는다
([search.rs:57](../../../src/handler/cmd/search.rs)의 attr 필터, `id_at`). 락을 안 잡으니
반대 방향이 아니다. 안전한 이유는 §3.1에서 받은 refcount다 — 그 아이템은 우리가 놓아줄
때까지 free되지 않는다.

읽는 것이 attr 128바이트와 id뿐이라는 점이 중요하다. **벡터는 usearch가 이미 자기 메모리에
복사해 갖고 있다** — `AnnIndex`는 커스텀 metric 콜백이 아니라 평범한 usearch 인덱스이고
([index.rs:520](../../../src/handler/usearch/index.rs)의 `typed_add`), `cfg(not(recovery))`
빌드에서는 arcus 원소에 벡터가 아예 없다
([element.rs:67](../../../src/handler/arcus/element.rs)). 검색 중에 엔진 메모리를 따라가는
것은 벡터 때문이 아니다.

### 6.2 반대 방향은 `release` 하나뿐이고, 미룬다

`release`는 `cache_lock`을 잡는다. 그러므로 **그래프 락을 쥔 채로는 절대 부르지 않는다.**

```
unlink 콜백   HeldSet에서 빼고 해제 목록에 push      // 엔진 호출 없음. 순수 메모리
sweeper       release(items...)                    // cache_lock만. 그래프 락은 안 잡는다
```

이 지연 해제는 새로 만드는 것이 아니다. 지금 `HeldSet` → `sweep`이 하는 일이 그것이고,
Map 원소 대신 KV 아이템을 놓아주도록 바꾸면 된다(`map_elem_release` → `release`).

그래서 방향이 이렇게 된다.

```
cache_lock  →  그래프 락     (콜백)
그래프 락   →  (락 없음)     (검색. 포인터를 읽을 뿐)
             cache_lock      (sweeper. 그래프 락을 쥐고 있지 않다)
```

`cache_lock`과 그래프 락을 **동시에** 쥐는 곳이 콜백 하나뿐이므로 인버전이 성립하지 않는다.

### 6.3 우리가 든 참조가 미루는 것

unlink된 아이템은 우리가 release할 때까지 메모리에 남는다. 축출로 회수될 메모리가 sweeper가
한 바퀴 돌 때까지 늦게 풀린다는 뜻이고, 이것이 refcount를 드는 대가다. 해제 목록이 무한히
길어지지 않도록 sweeper의 배치 크기와 주기는 지금 값을 그대로 쓴다.

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

### 7.1 `vdrop` — 메타를 지우고 프리픽스를 flush한다

키가 `arcus_trig{idx}:`로 시작하므로 그 인덱스만의 프리픽스가 존재한다(§4.1). 엔진 vtable의
`flush(handle, cookie, prefix, nprefix, when)`는 프리픽스 버퍼를 그대로 받으므로 ASCII
토큰화를 거치지 않는다.

```
vdrop idx
  delete_kv("arcus_trig{idx}:")        // 메타. unlink 콜백이 그래프를 즉시 놓아준다
  flush("arcus_trig{idx}")             // 벡터 전부
```

메타를 **먼저** 지운다. `flush`는 `oldest_live`를 세우고 LRU를 일부만 즉시 unlink한 뒤
나머지는 접근 시점에 지연 무효화하므로
([items.c:491](../../../../arcus-memcached-EE/engines/default/items.c)), flush만으로는
그래프가 언제 놓여질지 보장되지 않는다. 메타를 먼저 지우면 그래프는 그 자리에서 사라지고,
뒤늦게 뜨는 벡터 unlink 콜백들은 인덱스를 못 찾아 무동작이 된다.

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

**어느 줄이든 link에서 받은 참조를 해제 목록에 넘기고 끝낸다**(§5). 실패 경로에서
그것을 빠뜨리면 아이템이 영원히 free되지 않는다 — 실패가 잦을수록 새는 양이 늘어나므로,
처분을 `Drop`으로 묶어 빠뜨릴 수 없게 만든다.

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
| `src/handler/arcus/engine/elem.rs` | 대부분 | 원소 계열. `hold_addr`/`release_held`는 KV판으로 옮겨간다 |

`cfg(parked_cookie)`·`cfg(recovery)`도 함께 없어진다.

**남는 것과 새로 생기는 것.** 새 파일은 `src/repl/trigger.rs` 하나 — 콜백 등록, 키 파싱,
이벤트 적용. 그리고 **스레드는 sweeper 하나만 남는다.** sweeper는 이 설계에서 할 일이
둘로 늘어난다.

| sweeper가 하는 일 | 왜 배경이어야 하나 |
|---|---|
| 버려진 그래프를 free (`retire`) | 메타 unlink 콜백이 `cache_lock`을 쥐고 있다 (§5) |
| 해제 목록의 아이템을 `release` | `release`가 `cache_lock`을 잡으므로 그래프 락 아래에서 부를 수 없다 (§6.2) |

개수를 비교하던 `access::sweep::probe_round`는 비교 대상이 없어져 사라진다.
`HeldSet`은 그대로 남되 Map 원소 주소가 아니라 KV 아이템 포인터를 담는다.

## 11. 대가와 한계

1. **복제 적용과 persistence 복구가 HNSW 삽입 시간만큼 `cache_lock`을 쥔다.** 평소 이 락은
   해시테이블 삽입 정도만 쥐는데, HNSW 삽입은 그래프 탐색을 수반해 자릿수가 다르다. 특히
   **복제본이 처음 붙어 풀싱크를 받는 동안** 아이템마다 이것이 걸리므로 그 데몬의 다른 요청이
   전부 뒤에 줄을 선다. 큐를 없앤 대가가 정확히 이것이고, 설계로 피할 수 없다.
2. **삽입 실패는 갚을 수 없다** (§9). 복제·복구 경로에서 메모리 부족으로 건너뛴 벡터는
   영구히 빠진다. 화해 수단이 없다.
3. **우리가 든 참조만큼 메모리 회수가 늦다** (§6.3). unlink된 아이템은 sweeper가 놓아줄
   때까지 남는다.
4. **`cfg(not(recovery))`의 벡터 없는 레이아웃을 더는 쓸 수 없다** (§4.2). 그 사본이
   복제본에게 벡터를 실어 나르는 통로가 되기 때문이다. `recovery` 빌드 기준으로는 메모리
   변화가 없다.
5. **클라이언트가 벡터 아이템을 직접 지우거나 덮어쓸 수 있다** (§4.1). 프리픽스 이름에 공백을
   넣을 수 없어 키를 가릴 수단이 없다. `delete`는 그래프에서도 빠지게 하고, 뜻 없는 값을
   `set`하면 디코드 실패로 그 벡터가 조용히 빠진다. arcus에 키 단위 접근 제어가 없으므로
   설계로 막을 수 없고, 운영 규약에 맡긴다.
6. **인덱스 이름이 238자 이하로 제한되고** `{`, `}`, `:`, 공백을 쓸 수 없다 (§4.1).
7. **`ARCUS_ENABLE_SHARD_KEY=1`이 아니면** `{}`가 평범한 문자가 되어 한 인덱스의 키가
   여러 노드로 흩어진다. 배포 전제 조건이다.

## 12. 설정

| 항목 | 값 | 왜 |
|---|---|---|
| `ARCUS_ENABLE_SHARD_KEY` | `1` | `{index}`를 샤드키로 쓴다 (§4.1) |
| `ENABLE_REPLICATION` | 필요 | 훅이 이 매크로 안에 있다 (§3) |

---

관련 문서: [내부구조.md](../../내부구조.md), [복제.md](../../복제.md) (이 설계가 채택되면
다시 써야 한다).
