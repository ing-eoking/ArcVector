# ArcVector

**arcus-memcached에 벡터 유사도 검색(Vector Similarity Search) 기능을 추가하는 Rust 확장 모듈**입니다.

기존 arcus-memcached 데몬에 동적 라이브러리(`.so` / `.dylib`)로 로드되어, ASCII 프로토콜에 `vcreate` / `vadd` / `vsearch` 등의 벡터 명령어를 추가합니다. 벡터를 메모리 인덱스(FLAT / HNSW)에서 검색하면서도, 실제 데이터는 arcus의 Map 컬렉션에 저장하여 엔진의 메모리 관리·eviction 체계를 그대로 활용합니다.

이 저장소에는 확장 모듈뿐 아니라, 이 벡터 엔진을 백엔드로 사용하는 **문서 기반 RAG 챗봇**(`rag/`)도 포함되어 있습니다.

> 📐 벡터 저장 백엔드의 설계 대안(1안 Prefix / 2안 Map / 3안 엔진 네이티브 타입)과 추천은 별도 문서 **[DESIGN.md](DESIGN.md)** 를 참고하세요.

---

## 목차

- [아키텍처](#아키텍처)
- [빌드 및 실행](#빌드-및-실행)
- [명령어 레퍼런스](#명령어-레퍼런스)
- [메트릭 / 인덱스 타입](#메트릭--인덱스-타입)
- [내부 구조](#내부-구조)
  - [데이터 구조 / 저장 모델](#1-데이터-구조--저장-모델)
  - [확장 등록 흐름](#2-확장-등록-흐름)
  - [vadd 2-페이즈 (nread) 프로토콜](#3-vadd-2-페이즈-nread-프로토콜)
  - [검색 흐름](#4-검색-흐름)
  - [동시성 모델](#5-동시성-모델)
  - [HNSW 소프트 삭제](#6-hnsw-소프트-삭제)
  - [Eviction 처리](#7-eviction-처리)
- [RAG 챗봇](#rag-챗봇)
- [제한 사항 및 주의점](#제한-사항-및-주의점)

---

## 아키텍처

ArcVector는 세 계층으로 구성됩니다.

```
┌─────────────────────────────────────────────────────────────┐
│  RAG 애플리케이션 (rag/, Python)                              │
│  index.html ── server.py(Flask) ── ingest.py                 │
│                      │                                        │
│                ArcusVector (소켓 클라이언트, arcus_vector.py) │
└──────────────────────┼──────────────────────────────────────┘
                       │  ASCII 프로토콜 (TCP 11211)
                       │  vcreate / vadd / vsearch / ...
┌──────────────────────┼──────────────────────────────────────┐
│  arcus-memcached 데몬 │                                       │
│  ┌────────────────────▼─────────────────────────────────┐   │
│  │  ArcVector 확장 모듈 (src/lib.rs → libarcusv.so)      │   │
│  │                                                      │   │
│  │   메모리 인덱스             arcus Map 컬렉션 (백엔드) │   │
│  │   ┌──────────────┐         ┌───────────────────────┐ │   │
│  │   │ FLAT (HashMap)│ 검색용  │ field = vector_id     │ │   │
│  │   │ HNSW (hnsw_rs)│◄──────► │ value = [floats|payload]│ │ │
│  │   └──────────────┘ 저장/payload└─────────────────────┘ │ │
│  │            ▲  C ABI (bindgen → src/engine_api.rs)     │   │
│  └────────────┼─────────────────────────────────────────┘   │
│         default_engine (Map/List/Set/B+Tree 등 컬렉션 엔진)  │
└──────────────────────────────────────────────────────────────┘
```

| 계층 | 구성 요소 | 역할 |
|---|---|---|
| 확장 모듈 (Rust) | `src/lib.rs` | 벡터 명령어 파싱·실행, 인덱스 관리, HNSW/FLAT 검색 |
| C 바인딩 | `src/c/*.h`, `build.rs` → `src/engine_api.rs` | arcus 엔진 API(`engine_interface_v1`, Map 연산 등)를 Rust FFI로 노출 |
| RAG 앱 (Python) | `rag/` | 임베딩·검색·생성을 묶은 문서 챗봇 |

핵심 설계 원칙은 **"검색은 메모리 인덱스로, 저장은 arcus 컬렉션으로"** 입니다.
- **빠른 검색**: HNSW 그래프 또는 FLAT HashMap을 프로세스 메모리에 두고 검색.
- **데이터 저장**: 벡터 원본(float)과 payload는 arcus Map 컬렉션의 element로 저장하여, arcus의 메모리 회계·eviction·`maxcount` 정책을 그대로 적용.

---

## 빌드 및 실행

### 의존성

| 도구 | 용도 |
|---|---|
| Rust (edition 2024) | 확장 모듈 컴파일 |
| `hnsw_rs` 0.3.4 | HNSW 근사 최근접 이웃 인덱스 |
| `bindgen` 0.72 / `cc` 1.0 | 빌드 시 C 헤더 → Rust 바인딩 생성 |
| arcus-memcached | 모듈을 로드할 호스트 데몬 |

### 빌드

```bash
cd ArcVector
cargo build --release
```

`build.rs`가 빌드 시 수행하는 작업:

1. `src/c/engine.h`를 `bindgen`으로 파싱하여 `src/engine_api.rs`(arcus 엔진 API의 Rust FFI 바인딩)를 생성.
2. (해당 `.c` 파일이 있을 경우) `server_api.c` / `hash.c` / `stats_prefix.c`를 `cc`로 컴파일하여 `server_framework` 정적 라이브러리로 링크.
3. (`libengine.a` / `engine.lib`가 있을 경우) 엔진 정적 라이브러리를 `--whole-archive`로 링크.

빌드 산출물은 `target/release/libarcusv.dylib`(macOS) 또는 `libarcusv.so`(Linux)입니다. crate type은 `cdylib`이며 패키지 이름은 `arcusv`입니다.

### arcus-memcached에 로드

빌드된 라이브러리를 arcus-memcached의 ASCII 프로토콜 확장으로 로드합니다.

```bash
memcached -E default_engine.so \
          -X /경로/libarcusv.so \
          -p 11211
```

데몬이 모듈의 진입점 `memcached_extensions_initialize()`를 호출하면, 모듈이 자신을 `EXTENSION_ASCII_PROTOCOL` 핸들러로 등록하고 이후 `vcreate` 등의 명령어를 처리합니다.

> 저장소 루트의 `memcached`, `default_engine.so`는 로컬 테스트용으로 미리 빌드해 둔 arcus-memcached 산출물입니다.

### 테스트

```bash
cargo test
```

거리 함수(L2/Cosine/IP), HNSW 삽입·검색·삭제, 그리고 멀티스레드 동시 삽입/검색 테스트가 포함되어 있습니다.

---

## 명령어 레퍼런스

모든 명령어는 arcus ASCII 프로토콜로 전송하며, 줄 종료는 `\r\n`입니다.

### `vcreate` — 인덱스 생성

```
vcreate <이름> <차원> <메트릭> [FLAT|HNSW [max_elements]]
```

| 파라미터 | 설명 |
|---|---|
| 이름 | 인덱스(=arcus Map 키) 이름 |
| 차원 | 벡터 차원 수 |
| 메트릭 | `L2` / `COSINE` / `IP` |
| `FLAT`\|`HNSW` | 인덱스 타입 (기본: `FLAT`) |
| max_elements | HNSW 최대 수용 벡터 수 (생략 시 1,000,000) |

```
vcreate myidx 3 L2
vcreate myidx 3 COSINE HNSW
vcreate myidx 3 COSINE HNSW 100000
```

응답: `CREATED` / `CLIENT_ERROR index already exists` 등

### `vadd` — 벡터 추가

벡터는 **little-endian float32 바이너리 blob**으로 명령줄 다음에 전송합니다(nread). 명령줄에는 바이트 길이만 넣어, 고차원 벡터도 ASCII 명령줄 길이 제한(26KB)에 걸리지 않습니다.

```
vadd <인덱스> <id> <vec_bytes> [payload_len]\r\n
<float32 벡터 blob><payload>\r\n
```

| 파라미터 | 설명 |
|---|---|
| vec_bytes | 벡터 blob 바이트 수 (= 차원 × 4, 4의 배수) |
| payload_len | payload 바이트 수 (생략 시 payload 없음) |

데이터 블록 = `[벡터 blob][payload]` 를 이어 붙인 `vec_bytes + payload_len` 바이트(+ 끝 `\r\n`). 모듈이 앞 `vec_bytes`는 벡터로, 나머지는 payload로 분리합니다.

응답: `STORED`

### `vsearch` — 유사 벡터 검색

쿼리 벡터도 동일하게 바이너리 blob으로 전송합니다(nread).

```
vsearch <인덱스> <k> <vec_bytes> [threshold] [ef]\r\n
<float32 쿼리 blob>\r\n
```

| 파라미터 | 설명 |
|---|---|
| k | 반환할 최대 결과 수 |
| vec_bytes | 쿼리 blob 바이트 수 (= 차원 × 4) |
| threshold | 최대 허용 거리 (생략 시 제한 없음). `-` 를 주면 임계값 없이 `ef`만 지정 |
| ef | (선택) HNSW 탐색 폭. 생략 시 `max(k*10, 50)`. ↑recall ↓속도 |

```
vsearch myidx 10 12              # 기본
vsearch myidx 10 12 0.3          # threshold=0.3
vsearch myidx 10 12 - 256        # threshold 없이 ef=256
```

응답 형식 (결과마다 헤더 줄 + payload 줄, payload는 텍스트):
```
<id> <distance> <payload_len>
<payload>
...
END
```

### `vdel` — 벡터 삭제

```
vdel <인덱스> <id>
```
응답: `DELETED` / `NOT_FOUND`

### `vdrop` — 인덱스 삭제

```
vdrop <인덱스>
```
응답: `DROPPED` / `NOT_FOUND`

### `vlist` — 인덱스 목록

```
vlist
```
응답 형식:
```
<이름> <차원> <벡터수> <타입>
...
END
```

---

## 메트릭 / 인덱스 타입

| 메트릭 | 설명 | 거리 계산 |
|---|---|---|
| `L2` | 유클리드 거리(제곱) | `Σ(qᵢ-vᵢ)²` |
| `COSINE` | 코사인 거리 | `1 - cos(q,v)`, 음수 방지 위해 `max(0)` |
| `IP` | 내적 거리 | `-(q·v)` (작을수록 유사) |

| 타입 | 설명 | 검색 방식 |
|---|---|---|
| `FLAT` | 전수 탐색. 소규모·정확도 우선 | 모든 벡터와 거리 계산 후 정렬 |
| `HNSW` | 근사 최근접 이웃. 대용량 | `hnsw_rs` 그래프 탐색 |

`COSINE`의 경우 삽입·검색 시점에 벡터를 L2 정규화한 뒤 처리합니다(`normalize_vector`).

HNSW 파라미터(`src/lib.rs`의 `HnswWrapper::new`):
- 그래프: `max_nb_connection=16`, `max_layer=16`, `ef_construction=200`
- 검색: `ef_search` = `vsearch`의 `ef` 인자(지정 시), 미지정 시 `max(k*10, 50)`

---

## 내부 구조

### 1. 데이터 구조 / 저장 모델

벡터 데이터는 **세 곳**에 동시에 존재합니다.

| 위치 | 자료구조 | 보유 내용 | 용도 |
|---|---|---|---|
| 인덱스 레지스트리 | `INDICES: RwLock<HashMap<String, Arc<VectorIndex>>>` | 인덱스 이름 → 인덱스 | 인덱스 조회 |
| 메모리 벡터맵 | `VectorIndex.vectors: RwLock<HashMap<String, Vec<f32>>>` | id → float 벡터 | FLAT 검색, 차원 검증 |
| HNSW 그래프 | `HnswWrapper` | 그래프 + id 매핑 | HNSW 검색 |
| **arcus Map 컬렉션** | 엔진 내부 | field=id, value=`[float 바이트][payload 바이트]` | **영속 저장 / payload 보관 / eviction 회계** |

`VectorIndex` 구조:

```rust
struct VectorIndex {
    dimension: usize,
    metric: MetricType,
    vectors: RwLock<HashMap<String, Vec<f32>>>,  // FLAT용 + 차원검증
    storage: IndexStorage,                         // Flat | Hnsw(HnswWrapper)
}
```

`HnswWrapper`는 hnsw_rs가 정수 id만 지원하므로, 문자열 id ↔ 정수 id 매핑을 별도 보관합니다.

```rust
struct HnswIdState {
    id_map:  Vec<String>,            // numeric_id → string_id
    str_map: HashMap<String, usize>, // string_id  → numeric_id
    deleted: HashSet<usize>,         // 소프트 삭제된 numeric_id
}
```

> **핵심**: payload는 메모리 인덱스가 아니라 **arcus Map의 value에서** 읽습니다. `value`는 앞쪽 `dimension*4` 바이트가 float 벡터, 그 뒤가 payload입니다. 검색 시 `map_get_payload()`가 이 오프셋 뒤를 잘라 payload를 돌려줍니다.

### 2. 확장 등록 흐름

```
memcached_extensions_initialize(config, get_server_api)
  → get_server_api() 저장 (이후 엔진 핸들 획득에 사용)
  → server.extension.register_extension(
        EXTENSION_ASCII_PROTOCOL, &VECTOR_DESCRIPTOR)
```

`VECTOR_DESCRIPTOR`(`EXTENSION_ASCII_PROTOCOL_DESCRIPTOR`)는 네 개의 콜백을 등록합니다.

| 콜백 | 함수 | 역할 |
|---|---|---|
| `get_name` | `get_name_vector` | `"arcus-vector-db"` 반환 |
| `accept` | `accept_vector_cmd` | 이 명령을 처리할지 판단 + (벡터/payload 받을) nread 버퍼 준비 |
| `execute` | `execute_vector_cmd` | 실제 명령 실행 |
| `abort` | `abort_vector_cmd` | nread 중단 시 버퍼 정리 |

엔진 핸들은 `ensure_engine()`이 `get_server_api()`로부터 1회 획득해 `AtomicPtr<engine_interface_v1>`에 `compare_exchange`로 캐싱합니다(락 없는 지연 초기화).

### 3. 벡터 수신: 바이너리 nread 2-페이즈 프로토콜

`vadd`/`vsearch`는 벡터를 ASCII 명령줄 텍스트가 아니라 **little-endian float32 바이너리 blob**으로 받습니다. arcus의 nread(추가 데이터 읽기) 메커니즘을 써서 2단계로 처리되며, 명령줄에는 메타데이터(바이트 길이 등)만 들어가므로 고차원 벡터도 **ASCII 명령줄 26KB 제한에 걸리지 않습니다**.

```
1. accept_vector_cmd
     vec_bytes(+ payload_len) 파싱 → data_len = vec_bytes + payload_len
     malloc(data_len + 2)
     *ndata = data_len + 2, *ptr = 버퍼  (엔진이 여기로 blob을 읽어들임)
     PENDING[cookie] = PendingState { cmd, buffer, data_len }
        cmd = Vadd{index,id,vec_bytes} 또는 Vsearch{index,k,vec_bytes,threshold}

2. (엔진이 소켓에서 [벡터 blob][payload] 본문을 버퍼로 read)

3. execute_vector_cmd(argc==0) → execute_pending_nread
     PENDING[cookie] 회수 → 버퍼를 data_len 만큼 복사 → free(버퍼)
     앞 vec_bytes = bytes_to_floats() 로 f32 복원, 나머지 = payload
     Vadd:    execute_vadd_with_payload(...)  // Map 저장 + 메모리 인덱스 반영
     Vsearch: execute_vsearch_core(...)       // 검색 실행
```

`PENDING`은 `Mutex<HashMap<cookie_addr, PendingState>>`로, 커넥션(cookie)별 진행 중 상태를 보관합니다. 연결이 중단되면 `abort_vector_cmd`가 버퍼를 해제합니다.

> **분리 지점**: nread는 "길이 N짜리 바이트 덩어리 1개"만 읽으므로, 벡터와 payload를 한 덩어리로 이어 받은 뒤 `vec_bytes` 오프셋에서 모듈이 직접 나눕니다. `vec_bytes`는 클라이언트가 명령줄에 명시하고(= 차원 × 4), 차원 불일치는 blob을 다 읽은 뒤 검증합니다(프로토콜 desync 방지).

### 4. 검색 흐름

```
execute_pending_nread (blob 수신 완료) → execute_vsearch_core
  → 인덱스/차원 검증
  → HNSW: HnswWrapper::search(query, k)
       (deleted 스냅샷 기준으로 fetch_k = k + |deleted| 만큼 가져와 필터)
    FLAT: 모든 벡터와 compute_distance 후 정렬·truncate(k)
  → 각 결과에 대해 map_get_payload()로 arcus Map에서 payload 조회
  → threshold 필터 (distance ≤ threshold)
  → "<id> <distance> <payload_len>\r\n<payload>\r\n" ... "END\r\n"
```

HNSW 검색은 hnsw_rs 내부 panic이 FFI 경계를 넘지 않도록 `catch_unwind`로 감쌉니다.

### 5. 동시성 모델

| 자원 | 락 | 비고 |
|---|---|---|
| 인덱스 레지스트리 | `RwLock` | 조회는 read, 생성/삭제는 write |
| 인덱스별 벡터맵 | `RwLock` | FLAT 검색은 read, 삽입/삭제는 write |
| HNSW id 매핑 | `Mutex` | 삽입/삭제/검색 시 짧게 점유 |
| 엔진 핸들 | `AtomicPtr` | 락 없는 1회 초기화 |
| nread 대기 상태 | `Mutex<HashMap>` | cookie 단위 격리 |

인덱스는 `Arc<VectorIndex>`로 공유되어, 레지스트리 락을 짧게 잡아 `Arc::clone` 후 즉시 해제 → 실제 작업은 인덱스 자체 락으로 수행합니다(레지스트리 전역 락 경합 최소화).

### 6. HNSW 소프트 삭제

hnsw_rs는 그래프에서 노드 물리 삭제를 지원하지 않으므로, `vdel`은 **소프트 삭제**로 처리합니다.

- 삭제 시: `deleted` 집합에 numeric_id 추가 (그래프는 그대로).
- 검색 시: `k + |deleted|` 개를 넉넉히 가져온 뒤 `deleted`에 포함된 결과를 필터링하고 상위 `k`개만 반환.

삭제가 누적되면 검색 시 가져와야 할 후보 수가 늘어나는 트레이드오프가 있습니다.

### 7. Eviction 처리

arcus가 메모리 부족 등으로 인덱스(Map 키)를 **eviction** 하면, 백엔드 저장과 메모리 인덱스의 정합성이 깨집니다. `vadd` 시 `map_elem_insert`가 `KEY_ENOENT`를 반환하면(키가 사라짐) 모듈은 해당 인덱스를 레지스트리에서 제거하고 다음과 같이 응답합니다.

```
CLIENT_ERROR index evicted; use vcreate to rebuild
```

---

## RAG 챗봇

`rag/` 디렉토리는 ArcVector를 벡터 백엔드로 사용하는 한국어 문서 RAG 챗봇입니다. **bge-m3 임베딩**(1024차원) + **EXAONE 3.5 생성 모델**을 Ollama로 구동합니다.

| 파일 | 역할 |
|---|---|
| `arcus_vector.py` | arcus ASCII 프로토콜 소켓 클라이언트 (`vcreate`/`vadd`/`vsearch`/`vlist`) |
| `ingest.py` | 문서를 청크로 나눠 임베딩 후 arcus에 적재 |
| `server.py` | Flask 서버. 질문 임베딩 → `vsearch` → 프롬프트 구성 → 생성 |
| `index.html` | 브라우저 채팅 UI (`localhost:8000`에 연결) |

### 준비

```bash
# Ollama 설치 후
ollama pull bge-m3
ollama pull exaone3.5:2.4b

pip install -r rag/requirements.txt
```

### 문서 인제스트

`rag/docs/` 디렉토리에 `.md` / `.txt` 문서를 넣고 실행합니다. 청크 크기 1000자, 오버랩 100자로 분할하며, 벡터 id는 `경로:청크번호`의 SHA-256 앞 16자입니다.

```bash
cd rag
python ingest.py          # docs/ 전체 자동 인제스트
python ingest.py a.md     # 파일 직접 지정
```

### 서버 실행

```bash
python server.py
```

`/chat`은 질문을 임베딩하여 상위 `TOP_K`개를 검색하고, `SCORE_THRESHOLD` 이하(거리 기준, 낮을수록 유사)인 청크만 컨텍스트로 사용합니다. 인사말은 검색 없이 즉답하며, 관련 문서가 없으면 "문서에서 관련 내용을 찾을 수 없습니다."로 응답합니다.

### 환경변수

| 변수 | 기본값 | 적용 | 설명 |
|---|---|---|---|
| `ARCUS_HOST` | `127.0.0.1` | ingest, server | arcus 호스트 |
| `ARCUS_PORT` | `11211` | ingest, server | arcus 포트 |
| `INDEX_NAME` | `docs` | ingest, server | 인덱스 이름 |
| `OLLAMA_HOST` | `http://localhost:11434` | ingest, server | Ollama 호스트 |
| `DOCS_DIR` | `./docs` | ingest | 문서 디렉토리 |
| `PORT` | `8000` | server | Flask 포트 |
| `TOP_K` | `5` | server | 검색 결과 수 |
| `SCORE_THRESHOLD` | `0.5` | server | 최대 허용 거리(엄격할수록 낮게) |

### API

```
POST /chat   {"message": "질문"}
              → {"answer": "...", "sources": [{id, score, text}, ...]}
GET  /health → {"status": "ok", "index": "<vlist 결과>"}
```

### 채팅 UI

```bash
open rag/index.html
```

---

## 제한 사항 및 주의점

- **재시작 시 메모리 인덱스 소실**: 메모리 인덱스(FLAT/HNSW)는 프로세스 내에 있어 데몬 재시작 시 사라집니다. arcus Map은 in-memory 캐시이므로 영속 스토리지가 아닙니다 — 재기동 후에는 재적재가 필요합니다.
- **HNSW 삭제 누적**: 소프트 삭제이므로 삭제가 많아지면 검색 비용이 증가합니다(메모리도 회수되지 않음).
- **eviction 시 인덱스 무효화**: arcus가 인덱스 Map을 eviction 하면 해당 인덱스는 제거되고 `vcreate`로 재생성해야 합니다. `maxcount`(=`max_elements`)와 메모리 한도를 충분히 잡는 것을 권장합니다.
- **차원 검증**: `vadd`/`vsearch`는 인덱스 차원과 다른 길이의 벡터를 거부합니다(blob 수신 후 검증). NaN/무한값도 거부됩니다.
- **벡터 전송 형식**: 벡터는 little-endian float32 바이너리 blob으로만 받습니다(텍스트 `[1,2,3]` 형식은 지원하지 않음). 명령줄에는 바이트 길이만 들어가므로 ASCII 명령줄 26KB 제한과 무관하게 전송됩니다. 한 커넥션의 nread 할당은 `MAX_VEC_BYTES`/`MAX_PAYLOAD_LEN`(각 16MB)로 제한됩니다.
- **저장 크기 한도(= 차원 상한)**: `벡터(차원×4) + payload`가 엔진의 `max_element_bytes`(기본 16KB, 최대 32KB)를 넘으면 `CLIENT_ERROR ... too large`로 거부됩니다. 즉 실질 차원 상한은 기본 ~4,096 / 최대 ~8,192차원(payload 제외)입니다. 모듈이 `map_elem_alloc` 직접 호출로 이 검증을 우회하면 **거대 element가 슬랩 할당자를 크래시**시키므로(arcus `do_smmgr_free` assertion), `vadd`에서 `get_config("max_element_bytes")`로 선검증합니다.
- **인증/보안**: 모듈 자체는 별도 인증을 추가하지 않으며, 호스트 arcus-memcached의 네트워크·SASL 설정을 따릅니다.
