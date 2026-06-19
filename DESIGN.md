# ArcVector 저장 백엔드 설계 대안

벡터 원본·payload를 **arcus 엔진에 어떻게 저장할지**에 대한 설계 선택지와 추천을 정리한 문서입니다. 동작·명령어·내부 구조 설명은 [README.md](README.md)를 참고하세요.

현재 구현은 **2안(Map)** 입니다. 바이너리 nread 도입으로 *전송* 한도(ASCII 명령줄 26KB)는 사라졌지만, 이제는 *저장* 한도가 병목이 되므로 백엔드 설계를 다시 검토할 가치가 있습니다.

---

## 먼저: 벡터 인덱스는 arcus에 어떻게 저장되는가

대안을 비교하기 전에, **"벡터 인덱스"라는 개념이 arcus 내부 저장과 어떻게 연결되는지**를 먼저 이해해야 합니다.

### 두 개의 층으로 나뉜다

ArcVector는 하나의 벡터 인덱스를 **두 층**으로 다룹니다.

1. **검색 가속기 (메모리 ANN 인덱스)** — 모듈 프로세스 메모리에 있는 FLAT(HashMap) 또는 HNSW 그래프. "질의와 가까운 벡터의 **id**"를 빠르게 찾는 역할.
2. **저장소 (arcus 엔진)** — 벡터 원본 바이트 + payload를 보관. arcus의 메모리 관리·eviction 체계를 그대로 활용.

두 층은 **id로 연결**됩니다. 검색은 1층에서 id를 얻고, 그 id로 2층에서 payload(와 원본)를 꺼냅니다.

```
질의 벡터 ──▶ [1층] 메모리 ANN 인덱스 ──▶ 가까운 id 목록 ──▶ [2층] arcus 저장소 ──▶ payload
              (FLAT/HNSW, 검색용)                            (원본+payload, 메모리 회계)
```

### arcus의 저장 단위 (배경 지식)

arcus는 memcached 기반이라 저장 단위가 정해져 있습니다. "벡터 인덱스"라는 1급 개념은 **arcus에 없으며**, 아래 기본 단위 중 하나에 **얹어야** 합니다 — 그 선택이 곧 1안/2안/3안입니다.

| 저장 단위 | 구조 | 비고 |
|---|---|---|
| **KV 아이템** | `key → value` | value 한도 기본 1MB |
| **컬렉션** | 하나의 `key` 아래 여러 element (List/Set/Map/B+Tree) | element 값 한도 기본 16KB |
| **prefix** | key의 접두부(`:` 구분) | stats 집계·`flush_prefix`·`scan`의 단위 |
| **slab + LRU** | 고정 크기 슬랩 할당 + eviction | 메모리 부족 시 아이템을 밀어냄 |

### 현재(2안) 매핑

현재 구현은 인덱스를 **Map 컬렉션**에 얹습니다.

```
ArcVector 논리 개념              arcus 저장
──────────────────              ─────────────────────────────────────────
인덱스 "docs"            ─▶      Map 컬렉션 1개   (key = "docs")
 ├ 벡터 "vec1"           ─▶       ├ element (field="vec1", value=[float32 N개][payload])
 ├ 벡터 "vec2"           ─▶       ├ element (field="vec2", value=[float32 N개][payload])
 └ ...                            └ ...
인덱스 차원/메트릭        ─▶      (모듈 메모리에만 존재; arcus엔 없음)
검색용 FLAT/HNSW          ─▶      (모듈 메모리에만 존재)
```

- **인덱스 1개 = arcus Map 1개** (key = 인덱스 이름)
- **벡터 1개 = Map element 1개** (field = 벡터 id, value = `float32 바이트 + payload`)
- 벡터 좌표는 검색을 위해 **모듈 메모리(HashMap/HNSW)에도 복제**됨 → 같은 좌표가 최대 3중(Map 바이트 + HashMap + HNSW)
- **payload는 오직 Map에만** 있음 → 검색이 끝나면 id로 Map을 다시 조회해 payload를 가져옴

### vadd / vsearch 한 번에 일어나는 일

- **vadd**: ① arcus Map에 element 저장(메모리 회계) ② 메모리 ANN 인덱스에 좌표 삽입(검색용)
- **vsearch**: ① 메모리 ANN 인덱스에서 가까운 id 목록 ② 각 id로 arcus Map에서 payload 조회 → 응답

여기서 핵심 질문이 나옵니다 — **①의 "arcus 저장"을 Map 말고 다른 단위(KV+prefix, 또는 전용 item type)로 두면 어떨까?** 이것이 바로 아래 1·2·3안입니다.

---

## 배경: arcus 실제 제약

| 제약 | 값 | 근거 |
|---|---|---|
| Map element 값 한도 `max_element_bytes` | 기본 **16KB** / 최대 **32KB** | [engines/default/item_base.h:40-42](../arcus-memcached/engines/default/item_base.h#L40-L42) |
| 일반 KV 아이템 값 한도 | 기본 **1MB** (`-I`로 조정) | memcached `item_size_max` |
| prefix 단위 삭제 / 스캔 | `flush_prefix`, `scan prefix`, `scan key match` | memcached.c |
| prefix별 stats | 지원 | `stats_prefix.c` |

> 이 문서는 **영속성/복제는 범위 밖**으로 둡니다(현 단계 관심사 아님). 모든 저장은 인메모리 기준으로만 비교합니다.

### 실측으로 확인한 사실 (중요)

`max_element_bytes` 검증은 **프로토콜 레벨**([arcus-memcached/memcached.c:4731](../arcus-memcached/memcached.c#L4731) 등 `mop/bop/lop/sop insert`)에만 있고, 엔진 API `map_elem_alloc`에는 **없습니다**. 즉 모듈이 엔진 API를 직접 호출하면 이 한도를 우회할 수 있고, 실제로 다음이 관측되었습니다.

- 한도(16KB)를 한참 넘는 element도 **약 1019KB까지 저장됨** (한도 미강제).
- **정확히 1MB(`item_size_max`) element 저장 시 데몬 크래시**:
  ```
  Assertion failed: (cur_length == slen), do_smmgr_free, slabs.c:1006  → SIGABRT
  ```
  슬랩 small memory manager의 슬롯 길이가 `uint16_t`(8B 단위)이고 정상 슬롯 한계가 `SM_MAX_SLOT_SIZE`(48KB)라, 한도를 넘은 거대 element가 할당자 회계를 깨뜨립니다.

→ **결론**: 어떤 안을 택하든 element/아이템 크기 한도를 **모듈이 선검증해야** 합니다. ArcVector는 `vadd`에서 `get_config("max_element_bytes")`로 확인 후 초과 시 `CLIENT_ERROR ... too large`로 거부하도록 수정했습니다(2안의 저장 한도가 곧 차원 상한임을 그대로 강제).

---

## 1안 — Prefix (개별 KV 아이템 + 키 프리픽스)

각 벡터를 **독립 KV 아이템**으로 저장하고, 같은 인덱스를 키 프리픽스로 묶습니다.
- 키 = `<index>:<id>`, 값 = `[float32 벡터][payload]`
- 인덱스 삭제 = `flush_prefix <index>`, 열거 = `scan prefix`, 메모리 가시성 = prefix stats

**장점**
- 벡터당 값 한도가 **KV 아이템 한도(기본 1MB)** → 32KB element 한도보다 훨씬 큼. 초고차원 수용.
- 인덱스 크기가 `max_map_size`/`maxcount`에 묶이지 않음 → 해시테이블·메모리 한도까지 확장.
- 단일 거대 컬렉션(핫키)이 없음 → 아이템 락이 벡터 단위로 분산, 동시성 유리.
- 벡터 단위 TTL/eviction 독립 적용 가능.

**단점**
- **부분 eviction 위험**: 개별 벡터만 evict되면 메모리 ANN 인덱스에 payload 없는 "유령 항목"이 남음 → 정합성 관리 복잡.
- 인덱스 생성/삭제가 원자적이지 않음(`flush_prefix`·scan은 스캔성). 2안의 "element 한 방"보다 덜 깔끔.
- 키마다 프리픽스 문자열 메모리 오버헤드, delimiter 충돌 주의(인덱스명/id에 구분자 금지).
- "인덱스당 벡터 수" 같은 카운트는 scan/stats로 별도 집계.

## 2안 — Map 컬렉션 (현재 구현)

인덱스 = Map 1개(키=인덱스명), 벡터 = element(field=id, value=`[float32][payload]`).

**장점**
- 인덱스 라이프사이클이 **원자적**: Map 생성/삭제 = 인덱스 생성/삭제 한 방.
- 모든 벡터가 한 키 아래 그룹화 → 열거·관리 단순, `maxcount`로 인덱스 상한 명시.
- element 단위 get/insert/delete API를 엔진이 제공 → 모듈 구현 단순(현 코드).
- eviction이 **인덱스(키) 단위**라 정합성 단순: `KEY_ENOENT` 감지 시 인덱스 통째 무효화 → 메모리 인덱스와 어긋남 없음.

**단점**
- **저장 한도 = `max_element_bytes`(기본 16KB/최대 32KB)**: 벡터+payload가 이 안에 들어가야 함. 즉 차원 상한이 사실상 **~4,096차원(16KB)~8,192차원(32KB, payload 제외)**. ← 현재의 실질 병목.
- 인덱스 크기가 `max_map_size`/`maxcount`에 묶임.
- 인덱스당 단일 Map = 단일 핫키 → 큰 인덱스에서 해당 아이템 락 경합 가능.
- 키 하나 eviction = **인덱스 전체 소실(절벽형)**.

## 3안 — 엔진 네이티브 "벡터" item type (권장 종착지)

arcus 엔진에 List/Set/Map/B+Tree처럼 **"Vector" item type을 1급으로 추가**합니다. 1·2안이 기존 저장 단위(컬렉션/KV)에 벡터를 *얹는* 방식인 반면, 3안은 벡터를 **엔진이 이해하는 1급 타입**으로 만듭니다. Redis(RediSearch)·Qdrant·Milvus·pgvector·FAISS가 벡터를 다루는 방식에서 좋은 점을 모아 설계합니다.

### 핵심 아이디어: 벡터를 "타입화"한다

Redis가 벡터를 Hash의 바이너리 필드로 두고 그 위에 FLAT/HNSW 인덱스를 붙이듯, arcus도 **벡터 인덱스 = 하나의 item, 벡터 = 그 item의 고정형 element**로 모델링합니다. 핵심은 **차원이 고정**이라는 점을 활용하는 것입니다.

### 권장 설계

**(1) item = 하나의 벡터 인덱스 (헤더에 메타)**
- 메타: `dimension`, `metric`(L2/IP/COSINE), `algorithm`(FLAT/HNSW), HNSW 파라미터(M, ef_construction, ef_search), `count`.
- `vcreate`가 이 메타로 item을 생성. 차원·메트릭이 엔진의 1급 속성이 됨.

**(2) element = 고정 크기 레코드 `{ id, float32[dim], payload? }`**
- 차원이 고정 → **element 크기가 고정** → **전용 슬랩 클래스**로 정렬 할당.
- 👉 이 한 가지로 **2안의 두 문제가 구조적으로 사라짐**: ① 가변 길이 `max_element_bytes`(16KB) 한도, ② 거대 element가 가변 길이 small-memory-manager를 깨뜨려 생긴 `do_smmgr_free` 크래시 — 둘 다 "가변 길이"에서 비롯된 문제이므로 고정 크기 슬랩이면 발생하지 않음.
- 벡터는 바이트 패킹/텍스트 없이 raw `float32` → 거리 계산이 SIMD 친화적.

**(3) ANN 인덱스를 엔진이 소유 (중복 제거)**
- HNSW 그래프(또는 FLAT)를 item에 부속된 구조로 엔진이 관리, insert/delete 시 함께 갱신.
- 그래프 노드는 element가 가진 좌표를 **가리키기만** 함 → 지금처럼 모듈 메모리에 좌표를 또 복제(Map 바이트 + HashMap + HNSW = 3중)할 필요가 없어짐. **좌표는 단일 소유.**

**(4) 거리 계산·검색을 엔진 내부에서 (왕복 제거)**
- `vsearch`가 엔진 안에서 그래프 탐색 + 거리 계산 + payload 결합까지 끝내고 한 번에 반환 → 2안의 "id 얻고 다시 Map 조회"하는 왕복이 사라짐.

**(5) 메타데이터 필터 (Redis/Qdrant식 — 강력한 차별점)**
- element에 작은 메타(태그/숫자 범위)를 붙이고, 검색 시 `필터 조건`으로 후보를 제한(pre/post-filtering). Redis `FT.SEARCH ... FILTER`, Qdrant payload filter에 해당. "카테고리=X인 것 중 최근접" 같은 실무 질의를 서버가 처리.

**(6) 메모리 절감 — 양자화 (FAISS/Qdrant식, 후속 옵션)**
- 스칼라/프로덕트 양자화(SQ/PQ)로 `float32 → int8` 등 압축. 대규모에서 메모리 수배 절감. 고정형 레코드라 양자화 적용이 자연스러움.

### 다른 벡터 DB에서 가져온 설계 포인트

| 출처 | 차용한 설계 |
|---|---|
| **Redis (RediSearch)** | 벡터를 1급 타입으로, FLAT/HNSW + L2/IP/COSINE, KNN 질의, **서버측 메타 필터** |
| **Qdrant** | payload 필터링, named vectors, **양자화** |
| **Milvus** | 배치/세그먼트 단위 인덱스 빌드, 양자화 |
| **pgvector** | 타입화된 벡터 값 + 거리 연산자 |
| **FAISS** | IVF/PQ/HNSW 등 알고리즘 다양성, 정수 id 매핑 |

**장점**
- **2안의 16KB element 한도·smmgr 크래시가 구조적으로 소멸** (고정 크기 전용 슬랩).
- **중복 제거**: 좌표 단일 소유, 모듈 메모리 복제 불필요 → 메모리 1/2~1/3.
- 거리 계산이 엔진 내부(SIMD) + **서버측 메타 필터** → Redis/Qdrant급 기능.
- 차원/메트릭/알고리즘이 item 메타로 1급 관리, eviction·락 정책을 벡터에 맞게 설계.
- 양자화로 대규모 메모리 절감 여지.

**단점**
- **엔진 코어 수정** = 최대 공수/리스크: 신규 item type·프로토콜·메모리/락 통합·테스트.
- 업스트림 arcus와의 머지·유지보수 부담 증가, 출시까지 최장.

---

## 비교 매트릭스

| 항목 | 1안 Prefix | 2안 Map (현재) | 3안 네이티브 |
|---|---|---|---|
| 벡터당 값 한도 | KV 1MB | element 16~32KB | 설계하기 나름(무제한급) |
| 인덱스 크기 한도 | 메모리 한도 | maxcount/max_map_size | 설계하기 나름 |
| 인덱스 생성/삭제 원자성 | 약함(scan/flush) | **강함(한 방)** | 강함 |
| eviction 정합성 | 부분 evict→유령 항목 | 절벽형이나 단순 | 설계로 해결 |
| 핫키/동시성 | **분산(좋음)** | 단일 핫키 | 설계로 해결 |
| 서버측 거리계산·필터 | 모듈(왕복) | 모듈(왕복) | **엔진 내부 + 메타 필터** |
| 중복 저장(좌표) | 잔존 | 잔존 | **제거 가능** |
| 크래시 안전성 | 가변 길이 위험 | 가변 길이 위험(가드 필요) | **고정 슬랩으로 구조적 회피** |
| 구현 공수 | 중 | **소(완료)** | **대** |

---

## 추천 — 단계적 접근

1. **지금(단기): 2안(Map) 유지.** 이미 동작하고 리스크 최저. 단 **`max_element_bytes`(32KB) 저장 한도가 실질 상한**임을 명시하고, 실사용 모델(bge-m3 1024d=4KB, OpenAI 1536d=6KB·3072d=12KB)은 16~32KB 안에 들어옴을 확인. 더 큰 차원이 필요하면 `max_element_bytes`를 올리거나 1안으로.
2. **차원·규모가 커지면(중기): 1안(Prefix) 전환 검토.** KV 1MB 한도로 초고차원·대규모 인덱스를 수용하고 핫키를 제거. 대신 *부분 eviction ↔ ANN 정합성* 로직을 추가해야 함.
3. **벡터 검색이 제품 핵심이 되면(장기): 3안(네이티브 타입)이 정답.** 고정형 저장으로 크기 한도·크래시를 구조적으로 없애고, 좌표 중복 제거 + 서버측 거리계산·메타 필터로 Redis/Qdrant급 기능을 확보. 공수가 크므로 로드맵 과제로.

**한 줄 결론**: 현 시점 최선은 **2안 유지(+`max_element_bytes` 인지)**, 전략적 종착지는 **3안**, **1안은 "초고차원·초대규모인데 3안은 아직 부담"인 과도기에만** 선택. 1안은 2안의 단순함과 3안의 완결성 사이의 중간 다리이지 그 자체가 목적지는 아닙니다.
