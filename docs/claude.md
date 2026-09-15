# ArcVector 작업 지침

이 문서는 **지금 진행 중인 아이템 트리거 전환**을 전제로 쓰였다. `복제.md`와
`내부구조.md`는 아직 옛 설계(델타 채널·Map 저장·재구축)를 설명하므로, 충돌하면
**이 문서가 맞다.** 두 문서는 전환이 끝나면 다시 쓴다.

관련: [스펙](superpowers/specs/2026-09-14-arcvector-item-trigger-design.md) ·
[구현 계획](superpowers/plans/2026-09-14-arcvector-item-trigger.md)

---

## 1. 설계 방향

### 1.1 한 문장

**벡터 하나는 arcus KV 아이템 하나이고, 그 아이템이 곧 알림이다.**

arcus가 아이템을 복제본에 복제하거나 persistence 로그에서 되살리면 `do_item_link`가
ArcVector의 콜백을 부른다. 그래서 복제·복구·클라이언트 쓰기 **세 경로가 한 콜백으로
수렴**하고, 이 크레이트에는 자기 소켓도, 큐도, 역할 감지도, 저장소에서 그래프를 다시
짓는 코드도 필요 없다.

### 1.2 키

```
arcus_event{<index>}:<id>      벡터 하나
arcus_event{<index>}:          인덱스 메타 (id가 빈 문자열)
```

- 판별자는 **앞 11바이트 `arcus_event`**다. `do_item_alloc`이 그걸 보고
  `ITEM_WITH_EVENT`(iflag 256)를 세우고, `do_item_replace`가 새 아이템에 물려준다.
- `{}`는 arcus 샤드키 구분자다. 인덱스 이름을 그 안에 넣으면 한 인덱스의 모든 키가
  같은 노드에 모인다. **`ARCUS_ENABLE_SHARD_KEY=1`이 배포 전제.**
- 깊이-1 프리픽스가 `arcus_event{index}`가 되므로 **인덱스별 프리픽스**가 생긴다.
  `vdrop`이 `flush`로 인덱스 하나만 지울 수 있는 근거이고, `stats prefix`가 인덱스별
  아이템 수를 공짜로 준다.
- 인덱스 이름: 1~238바이트(`PREFIX_MAX_LENGTH` 250 − `arcus_event{}` 12).
  `{` `}` `:` 공백 금지 — 공백은 `mc_isnamechar`에 없어서 프리픽스 이름 검사에 걸리면
  **저장 자체가 실패**한다.
- **클라이언트도 이 키를 쓸 수 있다.** 아무 값이나 `set`하면 우리 콜백이 뜬다.
  디코드 실패는 예외가 아니라 **정상 경로**로 처리한다(§3.5).

### 1.3 이벤트 계약

```c
ON_EV_ITEM_LINK   = 5
ON_EV_ITEM_UNLINK = 6

perform_callbacks(ON_EV_ITEM_LINK,   (const void *)it,     NULL);
perform_callbacks(ON_EV_ITEM_LINK,   (const void *)new_it, (const void *)old_it);  // replace
perform_callbacks(ON_EV_ITEM_UNLINK, (const void *)it,     NULL);
```

`perform_callbacks(type, data, cookie)`는 `cb(cookie, type, data, cb_data)`로 부른다.
**아이템은 `event_data`로 온다. `cookie`가 아니다.** replace에서는 `cookie`가 옛
아이템이라 그래프의 주소 교체를 한 번에 할 수 있다.

**link 이벤트는 `ITEM_REFCOUNT_INCR`된 참조의 소유권을 넘긴다.** 확장은 콜백 안에서
스스로 참조를 잡을 방법이 없다 — 참조를 잡는 엔진 호출(`get`, `release`)은 전부
`LOCK_CACHE()`를 지나고 콜백은 이미 그 락을 쥐고 있다. 그래서 훅이 대신 올려준다.
Map 시절 `hold_addr` → `map_elem_get`이 암묵적으로 하던 일을 아이템 단위로 옮긴 것이다.

### 1.4 없어진 것과 그 이유

| 없앤 것 | 왜 필요 없어졌나 |
|---|---|
| 델타 TCP 채널·큐·라이터 스레드 | 아이템 자체가 알림이다 |
| 역할 하트비트 · `AV OWNER` · `vowner` · 파킹 연결 | 아무도 무엇을 보내지 않는다 |
| 지연 집합 · `Resync` · `converge` | 델타가 유실될 경로가 없다 |
| `Snapshot` | 메타 KV의 link가 인덱스 이름을 알려준다 |
| 저장소→그래프 재구축, 소유권 토큰 | 세 경로가 전부 link 이벤트를 지난다 |
| `sweep::probe_round`(개수 폴링) | 아이템이 사라지는 모든 경우가 `do_item_unlink`를 지나 **항상 통지된다** |

---

## 2. 잠금과 수명 — 절대 규칙

여기를 어기면 데드락이나 use-after-free다. 다른 절과 달리 **타협 대상이 아니다.**

### 2.1 잠금 방향은 한쪽뿐

```
cache_lock  →  그래프 락        (콜백. 유일하게 둘을 동시에 쥔다)
그래프 락   →  (엔진 호출 없음)  (검색. refcount 덕에 락 없이 읽는다)
             cache_lock         (release. 그래프 락을 쥐지 않은 채)
```

**`cache_lock`과 그래프 락을 동시에 쥐는 곳은 콜백 하나여야 한다.** 검색이 그래프 락
아래에서 엔진을 부르는 순간 AB-BA가 된다.

### 2.2 콜백 안에서 부를 수 있는 엔진 함수는 `get_item_info` 하나

락을 잡지 않는 순수 필드 읽기라서다. 나머지는 전부 `LOCK_CACHE()`를 지나므로 즉시
데드락이다. **`release()`도 안 된다** (`item_release`가 `LOCK_CACHE()`를 잡는다).

콜백이 하는 일은 순수 메모리 조작뿐이다 — 그래프에서 넣거나 빼고, 해제 목록에 주소를
적는다.

### 2.3 `release()`를 부를 때 쥐고 있으면 안 되는 것

- 그래프 락
- 해제 목록 뮤텍스
- `cache_lock` (당연히)

목록은 **지역 `Vec`으로 비우고 뮤텍스를 놓은 다음** release한다. 임시값 drop 순서에
기대지 말고 블록으로 명시한다.

이유가 둘이다. 콜백이 `cache_lock → 목록 뮤텍스` 순서로 잡으므로 반대로 잡으면
데드락이고, 더 중요하게 **`do_item_release`가 우리 unlink 콜백을 재진입시킬 수 있다:**

```c
if (it->refcount == 0) {
    if ((it->iflag & ITEM_LINKED) == 0) do_item_free(it);
    else if (it->prev == it && it->next == it) {
        if (do_item_isvalid(...)) { /* LRU에 다시 연결 */ }
        else { do_item_unlink(it, ITEM_UNLINK_INVALID); }   // ← 콜백 재진입
    }
}
```

`prev == it && next == it`은 LRU 축출 스캔이 `refcount > 0`이라 건너뛰며
`item_unlink_q`만 해둔 아이템이다. 만료된 그런 아이템을 놓아주는 순간 콜백이 다시 뜬다.

### 2.4 참조는 어느 경로로 나가든 반드시 처분한다

link에서 받은 참조는 **그래프에 넣거나, 못 넣으면 그 자리에서 해제 목록으로** 보낸다.
알 수 없는 인덱스, 디코드 실패, 메모리 부족 — 전부 해당한다. 빠뜨리면 그 아이템은
영원히 free되지 않는다. **처분을 `Drop`으로 묶어 빠뜨릴 수 없게 만든다.**

### 2.5 회수는 마지막 독자가, sweeper는 백스톱

`Searching`이 리더 슬롯을 점유하고(`index.rs`, 128슬롯), `retire`가 유예 시각과 함께
큐에 넣고, 유예가 지난 것만 `release`한다.

- **`reclaim()`을 `Searching::drop`에서 부른다.** 그 시점엔 엔진 락도 그래프 락도 안
  쥐고 있어 안전하고, 회수가 즉시 일어난다.
- **sweeper는 남긴다.** 조회가 없으면 마지막 독자가 영원히 안 나오므로 백스톱이 필요하고,
  버려진 그래프를 free하는 일도 sweeper 몫이다(수 GB짜리 HNSW를 `cache_lock` 아래에서
  해제할 수는 없다).
- **배치 상한을 둔다.** 대량 삭제 직후 마지막 독자가 전부 뒤집어쓰면 그 쿼리만 몇 초가
  된다. 한 번에 N개만 가져가고 나머지는 다음 독자에게 남긴다.
- **`retire`에 즉시-release 빠른 경로를 두지 않는다.** 콜백 스레드에서 그게 타면
  `cache_lock` 데드락이고, 등록 직전 조회자를 놓치는 창도 있다.

### 2.6 refcount가 사주는 것

`do_item_unlink`는 이렇게 끝난다:

```c
if (it->refcount == 0) do_item_free(it);
```

**콜백이 반환하고 몇 줄 뒤에 메모리가 사라진다.** 우리가 그래프에서 뺐다고 해서 이미
순회 결과에 주소를 담은 검색까지 되돌려지지 않는다. 우리 참조 하나가 그 free를 미루고,
마지막 독자가 끝난 뒤 `release`가 진짜 free를 일으킨다. **refcount가 하는 일은 그
유예를 사는 것, 그것 하나뿐이다.**

부작용을 알고 있어야 한다:

- **우리가 든 아이템은 축출되지 않는다.** LRU 스캔이 `refcount > 0`이면 `item_unlink_q`만
  하고 넘어간다. 벡터 메모리를 arcus가 회수하지 못하므로, 압박은 다른 키들이 받는다.
- **`flush`는 refcount를 보지 않고** `do_item_unlink`를 부른다. 그래서 통지는 정상이고
  free만 미뤄진다. `vdrop`이 동작하는 근거다.
- **만료는 지연 판정이다.** 우리가 참조를 든 동안은 아무도 그 키를 안 읽으므로 만료
  아이템이 그래프에 남는다. §2.3의 재진입 경로로 우리가 놓아주는 순간 정리된다.

---

## 3. 명령 경로

### 3.1 모든 쓰기가 같은 모양

**명령은 KV를 쓰거나 지울 뿐이고, 그래프는 트리거가 고친다.** 마스터도 예외가 아니다 —
자기 쓰기의 link도 자기 콜백을 부르므로 명령 핸들러에 그래프를 만지는 코드가 없다.

| 명령 | 하는 일 |
|---|---|
| `vcreate` | 메타 KV를 `add` 시맨틱으로 쓴다 (기존이면 `NOT_STORED` → `EXISTS`) |
| `vadd` | 양자화 → `Layout` 인코딩 → 벡터 KV를 `set` |
| `vdel` | 벡터 KV를 `delete` |
| `vdrop` | 메타 KV를 먼저 지우고, 그 다음 프리픽스를 flush |
| `vsearch` | 그래프만 본다 |

`vadd`가 `STORED`를 돌려줄 때 그래프에 이미 들어가 있다 — 삽입이 `set` 호출 **안에서**
동기로 끝나기 때문이다. `vadd` 직후 `vsearch`가 보이는 성질이 유지된다.

### 3.2 `vdrop`은 메타를 먼저

`flush`는 `oldest_live`를 세우고 LRU를 일부만 즉시 unlink한 뒤 나머지는 접근 시점에
지연 무효화한다. 그래서 flush만으로는 그래프가 언제 놓여질지 보장되지 않는다. 메타를
먼저 지우면 그래프는 그 자리에서 사라지고, 뒤늦게 뜨는 벡터 unlink 콜백들은 인덱스를
못 찾아 무동작이 된다.

### 3.3 순서는 저절로 지켜진다

복제 적용은 cset 시퀀스 순, persistence 복구는 로그 순이다. 마스터에서 `vcreate`가
`vadd`보다 먼저 커밋됐으면 복제본·복구에서도 그 순서로 link된다. **메타가 벡터보다
먼저 온다는 것이 보장된다.**

### 3.4 usearch 동시성

- **`remove`는 소프트 삭제다.** 노드를 그래프에서 빼지도, 벡터 메모리를 놓아주지도 않고
  `key = free_key_`로 표시만 한다. **조회 중 삭제돼도 안전하다.**
- **`add`는 `nodes_count_`를 벡터 포인터보다 먼저 공개한다.** `search`의 진입 가드는 그
  카운터 하나뿐이라 그 창으로 걸어 들어가 널 포인터를 역참조한다. **usearch 자신은
  thread-safe라고 하지만 2.26.0~2.26.2에서 실제로 죽는다** —
  `usearch_add_search_race` 프로브가 raw 인덱스로 재현한다(돌리면 죽는 것이 정상).
- **노출되는 건 진입점뿐이다.** `entry_slot_` 재지정은 두 곳 다 `callback`(벡터 채움)
  뒤에 있으므로, 취약한 건 첫 원소 하나다. `Shards::populated`가 **샤드당 완료된 add
  하나**로 그 창을 영구히 닫는다. 검색은 안 선 샤드를 건너뛴다.
- **노드별 refcount는 usearch에 없고, 붙여도 더 나쁘다.** HNSW 검색은 반환하는 k개보다
  훨씬 많은 노드를 역참조하고 그 집합을 미리 알 수 없다. 에포크 방식이 진입/이탈 각
  원자 연산 1회로 순회 전체를 덮는다.

### 3.5 실패 처리

콜백은 `cache_lock`을 쥐고 있으므로 **길게 실패하지 않는다.** 그리고 이미 일어난 link를
되돌릴 수 없다.

| 상황 | 처리 |
|---|---|
| 알 수 없는 인덱스의 벡터 link | 버리고 로그. §3.3이 보장하므로 정상 동작에서는 안 온다 |
| 키 파싱 실패 · 메타/값 디코드 실패 | 버리고 로그. **클라이언트가 만든 키일 수 있다** |
| 그래프 삽입 실패(메모리) | **데몬을 죽이지 않는다.** 로그 남기고 그 벡터를 건너뛴다 |

어느 줄이든 **받은 참조를 해제 목록에 넘기고 끝낸다**(§2.4).

클라이언트 경로(`vadd`)에서는 삽입 실패를 갚을 수 있다 — `set` 직후 그래프에 들어갔는지
확인하고, 아니면 방금 쓴 KV를 지우고 `SERVER_ERROR`로 답한다. 복제·복구 경로에는 갚을
상대가 없어 로그가 전부다.

---

## 4. 스레드

**ArcVector에 스레드는 `arcvector-sweep` 하나뿐이다.**

`memcached.c`의 순서가 이걸 가능하게 한다:

| 줄 | 무엇 |
|---|---|
| 16493 | `load_extension` — ArcVector 적재, 콜백 등록 |
| **16744** | **`daemonize()` — fork. 이 스레드만 살아남는다** |
| 16793 | `init_engine` → 복구 → **link 콜백 발생** |

- `trigger::install()`은 **콜백 등록만** 한다. 스레드를 안 띄우니 fork 전이어도 무해하다.
- sweeper는 **첫 link 콜백에서 lazily** 뜬다. 그 시점이 fork 이후로 보장되므로
  `pthread_atfork` 복구 장치가 필요 없다. (첫 spawn이 `cache_lock` 아래에서 일어나는
  비용은 프로세스당 한 번이라 받아들인다.)

---

## 5. ABI와 빌드

**vtable은 오프셋으로 호출된다.** 헤더 하나가 멤버를 추가/삭제하면 그 뒤 호출이 전부
밀리고, `abi::verify`는 그걸 못 잡는다 — 필수 슬롯이 null이 아닌지만 보는데, 밀린 자리엔
이웃 함수의 멀쩡한 포인터가 들어앉기 때문이다. 실제로 `types.h`에서
`JHPARK_OLD_SMGET_INTERFACE`가 빠지자 `get_config`가 `item_cachedump`가 되어 첫
`vcreate`에서 데몬이 죽었다.

- **서버의 `config.h`가 단일 출처다.** `build.rs`가 그걸 `-include`로 clang에 넘기므로
  `ENABLE_*`이 파일에서 온다. **크레이트에 ABI 기능 플래그는 없다.**
- 서버 트리를 따라갈 때: `make sync-headers TREE=<path>` (config.h, config_static.h,
  include/memcached 복사 + 재생성). 직접 복사했다면 `cargo build --features regen-bindings`.
- 바인딩은 `bindings/engine_api.rs` **한 개**. 평범한 빌드는 그걸 복사만 하므로 libclang이
  필요 없다.
- `build.rs`가 `include/` 전체의 지문을 `bindings/headers.fingerprint`에 적고, 어긋나면
  **빌드를 거부하며 고치는 명령을 알려준다.** 세그폴트 대신 컴파일 에러가 나게 하는 장치다.

---

## 6. 일하는 방식

이 프로젝트에서 값비싼 실수는 전부 "확인 안 하고 단정한 것"이었다.

- **주석과 문서를 근거로 삼지 않는다.** 원본 소스를 읽는다. `docs/vadd-동시성.md`는
  "검색이 샤드 게이트를 write로 잡는다"고 적혀 있지만 실제 코드는 `populated` 플래그다.
  코드 주석의 줄 번호도 밀려 있었다.
- **논쟁보다 재현.** usearch의 thread-safety 주장이 맞는지 따지는 대신 프로브를 돌려
  SIGSEGV와 크래시 백트레이스를 얻었다. 그게 결론을 냈다.
- **"기존 문제"라고 말하기 전에 베이스라인을 잰다.** 통합 테스트 3건이 실패했을 때,
  작업 전 커밋에 워크트리를 만들어 같은 ABI로 돌려 **같은 3건**임을 확인한 뒤에 그렇게
  말했다.
- **안 돌린 검증은 안 돌렸다고 쓴다.** Docker가 없어 통합 테스트를 못 돌리던 동안에는
  매번 그렇게 적었다. 통과했다고 말하지 않는다.
- **틀렸으면 짧게 고치고 넘어간다.** "usearch add∥search는 전부 위험" → "진입점만",
  "복사가 낫다" → "refcount 유지가 낫다" 둘 다 확인 후 번복했다. 길게 사과하지 않는다.
- **범위를 넓힐 때는 먼저 말한다.** Map→KV는 전송 계층 교체가 아니라 저장 구조 재설계라
  멈추고 확인했다.

---

## 7. 코드 스타일

### 7.1 주석은 "왜"만

무엇을 하는지는 코드가 말한다. 주석은 **그렇게 하지 않으면 무엇이 깨지는지**를 적는다.
이 레포의 기존 주석이 그 기준이고, 특히 잠금 순서·수명·ABI처럼 틀리면 프로세스가 죽는
자리는 근거를 줄 번호까지 남긴다.

```rust
/// `get_item_info` is a pure field read in the default engine -- it takes no
/// cache lock -- which is what makes this callable from inside the callback,
/// where the cache lock is already held and every other engine call would
/// deadlock.
```

주석 언어는 **영어**, `docs/`의 산문은 **한국어**.

### 7.2 데몬 안에서 지킬 것

- **패닉하지 않는다.** 이 코드는 남의 프로세스 안에서 돈다. 에러는 로그와 응답으로.
- **메모리 부족으로 죽지 않는다.** 컬렉션 증가는 `try_reserve`, 실패하면 그 건을 건너뛰고
  로그를 남긴다. `expect`/`unwrap`은 "여기서 실패하면 프로그램 버그"인 자리에만.
- **락을 쥔 채 오래 걸리는 일을 하지 않는다.** 큰 free, HNSW 재구축, I/O는 sweeper로.
- `PoisonError::into_inner`로 뮤텍스 중독을 흡수한다 — 한 요청의 패닉이 인덱스를 영구히
  못 쓰게 만들면 안 된다.

### 7.3 테스트

- **순수 로직은 같은 파일 `#[cfg(test)]`에.** 키 코덱처럼 엔진도 스레드도 안 쓰는 것은
  전부 여기서 덮는다.
- **엔진이 필요하면 `tests/integration.rs`.** `--features integration`으로 게이트하고,
  `ARCVECTOR_MEMCACHED`/`ARCVECTOR_ENGINE`으로 서버를 가리킨다.
- **벤더 버그 재현·성능 측정은 `#[ignore]` 프로브로.** 돌리면 죽는 것이 정상인 테스트는
  그 사실을 주석에 적는다.
- 테스트 이름은 **주장하는 명제**로 짓는다 — `a_key_that_is_not_utf8_is_refused`.
- 기능을 없앨 때 그 기능의 테스트는 **고치지 말고 지운다.**

### 7.4 커밋

```
<type>: <소문자로 시작하는 한 줄 요약>

무엇을 왜 바꿨는지. 특히 "왜"에 지면을 쓴다. 검증한 것이 있으면
숫자로 적는다.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
```

`type`은 `feat` `fix` `refactor` `build` `docs`. 커밋 메시지는 **영어**.

### 7.5 검증 없이 완료라고 하지 않는다

작업을 끝냈다고 말하기 전에 돌린다:

```bash
cargo test --lib
make lint
ARCVECTOR_MEMCACHED=$PWD/data/memcached \
ARCVECTOR_ENGINE=$PWD/data/default_engine.so \
  cargo test --features integration --test integration
```

`cargo test`는 서버가 적재하는 cdylib을 다시 빌드하지 않으므로, 통합 테스트 전에
`cargo build`가 필요하다(하니스가 오래된 `.dylib`을 감지해 거부한다).
