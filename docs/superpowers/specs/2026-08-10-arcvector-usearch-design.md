# ArcVector v2 — usearch 기반 재설계

- 작성일: 2026-08-10
- 상태: 구현 완료
- 브랜치: `usearch-base`
- 대체 대상: `hnsw_rs` 기반의 이전 `src/lib.rs` (1213줄 단일 파일)

---

## 1. 배경과 목표

현행 ArcVector는 `hnsw_rs`로 메모리 ANN 인덱스를 만들고, 벡터 원본(f32)과 payload를
arcus Map 컬렉션의 element에 저장했다. 두 가지가 실질적 병목이었다.

1. **element 크기 한도** — `max_element_bytes`(기본 16KB / 최대 32KB) 안에 `f32 벡터 + payload`가
   들어가야 하므로 차원 상한이 사실상 4,096 수준이다.
2. **좌표 3중 저장** — Map 바이트 + 모듈 HashMap + HNSW 그래프.

v2의 목표:

- ANN 라이브러리를 **usearch**로 교체한다.
- element 값에 **고정 크기 필터 슬롯 + 양자화 벡터**를 담는다. 양자화로 차원 상한을 끌어올리고,
  고정 슬롯으로 필터를 상수 오프셋에서 읽는다.
- **서버측 메타데이터 필터링**을 지원한다.
- arcus 워커 스레드 모델 위에서 **동시성이 증명 가능하게** 동작한다.

비목표(이번 범위 밖): 영속성 설계 변경, 복제 프로토콜 변경, 엔진 코어 수정.

---

## 2. 저장 모델

인덱스 1개 = arcus Map 1개(key = 인덱스 이름), 벡터 1개 = Map element 1개(field = 벡터 id).
Map을 유지하는 이유는 **키 기반 조회 · 복제 · TTL**이 이미 Map 경로에 붙어 있기 때문이다.

### 2.1 element 값 레이아웃

```
 off  0    2   3     4       6        8       16              144
     +----+---+-----+-------+--------+-------+---------------+---------------+
     |"AV"|ver|quant|dim u16|alen u16|rsvd 8B| ATTR 128B     | 양자화 벡터     |
     +----+---+-----+-------+--------+-------+---------------+---------------+
       헤더 16B (고정)                          고정 128B        dim*sz(quant)
```

| 필드 | 크기 | 설명 |
|---|---|---|
| magic | 2B | `"AV"` — 손상 감지 |
| ver | 1B | 레이아웃 버전 (현재 1) |
| quant | 1B | 0=f32, 1=f16, 2=i8, 3=b1 |
| dim | 2B LE | 차원 (최대 65535) |
| alen | 2B LE | ATTR 영역 내 실제 JSON 길이 |
| rsvd | 8B | 정렬 및 향후 확장 |
| ATTR | 128B | JSON 객체 텍스트, 뒤는 0 패딩 |
| 벡터 | dim×sz | 양자화된 좌표 |

설계 근거:

- **ATTR이 벡터 앞에 온다.** 오프셋이 `dim`·`quant`와 무관한 상수 16이므로 predicate가
  캐시라인 한두 개만 건드린다. 벡터 시작 오프셋도 상수 **144**로 고정된다.
- **ATTR은 조정 불가한 고정 128B다.** 인덱스별 설정(`FBYTES`)을 두지 않는다.
  - 실사용 크기: `{"category":"tech","lang":"ko","ts":1723248000,"score":0.87}` = 62B이므로
    필드를 배로 늘릴 여유가 있다.
  - 슬랩 적합성이 오히려 낫다. 1024차원 i8에서 `16+128+1024 = 1168B`는 1184 슬랩 클래스에
    1.4% 낭비로 들어가는데, 64B였다면 1104B로 **같은 클래스에서 6.8%를 버린다.**
  - 메모리는 100만 벡터당 128MB. 같은 조건의 i8 벡터 자체가 1GB다.
  - 144가 16의 배수라 벡터가 16바이트 정렬되어 SIMD에 유리하다.

### 2.2 양자화

클라이언트는 **항상 f32를 전송**한다. 모듈이 인덱스의 `quant`로 변환해 Map과 usearch
**양쪽에 동일한 바이트**를 넣는다. 저장 정밀도와 검색 정밀도가 항상 일치한다.

| quant | 변환 | usearch |
|---|---|---|
| f32 | 그대로 | `add_f32` / `search_f32` |
| f16 | IEEE half (i16 컨테이너) | `add_f16` / `search_f16` |
| i8 | L2 정규화 후 ×127 클램프 | `add_i8` / `search_i8` |
| b1 | `x > 0` → 1비트 팩킹 | `add_b1x8` / `search_b1x8` |

이 대칭성 덕분에 Map으로부터의 인덱스 재구축이 **무손실**이다. 재양자화가 없다.

`quant`와 `metric`은 독립이 아니다. `b1`은 비트 벡터이므로 `hamming` / `tanimoto`만 허용하고,
`cos` / `l2` / `ip`와 조합되면 `vcreate`가 거부한다. `i8`은 `cos`를 전제로 L2 정규화하므로
`l2`와 조합할 때 거리값이 정규화된 공간 기준임을 문서에 명시한다.

---

## 3. 인덱스에 강제되는 Map 제약

인덱스는 Map 위에 얹히므로 Map의 제약을 그대로 물려받아야 한다. 그런데
**엔진은 이 한도를 강제하지 않는다.** `max_element_bytes` 검증은 프로토콜 레벨
(`mop/bop/lop/sop insert`)에만 있고 엔진 API `map_elem_alloc`에는 없어서, 모듈이 엔진 API를
직접 호출하면 한도를 우회할 수 있다. 실측에서 16KB 한도를 한참 넘는 element가 약 1019KB까지
저장되었고, 정확히 1MB(`item_size_max`) element를 저장하자 슬랩 할당자 회계가 깨지며
데몬이 죽었다.

```
Assertion failed: (cur_length == slen), do_smmgr_free, slabs.c:1006  -> SIGABRT
```

small memory manager의 슬롯 길이가 `uint16_t`(8B 단위)이고 정상 슬롯 한계가
`SM_MAX_SLOT_SIZE`(48KB)이기 때문이다. 따라서 **모듈이 선검증한다.**

| Map 제약 | 인덱스에서의 의미 | 강제 지점 |
|---|---|---|
| `max_element_bytes` | `16 + F + dim×sz(quant) <= 한도` | **`vcreate`** — 차원 상한 검증 |
| `maxcount` / `max_map_size` | 인덱스당 최대 벡터 수 | `vadd` 선검증 |
| `exptime` (TTL) | 만료 시 인덱스도 소멸 | Map attr 그대로 |
| eviction | Map 키가 밀려나면 인덱스 폐기 | `KEY_ENOENT` 감지 |

`max_element_bytes = 16KB` 기준 최대 차원 (고정 오버헤드 144B를 뺀 값):

| quant | 벡터 바이트 | 최대 차원 |
|---|---|---|
| f32 | 4×dim | 4,060 |
| f16 | 2×dim | 8,120 |
| i8 | 1×dim | **16,240** |
| b1 | dim/8 | 129,920 |

`max_element_bytes`는 런타임에 낮출 수 있으므로, 생성 시점에 맞던 인덱스가 나중에 한도를
넘을 수 있다. 따라서 `vcreate`뿐 아니라 **`vadd`에서도 매번 검사한다.**

양자화가 곧 차원 병목의 해법이다.

---

## 4. 정합성 — 단 하나의 규칙

> **usearch 인덱스는 Map으로부터 언제든 재구축 가능한 순수 캐시다. Map에 없으면 없는 것이다.**

이 규칙 하나로 아래가 모두 처리된다.

- **재시작** — 인덱스가 비어 있음 → 첫 `vsearch`에서 Map 스캔 후 지연 빌드
- **eviction / TTL 만료** — Map 키가 사라짐 → `KEY_ENOENT` → 인덱스 폐기
- **복제 슬레이브** — Map 연산이 복제 경로를 타므로 슬레이브에 Map은 복원됨.
  usearch 인덱스는 슬레이브에서도 동일하게 지연 빌드된다.
- **부분 실패** — Map에는 들어갔는데 usearch add가 실패하면, 그 벡터는 검색에 안 보일 뿐
  Map이 진실이므로 규칙 위반이 아니다. 다음 재구축에서 복구된다.

---

## 5. 동시성 설계

arcus는 고정 개수의 libevent 워커 스레드가 명령을 처리한다
(`settings.num_threads`, 기본 4, [memcached.c:394](../../../../arcus-memcached/memcached.c#L394)).
따라서 ArcVector로의 동시 진입 수는 워커 수로 상한이 잡힌다.

**중요한 결과**: `vsearch` 동안 해당 워커의 이벤트 루프 전체가 멈춘다. 같은 워커에 붙은 다른
연결들이 함께 지연되므로, `k`와 `ef_search`에 상한을 두고 predicate 비용을 낮게 유지해야 한다.

### 5.1 usearch 스레드 컨텍스트 — 반드시 지켜야 할 불변식

usearch의 Rust 바인딩은 모든 호출을 `any_thread()`로 넘기고
([rust/lib.cpp:90](https://github.com/unum-cloud/USearch)), 이는 **고정 크기 컨텍스트 풀에서
pop**한다. 풀이 비면 **블록하지 않고 실패한다**:

```cpp
// include/usearch/index_dense.hpp:2140
thread_lock_t thread_lock_(std::size_t thread_id) const {
    if (thread_id != any_thread()) return {*this, thread_id, false};
    std::unique_lock<std::mutex> lock(available_threads_mutex_);
    if (!available_threads_.try_pop(thread_id))
        return {*this, any_thread(), false};   // -> "Reserve capacity ahead of insertions!"
    return {*this, thread_id, true};
}
```

`-t`는 64를 넘어도 경고만 하고 통과하므로([memcached.c:16107](../../../../arcus-memcached/memcached.c#L16107)),
"워커가 64개 이하일 것"이라는 가정에 기댈 수 없다.

**규약**: 인덱스마다 `T`개의 스레드 컨텍스트를 `reserve_capacity_and_threads(cap, T)`로 예약하고,
**permit이 정확히 `T`개인 세마포어**로 usearch 진입을 게이트한다.

```
동시 usearch 진입 수 <= T == 예약된 컨텍스트 수   ==>   any_thread() pop은 실패할 수 없다
```

`T` 기본값 64, `vcreate ... THREADS n`으로 조정. 워커가 `T`보다 많으면 초과분은 세마포어에서
대기한다 — 느려질 뿐 실패하지 않는다.

### 5.2 락 구조

```rust
static REGISTRY: RwLock<HashMap<String, Arc<VectorIndex>>>

struct VectorIndex {
    meta:    IndexMeta,                    // 불변 (dim, quant, metric, F, T)
    build:   Mutex<BuildState>,            // 지연 빌드 직렬화
    ann:     RwLock<usearch::Index>,       // 아래 표 참조
    permits: Semaphore,                    // permit = T
    ids:     boxcar::Vec<IdSlot>,          // key -> id, 락 없는 append/read
    by_id:   RwLock<HashMap<Box<str>, u64>>, // id -> key, 쓰기 경로 전용
}
```

| 연산 | `ann` 락 | 근거 |
|---|---|---|
| `search` / `filtered_search` | **read** | usearch가 "concurrent search" 보장 |
| `add` | **read** | usearch가 스트라이프 스핀락으로 그래프 변경 보호 (index.hpp:661) |
| `remove` | **read** | usearch가 "concurrent updates" 보장 |
| `reserve` / `load` / `reset` | **write** | 노드 배열을 재할당하므로 배타 필요 |

usearch 자체가 "Thread-safe for concurrent construction, search, and updates"를 명시한다
(index.hpp:2215). 우리 `RwLock`은 **재할당(reserve)만** 배타로 만들기 위한 것이다.

`ids`가 **락 없는 append-only 구조**여야 하는 이유: predicate가 방문 노드마다 `key -> id`를
조회하므로, 여기에 `RwLock`을 두면 검색 한 번에 수백 번의 원자적 RMW가 발생하고 `vadd`가
검색 시간 내내 블록된다. `boxcar::Vec`은 append와 인덱스 읽기가 모두 락 없이 동작한다.

### 5.3 락 순서

```
REGISTRY  ->  build  ->  ann  ->  by_id  ->  [엔진 cache_lock]
```

역순 획득은 금지한다. 엔진은 모듈을 콜백하지 않으므로 `cache_lock`은 항상 최내곽이고 항상
해제된다. 따라서 ArcVector 락과 엔진 락 사이에 순환은 구조적으로 발생할 수 없다.

`REGISTRY` 읽기 락은 `Arc<VectorIndex>`를 복제한 뒤 **즉시 해제**한다. `vdrop`이 레지스트리에서
제거해도 진행 중인 검색은 자신의 `Arc`로 인덱스를 살려 두므로 안전하게 완주한다.

### 5.4 연산별 순서

**vadd**
1. 검증 — 차원, `16+F+벡터 <= max_element_bytes`, `maxcount`, JSON이 `F` 안에 들어가는지
2. **Map insert** (진실의 원천을 먼저 갱신)
3. `by_id` write 락 — 기존 id면 기존 key 재사용, 신규면 카운터에서 할당 후 해제
4. permit 획득 → `ann` read 락 → `add`. 용량이 부족하면 read 락을 놓고 `ann` write 락을 잡아
   `reserve`한다(2배 성장, 최소 1024) → read 락으로 재시도.
   write 락 획득 자체가 모든 reader의 배수를 보장하므로, 그 시점에 usearch 내부에 진입해 있는
   스레드는 없다. permit을 따로 회수할 필요는 없다.
5. `ids`에 append (락 없음)

2단계 성공 후 4단계가 실패해도 Map이 진실이므로 정합성 규칙(4장)을 만족한다.

인덱스는 `multi: false`로 만들므로 **usearch는 같은 key로의 두 번째 `add`를 거부한다.**
따라서 기존 id의 갱신은 `remove` 후 `add`이다. 그 사이의 짧은 창에서 동시 검색이 해당 벡터를
놓칠 수 있으나, Map이 진실의 원천이므로 데이터가 사라지지는 않는다.

**vdel** — Map delete → permit + `ann` read 락으로 `remove` → `ids` 슬롯 tombstone.
`remove` 실패 시 유령 키가 남지만, predicate가 `KEY_ENOENT`로 걸러내고 지연 삭제한다.

**vsearch**
```
permit 획득
  ann.read()
    filtered_search(query, k, predicate)
      predicate(key):
        id = ids[key]                              // 락 없음
        store.get_filter_slot(index, id, &mut buf) // 엔진 락 1회, 64B
        filter.eval(&buf)                          // 무할당 스캐너
  상위 k의 id로 Map 재조회 -> JSON + 거리 응답
permit 반납
```

**지연 빌드** — `build` 뮤텍스로 직렬화하고 이중 검사한다. 두 워커가 동시에 "인덱스 없음"을
발견해도 재구축은 한 번만 일어난다. 재구축은 `map_elem_get(numfields=0)`으로 전체를 한 번에
읽는다(큰 malloc 1회지만 드문 경로).

### 5.5 필터 읽기 경로

`map_elem_get`은 필드가 1개여도 `elem_array`를 malloc하고 refcount를 올린다
([coll_map.c:934](../../../../arcus-memcached/engines/default/coll_map.c#L934)).
컬렉션 API 전체에 단일 요소를 직접 반환하는 경로는 없고, 내부 `do_map_elem_find`는 `static`이라
링크도 불가하다.

따라서 베이스는 `map_elem_get`을 그대로 쓰되, `store.rs`에 경계를 둔다.

```rust
fn get_filter_slot(&self, index: &str, id: &str, out: &mut [u8]) -> Result<(), StoreError>;
```

방문 노드당 비용: 전역 `cache_lock` 1회 + malloc 1회 + refcount 1회 + 64B 읽기(무복사 슬라이스).
나중에 엔진에 **신규** API(`map_elem_get_value` — 락 안에서 offset/len만 memcpy)를 추가하면
이 함수의 내부 구현만 바뀌고 호출부는 그대로다. 기존 API는 건드리지 않는다.

---

## 6. 모듈 구조

```
src/
  lib.rs      extension 등록, ASCII 토큰 파싱, nread 상태머신
  codec.rs    레이아웃 encode/decode              <- 순수, 단위테스트
  quant.rs    f32 -> f16/i8/b1 변환               <- 순수, 단위테스트
  filter.rs   필터식 파서 + 무할당 평가             <- 순수, 단위테스트
  index.rs    usearch 래퍼, 세마포어, id 매핑
  store.rs    Map 엔진 API 래퍼 (get_filter_slot 포함)
  cmd/        vcreate vadd vget vsearch vdel vdrop vlist
```

`src/c/*.h`와 bindgen 빌드 설정은 arcus ABI 정의이므로 재사용한다. 로직은 전부 새로 쓴다.

의존성: `usearch 2.26`, `serde_json`(vadd 시 JSON 검증), `boxcar`(락 없는 append-only 벡터).
`hnsw_rs` 제거.

---

## 7. 명령어

```
vcreate <index> <dim> [METRIC cos|l2|ip|hamming|tanimoto] [QUANT f32|f16|i8|b1]
                      [THREADS n] [M n] [EFC n] [EFS n] [MAXCOUNT n] [EXPTIME n]
vadd    <index> <id> <veclen> [ATTR <attrlen> <attr JSON>]\r\n<f32 LE 벡터>\r\n
vsearch <index> <k> <veclen> [filterlen]\r\n<f32 LE 질의벡터><필터식>\r\n
vget    <index> <id>
vdel    <index> <id>
vdrop   <index>
vlist
```

**ATTR은 명령줄에, 검색 필터식은 본문에 실린다.** 둘을 다르게 두는 이유가 있다.

ATTR은 128B로 짧고 상한이 정해져 있어 명령줄에 실어도 안전하다. 다만 JSON에 공백이 들어가면
토크나이저가 쪼개므로 원문을 복원해야 한다. [`tokenize_command`](../../../../arcus-memcached/mc_util.c)는
토큰을 끝내는 공백만 `'\0'`으로 덮고 연속 공백은 건드리지 않으며, 토큰들은 같은 버퍼의 연속된
구간이다. 따라서 첫 토큰부터 마지막 토큰 끝까지 읽어 `'\0'`을 `' '`로 되돌리면 원문이 그대로
복원된다 — JSON에는 생(raw) NUL이 올 수 없으므로 이 대응은 모호하지 않다. memcached 자신도
명령줄을 로깅할 때 같은 방식을 쓴다(memcached.c:8078-8086).

검색 필터식은 길이 상한이 없어 이 방법을 쓸 수 없다. 토큰 수가 `MAX_TOKENS`(30)에 닿으면
토크나이저가 남은 줄을 쪼개지 않고 통째로 남기는데, 그 길이는 확장이 받은 토큰 배열만으로는
알 수 없다. 그래서 필터식은 본문으로 보낸다.

`ATTR`은 생략 가능하고(속성 없이 저장), 다음을 검사한다.

| 검사 | 시점 |
|---|---|
| `attrlen > 128` | 선언값만으로 즉시 거부 (본문을 읽기 전) |
| `attrlen` ≠ 실제 전달 바이트 수 | 복원 후 비교 |
| ATTR이 JSON **객체**인지 | `vadd` — 필드로 질의하므로 배열·스칼라는 거부 |
| `144 + 벡터 바이트 > max_element_bytes` | `vadd`마다 |

필터 문법(베이스): `field op value`를 `AND` / `OR`로 결합. `op` = `=` `!=` `<` `<=` `>` `>=`.
`AND`가 `OR`보다 강하게 결합하며 괄호는 없다. JSON 최상위 필드만 지원하고 중첩 경로는 후속
과제다. **없는 필드에 대한 조건은 `!=`를 포함해 항상 거짓**이다 — 그렇지 않으면 `a != x`가
`a`가 없다는 이유만으로 통과해 버린다.

---

## 8. 테스트 전략

- **순수 단위테스트** (엔진 불필요): `codec` 왕복, `quant` 변환 정확도, `filter` 파서·평가
- **동시성 테스트**: `T`보다 많은 스레드로 `vsearch`를 때려 세마포어 불변식이
  usearch 컨텍스트 고갈을 막는지 확인. 이 테스트가 없으면 5.1의 함정이 프로덕션에서 드러난다.
- **통합 테스트**: 실제 arcus 데몬에 붙여 `max_element_bytes` 초과 거부, TTL 만료 후 재구축,
  eviction 후 `KEY_ENOENT` 처리

`store`와 `index`는 얇은 래퍼로 유지해 FFI 표면을 최소화한다.

---

## 9. 남은 위험

| 위험 | 완화 |
|---|---|
| predicate당 전역 `cache_lock` — 동시성 하에서 처리량 저하 | `get_filter_slot` 경계 유지, 벤치 후 신규 엔진 API로 교체 |
| `vsearch`가 워커 이벤트 루프를 점유 | `k`·`ef_search` 상한, predicate 비용 최소화 |
| 대형 인덱스의 지연 빌드가 첫 검색을 지연 | 빌드 진행 중 `SERVER_ERROR busy` 반환 검토 |
| i8 양자화 정확도 손실 | `vcreate`에서 quant 선택 가능, 기본은 f32 |
