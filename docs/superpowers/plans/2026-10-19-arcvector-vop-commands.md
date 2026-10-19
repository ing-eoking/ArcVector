# vop 명령어 체계 전환 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ArcVector를 기준 PDF(10/15판)의 `vop` 8개 명령으로 바꾸고, EE `vectest` 서버의 확장 API(비동기 콜백, Map 값, 이벤트 참조)에 맞춘다.

**Architecture:** 엔진 쓰기는 `execute`(워커)에서, HNSW 작업과 검색은 풀에서 한다. 같은 (index, vkey)의 그래프 작업은 해시로 고른 같은 풀 스레드에서 `link_io_wait` 사슬 순서로 돈다. 속성과 인덱스 설정은 BSON, 인덱스 설정은 Map 아이템 값에 둔다.

**Tech Stack:** Rust 2024, usearch 2.26, serde_json(입력 JSON 파싱만), bindgen(바인딩 재생성 시), arcus-memcached-EE `vectest`.

**Spec:** `docs/superpowers/specs/2026-10-19-arcvector-vop-commands-design.md` — 각 태스크는 스펙의 해당 절을 함께 읽는다.

## Global Constraints

- 기준 인터페이스는 `/Users/yeoncheol/Desktop/GS인증/pdf-build/임시/Arcus 서버 - 벡터 명령어.pdf`(10/15판). 다른 판과 달라도 이것을 따른다.
- 엔진 쓰기(`insert_elem`, `delete_elem`, `drop_map`, Map 생성)는 **`execute` 안에서만** 부른다. 풀 스레드와 `on_io_complete` 콜백에서는 엔진 쓰기를 하지 않는다.
- 이벤트 콜백(cache lock 아래)에서는 HNSW 삽입, 검색, 무한 할당을 하지 않는다. 지금 있는 O(1) 노드 제거(`unlink_at`)와 키 이동(`rename_node`)은 콜백에 남긴다(held 집합과 그래프를 한 자리에서 맞추기 위해. 구현 판단).
- 결과 크기에 비례하는 할당은 `crate::room`(fallible)으로 한다. abort 경로를 새로 만들지 않는다.
- 주석은 주변 코드처럼 한국어, 같은 밀도로 쓴다.
- 커밋 메시지는 저장소 관례 한 줄(`INTERNAL: ...`, `FIX: ...`). **`Co-Authored-By` 등 Claude 표기를 넣지 않는다.**
- 매 태스크 끝에 `cargo test --lib`가 통과해야 한다(기준선 197 passed). 빌드는 샌드박스 밖에서 돌려야 할 수 있다.
- 바인딩 재생성: `LIBCLANG_PATH=/opt/homebrew/opt/llvm/lib cargo build --features regen-bindings`.
- 명령 이름·키워드는 대소문자 무시.

## Review Focus

1. 본문 구간 길이가 선언과 다를 때(메타는 맞고 벡터가 짧음 등) — 연결이 끊기지 않고 `CLIENT_ERROR bad data chunk` 한 줄, 아무것도 저장되지 않아야 한다. → Task 7 테스트 `a_short_vector_after_a_good_meta_is_a_bad_data_chunk`.
2. 메타 JSON이 객체가 아님(`[1,2]`, `"x"`, 빈 문자열, 깨진 JSON) — `CLIENT_ERROR`. → Task 1 `only_an_object_becomes_a_document`, Task 8 통합 확인.
3. 필터 값에 따옴표 안 공백·연산자 문자(`title = 'a <= b'`) — 값으로 읽혀야 한다. → Task 2 `quoted_values_keep_spaces_and_operators`.
4. 줄 선 같은 vkey 커넥션 중 하나의 작업이 실패해도 뒤 커넥션은 깨어나야 한다(사슬이 끊기면 영원히 멈춤). → Task 5 `a_failed_job_still_hands_the_key_on`.
5. sync 복제 마스터에서 엔진 쓰기가 `ENGINE_EWOULDBLOCK`을 돌려줌 — 실패가 아니라 성공 + 대기. → Task 5 `ewouldblock_from_a_write_counts_as_written`.

---

### Task 1: BSON 코덱

**Files:**
- Create: `src/bson.rs`
- Modify: `src/lib.rs` (`pub mod bson;`)

**Interfaces:**
- Produces:
  - `pub enum BsonError { NotAnObject, BadJson(String), Malformed, TooDeep }` (+ `Display`, `From<BsonError> for crate::error::Error` → `bad_request`)
  - `pub fn from_json(json: &[u8]) -> Result<Vec<u8>, BsonError>` — 최상위가 객체여야 함. 정수: i32 → 0x10, i64 → 0x12, 그 밖 → 0x01(double). 소수 0x01, 문자열 0x02, 객체 0x03, 배열 0x04(키 "0","1",...), bool 0x08, null 0x0A. 키 순서 유지(serde_json `preserve_order` 피처를 켠다).
  - `pub fn to_json(doc: &[u8]) -> Result<String, BsonError>` — 공백 없는 JSON. double은 정수값이면 `1.0`, 아니면 Rust `{}` 표기.
  - `pub fn doc_len(buf: &[u8]) -> Option<usize>` — 첫 4바이트 LE i32, 5 이상이고 `buf.len()` 이하일 때만.
  - `pub const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];`
  - `pub enum Value<'a> { Num(f64), Str(&'a [u8]), Bool(bool), Null, Other }`
  - `pub fn lookup<'a>(doc: &'a [u8], field: &[u8]) -> Option<Value<'a>>` — 최상위 필드만. 잘못된 문서면 `None`.
  - 중첩 깊이 상한 `MAX_DEPTH = 32`(넘으면 `TooDeep`).

- [ ] **Step 1: 실패하는 테스트 작성** (`src/bson.rs` 하단 `mod tests`)
  - `round_trips_every_scalar_type`: `{"a":1,"b":-5000000000,"c":1.5,"d":"x y","e":true,"f":null}` → `from_json` → `to_json` == 같은 문자열.
  - `nested_objects_and_arrays_round_trip`: `{"o":{"p":[1,"two",{"q":false}]}}` 왕복 동일.
  - `key_order_is_kept`: `{"z":1,"a":2}` 왕복 동일.
  - `only_an_object_becomes_a_document`: `[1,2]`, `"x"`, `1`, ``(빈), `{` 각각 `Err`(앞 넷 중 `[1,2]`/`"x"`/`1`은 `NotAnObject`, 빈 것과 `{`는 `BadJson`).
  - `empty_object_is_the_five_byte_document`: `from_json(b"{}") == EMPTY`, `to_json(&EMPTY) == "{}"`.
  - `lookup_reads_top_level_fields_only`: `{"n":3,"s":"v","o":{"n":9}}`에서 `lookup(n) == Num(3.0)`, `lookup(s) == Str(b"v")`, `lookup(o) == Other`, `lookup(x) == None`.
  - `doc_len_refuses_short_or_lying_headers`: `doc_len(&[4,0,0,0])==None`, 길이 필드가 버퍼보다 크면 `None`, `doc_len(&EMPTY)==Some(5)`.
  - `a_truncated_document_is_malformed`: 유효 문서의 마지막 바이트를 자른 것에 `to_json`이 `Err(Malformed)`.

- [ ] **Step 2: 실패 확인** — `cargo test --lib bson::` → 컴파일 실패(모듈 없음).
- [ ] **Step 3: 구현** — serde_json으로 파싱해 `Value`를 순회하며 인코딩. 디코더는 바운드 검사 후 바이트를 직접 읽는다(패닉 없는 슬라이싱).
- [ ] **Step 4: 통과 확인** — `cargo test --lib bson::` 전부 PASS, `cargo test --lib` 기존 197 유지.
- [ ] **Step 5: 커밋** — `INTERNAL: Add a BSON codec for vector metadata`

---

### Task 2: 필터가 BSON을 평가한다

**Files:**
- Modify: `src/command/filter/mod.rs`
- Delete: `src/command/filter/json.rs`

**Interfaces:**
- Consumes: `crate::bson::{lookup, Value}` (Task 1)
- Produces: `Filter::parse(&str) -> Result<Filter, ParseError>`(변경 없음), `Filter::matches(&self, doc: &[u8]) -> bool` — 인자가 BSON 문서로 바뀜.

- [ ] **Step 1: 테스트 수정/추가** — 기존 필터 테스트의 입력 JSON을 `bson::from_json(...).unwrap()`으로 감싼다. 추가:
  - `and_binds_tighter_than_or`: `a=1 or b=2 and c=3` — `{"a":1}` 참, `{"b":2}` 거짓, `{"b":2,"c":3}` 참.
  - `keywords_are_case_insensitive`: `a=1 AND b=2`, `a=1 and b=2` 동일 결과.
  - `spaces_around_the_operator_are_optional`: `a=1`, `a = 1`, `a =1` 모두 `{"a":1}` 참.
  - `quoted_values_keep_spaces_and_operators`: `title = 'a <= b'` 가 `{"title":"a <= b"}` 참.
  - `a_missing_field_is_false_even_for_ne`: `x != 1` 이 `{"a":1}` 거짓.
  - `int64_and_double_compare_as_numbers`: `n > 4000000000` 이 `{"n":5000000000}` 참, `f <= 1.5` 가 `{"f":1.5}` 참.
- [ ] **Step 2: 실패 확인** — `cargo test --lib command::filter` FAIL(시그니처 불일치/결과 불일치).
- [ ] **Step 3: 구현** — `eval_cond`가 `bson::lookup` 결과(`Value`)로 비교. 없는 필드는 `false`(현재처럼 `!=`도 거짓). 타입 불일치는 현재 규칙(`!=`만 참) 유지. `Value::Other`는 타입 불일치로 취급.
- [ ] **Step 4: 통과 확인** — `cargo test --lib` 전부 PASS. (`search.rs`의 호출부는 Task 6에서 바뀌므로 지금 컴파일이 깨지면 `layout.attr_of` 결과를 임시로 `bson::from_json`에 넘겨 맞춰 둔다.)
- [ ] **Step 5: 커밋** — `INTERNAL: Evaluate search filters over BSON metadata`

---

### Task 3: 요소와 인덱스 설정 코덱

**Files:**
- Modify: `src/handler/arcus/element.rs` (전면 교체)

**Interfaces:**
- Consumes: `crate::bson` (Task 1)
- Produces:
  - `Layout { dim, quant }` 유지. 새 메서드: `element_len(meta_len) = meta_len + vector_bytes()`, `stored_len(meta_len) = element_len + 2`, `fn encode(&self, vector: &[u8], meta: &[u8]) -> Result<Vec<u8>, CodecError>`(meta는 BSON 문서, 빈 경우 `bson::EMPTY`), `fn split<'a>(&self, value: &'a [u8]) -> Result<(&'a [u8] /*meta*/, &'a [u8] /*vector*/), CodecError>`, `fn max_dim_for(quant, max_element_bytes) -> usize`(오버헤드 = 5 + 2).
  - `pub struct IndexConfig { pub dim: usize, pub metric: Metric, pub quant: Quant, pub m: usize, pub efc: usize, pub efs: usize }`
    `impl IndexConfig { pub fn encode(&self) -> Vec<u8> /* BSON + "\r\n" */; pub fn decode(value: &[u8]) -> Result<Self, CodecError> /* 끝의 "\r\n" 허용 */; pub fn layout(&self) -> Layout }`. BSON 키는 스펙 §4.1(`v`=1, `dim`, `metric`, `quant`, `M`, `efc`, `efs`).
  - 제거: `ATTR_BYTES`, `ATTR_OFFSET`, `META_FIELD`, `MetaRecord`, `attr_of`, `vector_offset`, `max_stored_len`. `lib.rs`의 `pub use ATTR_BYTES` 제거.
- [ ] **Step 1: 테스트** — `encode_then_split_returns_both_parts`, `an_empty_meta_is_five_bytes_and_splits_back`, `a_vector_of_the_wrong_length_is_refused`(`VectorLenMismatch`), `a_value_shorter_than_its_meta_header_is_truncated`, `config_round_trips_through_bson`(모든 metric·quant 조합 중 유효한 것), `config_with_an_unknown_metric_is_bad_metadata`, `max_dim_counts_the_empty_document_and_terminator`(16384바이트, i8 → 16377).
- [ ] **Step 2: 실패 확인** — `cargo test --lib element` FAIL.
- [ ] **Step 3: 구현.** 이 파일을 쓰는 곳(`cmd/vector.rs`, `cmd/index.rs`, `cmd/search.rs`, `trigger/event.rs`의 `MetaRecord`/`same_vector`)은 새 API로 최소한만 맞춘다: `attr_of` → `split().0`, `vector_of` → `split().1`, `MetaRecord` → `IndexConfig`(기존 메타 요소 경로는 Task 6에서 지우므로 지금은 요소 값에 `IndexConfig::encode`를 담아도 된다). vadd/vsetattr는 받은 JSON을 `bson::from_json`으로 바꿔 넣는다.
- [ ] **Step 4: 통과 확인** — `cargo test --lib` PASS, `cargo build` 성공.
- [ ] **Step 5: 커밋** — `INTERNAL: Store vector elements as BSON metadata followed by the vector`

---

### Task 4: EE 헤더와 확장 ABI 이전

**Files:**
- Replace: `include/memcached/*.h`, `config.h`, `config_static.h` ← `/Users/yeoncheol/Github/arcus-memcached-EE` (`vectest`, 커밋 `a2839c19` 이후)
- Regenerate: `bindings/engine_api.rs`, `bindings/headers.fingerprint`
- Modify: `src/lib.rs`, `src/server.rs`, `src/handler/offload.rs`, `src/conn_state.rs`, `src/trigger/event.rs`, `src/handler/arcus/engine/map.rs`

**Interfaces:**
- Produces (`src/server.rs`):
  - `pub type IoCallback = unsafe extern "C" fn(*const c_void, *mut c_void, ResponseHandler) -> bool;` (바인딩의 `AIO_CALLBACK`과 같은 모양)
  - `pub unsafe fn on_io_complete(cookie: *const c_void, cb: IoCallback, data: *mut c_void)`
  - `pub unsafe fn link_io_wait(owner: *const c_void, waiter: *const c_void, data: *mut c_void)`
  - `pub unsafe fn next_io_wait(owner: *const c_void) -> Option<(*const c_void, *mut c_void)>`
- Produces (`src/trigger/event.rs`): `pub(crate) struct Refs { pub target: u16, pub source: u16 }`, `fn refs_for(event: Event, decision: ...) -> Refs`(순수 함수, 테스트 대상). 이벤트 값 이름은 `EVENT_ITEM_LINK/UNLINK`, `EVENT_ELEM_INSERT/DELETE/REPLACE`, 필드는 `coll_item`, `target`, `source`, `target_ref`, `source_ref`.
- Produces (`map.rs`): `index_attr(maxcount, exptime)`가 `attr.event = 1`도 세운다.

- [ ] **Step 1: 헤더 교체·재생성** — 위 파일 복사 후 재생성 명령 실행. 생성물에 `on_io_complete`, `link_io_wait`, `next_io_wait`, `target_ref`, `source_ref`, `map_struct_create_with_value`가 있는지 `grep`으로 확인.
- [ ] **Step 2: 테스트 작성** (`event.rs` tests, 기존 `answer`/`Decision` 테스트를 대체)
  - `insert_takes_one_when_held_and_none_when_declined`
  - `delete_returns_now_only_what_never_reached_the_graph`: 그래프에 있던 것 → target 0, waiting에서 지운 것 → 1, 안 잡던 것 → 0
  - `replace_takes_new_and_keeps_old_for_the_sweeper`: old가 그래프에 있었음 → (1, 0); old가 waiting → (1, 1); old 안 잡음 → (1, 0); new를 못 잡음 → target 0
  - `item_events_never_move_references`: ITEM_LINK/UNLINK → (0, 0)
  - `the_five_event_types_are_told_apart`
- [ ] **Step 3: 실패 확인** — `cargo test --lib trigger::event` FAIL.
- [ ] **Step 4: 구현**
  - `lib.rs`: 설명자에서 `block` 제거, `block_vector_cmd`/`wake_vector_cmd` 삭제. `abort`는 유지.
  - `offload.rs`: `submit`이 `waitfor_io_complete` → `on_io_complete(cookie, wake_cb, waiting_ptr)` → 풀 제출 순서. 대기 상태(`Waiting`)를 `Box::into_raw`로 `cb_data`에 넘기고 `wake_cb`에서 되찾아 응답한다(`ConnState::Waiting`과 `is_waiting`은 삭제). 풀이 못 받으면 같은 콜백을 `waitfor` 없이 등록하고 그 자리에서 결과를 채운다. `execute`는 언제나 `true`를 반환한다(오류는 응답으로).
  - `event.rs`: 새 이벤트 이름·필드로 디스패치, `Decision` → `Refs`로 바꿔 `ev.target_ref`/`ev.source_ref`에 쓴다(`ev.ret` 없음). 스펙 §3.2 표대로. 요소 이벤트의 인덱스 이름은 `coll_item`의 키로 찾는다(접두사 처리는 Task 6 전까지 그대로).
  - `map.rs`: `index_attr`에 `event = 1`.
- [ ] **Step 5: 통과 확인** — `cargo test --lib` PASS, `cargo build` 성공, 적재 스모크: `data/`의 서버는 Task 9에서 바꾸므로 여기서는 EE 빌드(`/Users/yeoncheol/Github/arcus-memcached-EE/memcached -E .../.libs/default_engine.so -X target/debug/libarcusv.dylib -p 11299`)로 띄워 `vcreate t 2` → `CREATED`, `vadd`/검색 한 번이 응답하는지 `nc`로 확인.
- [ ] **Step 6: 커밋** — `INTERNAL: Move to the EE extension API for async replies and item events`

---

### Task 5: 풀 스레드별 대기열과 vkey 순서

**Files:**
- Modify: `src/handler/access/pool.rs`, `src/handler/offload.rs`
- Create: `src/handler/order.rs` (소유 표)

**Interfaces:**
- Consumes: `server::{link_io_wait, next_io_wait, notify_io_complete}` (Task 4)
- Produces:
  - `pool::submit_to(lane: usize, job: Job) -> Option<Job>`(가득 차면 돌려줌), `pool::lanes() -> usize`, 기존 `try_submit`은 아무 레인이나(가장 짧은 대기열).
  - `order::lane_of(index: &str, vkey: &str) -> usize` — 안정적인 해시(`std::hash::DefaultHasher`는 프로세스마다 시드가 같으므로 사용 가능) `% pool::lanes()`.
  - `order::begin(index: &str, vkey: &str, cookie: *const c_void, job: *mut c_void) -> Begin` where `enum Begin { Own, Queued }` — 소유하면 `Own`(호출자가 제출), 아니면 `link_io_wait`로 이어 `Queued`.
  - `order::finish(owner: *const c_void, index: &str, vkey: &str) -> Option<(*const c_void, *mut c_void)>` — 다음 대기자와 그 job을 꺼내고 남은 사슬을 그쪽으로 옮기고 소유를 넘긴다. 없으면 표에서 지움.
  - `offload::Work`에 `key: Option<(String, String)>` 추가. 풀 스레드는 키 있는 작업을 끝내면 `order::finish` → `notify_io_complete(owner)` → 다음 작업을 같은 스레드에서 이어서 처리(루프).
  - 엔진 쓰기 결과 `StoreError`에 `WouldBlock` 추가(`ENGINE_EWOULDBLOCK` 번역). 쓰기 호출자는 `WouldBlock`을 성공으로 본다(`Inserted`는 `replaced`/`created`를 엔진이 채운 값 그대로 쓴다).
- [ ] **Step 1: 테스트** (`order.rs` — FFI 대신 `link`/`next`를 주입할 수 있게 `order` 내부를 `trait Chain`으로 추상화하고 테스트는 Vec 기반 가짜로)
  - `the_first_writer_owns_the_key`, `a_second_writer_queues_behind_the_owner`
  - `finish_hands_the_key_to_the_next_in_arrival_order`(3개 대기 → 순서 1,2,3)
  - `the_rest_of_the_chain_moves_with_the_key`(owner가 끝난 뒤 owner의 사슬은 비고 새 owner에 남은 둘)
  - `a_failed_job_still_hands_the_key_on`(작업 결과가 `Err`여도 `finish`가 호출되는 경로를 `Work::run`에서 보장 — `Work::run`의 단위 테스트로 가짜 job이 실패할 때 `finish` 호출 횟수 1)
  - `different_keys_do_not_wait_for_each_other`
  - `ewouldblock_from_a_write_counts_as_written`(`error.rs` translate 테스트: EWOULDBLOCK 코드 → `WouldBlock`, 그리고 `vadd_write` 계열 헬퍼가 그것을 `Ok`로 다루는 순수 함수 `written(code) -> Result<..>` 테스트)
  - `jobs_on_one_lane_run_in_submission_order`(pool 레인 단위 테스트)
- [ ] **Step 2: 실패 확인** — `cargo test --lib order` FAIL.
- [ ] **Step 3: 구현.** 샤드 수 64, 샤드는 `Mutex<HashMap<u64 /*키 해시*/, usize /*owner cookie*/>>`. 해시 충돌은 같은 줄에 서는 것뿐이라 정확성 문제는 없다(순서만 더 엄격). 레인 수 = 기존 `WORKERS`(32), 레인별 대기열 길이 = 기존 `QUEUE`(256).
- [ ] **Step 4: 통과 확인** — `cargo test --lib` PASS.
- [ ] **Step 5: 커밋** — `INTERNAL: Order graph work per vector key on one pool lane`

---

### Task 6: 저장 위치 전환 — 키 = 인덱스 이름, 설정 = Map 값

**Files:**
- Modify: `src/trigger/key.rs`, `src/trigger/event.rs`, `src/trigger/waiting.rs`, `src/trigger/recover.rs`, `src/handler/arcus/engine/map.rs`, `src/handler/access/mod.rs`, `src/handler/cmd/index.rs`, `src/handler/cmd/vector.rs`, `src/handler/cmd/search.rs`

**Interfaces:**
- Consumes: `IndexConfig` (Task 3)
- Produces (`map.rs`):
  - `pub fn create_index_map(&self, key: &str, config: &[u8], attr: item_attr) -> Result<()>` — `map_struct_create_with_value`. 이미 있으면 `StoreError::KeyExists`(새 변형).
  - `pub fn map_value(&self, key: &str) -> Result<Vec<u8>>` — `get`으로 아이템을 잡고 `get_item_info`로 값 복사, `release`. Map이 아니면 `BadType`.
  - `pub fn with_item_value_at<T>(&self, item: u64, f: impl FnOnce(&[u8]) -> T) -> Option<T>` — 콜백용, `get_item_info`만 씀.
- Produces (`access::resolve`): Map이 아니거나 값이 `IndexConfig`로 안 읽히면 `Error::TypeMismatch`(새 변형, 응답 `TYPE_MISMATCH`), 키 없으면 `NoSuchIndex`.
- `key.rs`: `PREFIX`, `META_ID`, `is_meta`, `map_key` 삭제. `MAX_INDEX_NAME = 250`, `valid_index_name` 유지.
- [ ] **Step 1: 테스트** — `key.rs`: `an_index_name_is_its_own_key`, `names_with_spaces_or_over_250_bytes_are_refused`. `event.rs`: 콜백 디스패치에서 메타 요소 분기가 없어졌음을 보이는 순수 함수 테스트 `an_item_link_builds_the_index_from_the_map_value`(값 바이트 → `IndexConfig` → `VectorIndex::building` 생성까지를 `build_from_value(name, value)`로 분리해 테스트). `waiting.rs`: "메타 전 도착" 관련 테스트 삭제.
- [ ] **Step 2: 실패 확인.**
- [ ] **Step 3: 구현** — `vcreate`는 `create_index_map`(event=1, readable=1, maxcount, exptime)으로 Map과 설정을 한 번에 만든다. `ITEM_LINK` 콜백이 `with_item_value_at(target)`으로 설정을 읽어 등록·서빙(이 노드의 create든 복제든 같은 길). `ITEM_UNLINK`는 `registry::remove`. 요소 이벤트에서 인덱스를 모르면(레지스트리에 없음) 참조를 받지 않는다(target 0) — Map이 요소보다 먼저 link되므로 "메타 전 도착"이 없다. `waiting`의 그 용도와 `recover::work`의 BUILDING 승격은 `ITEM_LINK` 경로에 맞춰 정리.
- [ ] **Step 4: 통과 확인** — `cargo test --lib` PASS, `cargo build`, Task 4의 스모크 반복(`vcreate`/`vadd`/검색).
- [ ] **Step 5: 커밋** — `INTERNAL: Keep index settings in the map value under the index name`

---

### Task 7: `vop` 파서와 본문 나누기

**Files:**
- Modify: `src/command/request/{mod.rs,parse.rs,layout.rs}`, `src/command/nread.rs`, `src/command/mod.rs`, `src/command/tokens.rs`

**Interfaces:**
- Produces:
  - `Cmd::Vop` 하나(토큰 0 = `vop`), 하위 명령 `enum Sub { Create, Upsert, SetMeta, Search, GetMeta, Delete, Drop }`(토큰 1).
  - `enum Format { Bin, Str }` (기본 Bin)
  - `struct Segments { meta: Option<usize>, filter: Option<usize>, vector: Option<usize> }` + `fn total(&self) -> usize`(각 구간 + 2) + `fn split<'b>(&self, body: &'b [u8]) -> Result<Parts<'b>>` where `struct Parts<'b> { meta: Option<&'b [u8]>, filter: Option<&'b [u8]>, vector: Option<&'b [u8]> }`. **구간 순서**: upsert = meta, vector / search vector = filter, vector / setmeta = meta / search vkey = filter. 각 구간 뒤 `\r\n`이 아니면 `Error::bad_request("bad data chunk")`.
  - 소유 요청 타입: `Create { index, dim, exptime: u32, maxcount: Option<u32>, metric, quant, m, efc, efs }`(기본 m 16, efc 128, efs 64, maxcount는 핸들러에서 4000), `Upsert { index, vkey, format, has_meta: bool }`, `SetMeta { index, vkey }`, `SearchKey { index, vkey, k, with_meta, has_filter: bool }`, `SearchVector { index, k, format, with_meta, has_filter: bool }`, `GetMeta { index, vkey }`, `Delete { index, vkey }`, `Drop { index }`.
  - `Parsed::Body { len: usize, request: Result<Body> }`의 `len`은 `Segments::total()`. `Body`는 본문 있는 명령(Upsert, SetMeta, SearchKey+filter, SearchVector), `Line`은 나머지.
  - 문법(기준 PDF 그대로): `vop create <index> <dim> <expiretime> [<maxcount>] [metric m] [quant q] [M n] [efc c] [efs s]` — maxcount는 4번째 토큰이 숫자면 maxcount, 키워드면 옵션 시작. `vop upsert <index> <vkey> <vector_bytes> [meta <metabytes>] [bin|str]`. `vop setmeta <index> <vkey> <metabytes>`. `vop search <index> vkey <vkey> <num> [filter <f>] [withmeta]`. `vop search <index> vector <num> <vector_bytes> [filter <f>] [withmeta] [bin|str]`. `vop getmeta|delete <index> <vkey>`. `vop drop <index>`.
  - 기존 `vcreate`/`vadd`/`vsearch`/`vgetattr`/`vsetattr`/`vdel`/`vdrop`/`vlist`/`vstats` 파서와 그 테스트는 삭제.
- [ ] **Step 1: 테스트** (`parse.rs` tests, 기존 것을 대체)
  - `vop_and_its_subcommands_are_case_insensitive`
  - `create_fills_the_pdf_defaults`(`vop create docs 8 0` → m 16, efc 128, efs 64, metric cos, quant f32, maxcount None)
  - `create_reads_a_positional_maxcount_then_options`(`vop create docs 8 60 100 metric l2 quant f16 M 8 efc 64 efs 32`)
  - `create_requires_an_expiretime`(`vop create docs 8` → bad command line format)
  - `create_rejects_b1_with_cos_and_hamming_without_b1`
  - `upsert_body_is_meta_then_vector`: `vop upsert i v 8 meta 7` → len = 7+2+8+2; split(`{"a":1}\r\n` + 8바이트 + `\r\n`) → meta `{"a":1}`, vector 8바이트
  - `upsert_without_meta_is_vector_only`(len = 8+2)
  - `a_short_vector_after_a_good_meta_is_a_bad_data_chunk`
  - `search_vector_reads_num_before_vector_bytes`: `vop search docs vector 10 16 filter 5 withmeta str` → k 10, vector 16, filter 5, with_meta, Str; 본문 순서 filter → vector
  - `search_options_come_in_any_order`(`withmeta filter 5` / `str withmeta` 등)
  - `search_by_vkey_carries_only_a_filter_body`(filter 없으면 `Parsed::Line`)
  - `setmeta_body_is_the_metadata_alone`
  - `getmeta_delete_drop_check_arity`
  - `an_unknown_subcommand_or_option_is_a_client_error`
  - `a_body_over_the_limit_is_refused_but_drained`(len > `MAX_BODY_BYTES`면 accept는 버퍼를 열지 않고 execute가 `body_refused` 메시지)
- [ ] **Step 2: 실패 확인.**
- [ ] **Step 3: 구현.** `MAX_BODY_BYTES = 64 * 1024`(요소 최대 + 필터 최대 4096을 넉넉히 덮는 값, 구현 판단). 필터 문자열 상한 4096바이트 넘으면 `CLIENT_ERROR`. 핸들러 연결(Task 8) 전까지 `handler::run`은 새 요청 타입에 `todo` 대신 `Err(Error::bad_request("not yet"))`를 돌려 컴파일만 유지한다.
- [ ] **Step 4: 통과 확인** — `cargo test --lib` PASS.
- [ ] **Step 5: 커밋** — `INTERNAL: Parse the vop commands and split their bodies in one buffer`

---

### Task 8: `vop` 핸들러와 응답

**Files:**
- Modify: `src/handler/mod.rs`, `src/handler/cmd/{index.rs,vector.rs,search.rs,coords.rs,mod.rs}`, `src/handler/offload.rs`, `src/error.rs`
- Create: `src/handler/cmd/meta.rs` (getmeta/setmeta)

**Interfaces:**
- Consumes: Task 1(BSON), 3(코덱), 5(`order`, 레인), 6(`create_index_map`, `map_value`, `TypeMismatch`), 7(요청 타입, `Segments::split`)
- Produces:
  - `Reply`에 `Replaced`(`REPLACED\r\n`) 추가, `Overflowed` 제거(maxcount 도달은 `Error::Index("index is full")` → `SERVER_ERROR index is full`, 스펙 §9-1). `NotFoundVector` 유지.
  - `cmd::coords::vector(body: &[u8], dim: usize, format: Format) -> Result<Vec<f32>>` — Bin: `body.len() == dim*4` 아니면 `CLIENT_ERROR vector length mismatch (need N bytes, got M)`, LE f32로 읽음. Str: 기존 `coords`.
  - 핸들러: `create(store, &Create)`, `upsert_write(store, &Upsert, Parts) -> Result<UpsertPlan>`(워커), `upsert_insert`(풀), `setmeta(...)`, `search_key(...)`, `search_vector(...)`, `getmeta(...)`, `delete(...)`, `drop(...)`.
  - 검색 응답: `VECTORS <count>\r\n` + 행 `<vkey> <score>\r\n` 또는 `<vkey> <score> <metabytes>\r\n<json>\r\n` + `END\r\n`. `PARTIAL_VECTORS` 헤더는 없앤다(기준 PDF에 없음) — 재구성 중인 인덱스는 지금처럼 보이지 않게(`NoSuchIndex` 아님, 서빙 전에는 `SERVER_ERROR index is rebuilding`) 한다.
  - getmeta 응답: `META <vkey> <metabytes>\r\n<json>\r\n`.
- [ ] **Step 1: 테스트** (핸들러는 엔진이 필요해 단위 테스트가 어려우므로 순수 부분만)
  - `coords.rs`: `bin_vectors_are_little_endian_f32`, `a_bin_vector_of_the_wrong_length_names_both_lengths`, `str_vectors_need_exactly_dim_numbers`
  - `search.rs`: 응답 렌더러를 `fn render(rows: &[(String, f32, Option<String>)]) -> Result<String>`로 분리하고 `renders_vectors_without_meta`, `renders_metabytes_as_the_json_byte_length`(멀티바이트 UTF-8 포함 `{"t":"한"}` → 12)
  - `meta.rs`: `renders_meta_block`
  - `error.rs`: `replaced_and_not_found_vector_are_wire_lines`
- [ ] **Step 2: 실패 확인.**
- [ ] **Step 3: 구현** — 스펙 §5.3 그대로. 흐름은 스펙 §6.2/6.3:
  - create: 검증(`max_element_bytes`, `max_map_size` 초과 시 `CLIENT_ERROR`, maxcount 기본 4000) → `create_index_map` → `CREATED`/`EXISTS`(우리 인덱스)/`TYPE_MISMATCH`(그 밖). 응답은 `on_io_complete`로(sync 복제 대기 대비).
  - upsert: `resolve` → 본문 분해 → 벡터 디코드·양자화 → 메타 BSON → 크기 검사 → `capture(insert_elem(replace=true))` → `order::begin` → `Own`이면 해당 레인에 제출 → 콜백에서 `STORED`/`REPLACED`. 붙잡힌 주소가 없으면(그래프 반영 불필요) 바로 응답.
  - setmeta: 요소 읽기 → 벡터 바이트 유지·메타 교체 → replace 쓰기 → REPLACE 콜백의 `rename_node`가 그래프를 옮김 → `STORED`.
  - search: 통째로 풀(아무 레인). vkey 검색은 `hold_elem`으로 기준 벡터를 읽고(풀에서 엔진 읽기 허용) 없으면 `NOT_FOUND_VECTOR`.
  - delete: `delete_elem` → `DELETED`/`NOT_FOUND_VECTOR`. drop: `drop_map` → `DROPPED`/`NOT_FOUND`. 둘 다 `TYPE_MISMATCH` 판정 포함.
  - 엔진에 쓰는 명령(create, upsert, setmeta, delete, drop)은 풀 작업이 없어도 응답을 `on_io_complete` 콜백에서 쓴다. sync 복제가 `execute` 안의 쓰기에서 기다림을 걸었으면 ack 뒤에 응답이 나가게 하기 위해서다(스펙 §6.2).
  - `vlist`/`vstats`/다중 질의/`QUERY`·`VALUE` 형식 코드 삭제.
- [ ] **Step 4: 통과 확인** — `cargo test --lib` PASS, `cargo build`, EE 서버로 8개 명령 수동 스모크(`nc`).
- [ ] **Step 5: 커밋** — `INTERNAL: Answer the vop commands`

---

### Task 9: 통합 테스트, 테스트 서버, 문서

**Files:**
- Replace: `data/memcached`, `data/default_engine.so` ← EE `vectest` 빌드(`/Users/yeoncheol/Github/arcus-memcached-EE/memcached`, `.libs/default_engine.so`)
- Modify: `tests/common/mod.rs`, `tests/integration.rs`, `README.md`, `docs/내부구조.md` §3(명령 API 표)

**Interfaces:**
- Consumes: 전 태스크.
- [ ] **Step 1: 통합 테스트 재작성** — `tests/common/mod.rs`의 `TERMINATORS`에 `REPLACED\r\n` 추가, `OVERFLOWED` 제거. 테스트(이름과 단언):
  - `create_answers_created_then_exists`, `create_on_a_plain_key_is_type_mismatch`(먼저 `set k 0 0 1`)
  - `upsert_stores_then_replaces`(`STORED` → 같은 vkey `REPLACED`)
  - `upsert_on_a_missing_index_is_not_found`
  - `bin_and_str_vectors_find_the_same_neighbour`
  - `meta_round_trips_through_getmeta_as_json`(`{"a":1,"s":"x y"}` → `META v1 17\r\n{"a":1,"s":"x y"}\r\n`)
  - `setmeta_changes_what_a_filter_matches`
  - `search_by_vkey_and_by_vector_rank_by_distance`, `search_by_a_missing_vkey_is_not_found_vector`
  - `withmeta_appends_metabytes_and_the_json_line`
  - `a_filter_with_or_and_quoted_spaces_selects_the_right_rows`
  - `delete_then_delete_again_is_not_found_vector`, `drop_then_upsert_is_not_found`
  - `a_short_body_is_a_bad_data_chunk_and_the_connection_survives`
  - `maxcount_full_is_a_server_error`(`vop create t 2 0 1` 후 두 번째 다른 vkey upsert)
  - `concurrent_upserts_of_one_vkey_all_answer`(8개 연결이 같은 vkey에 동시 upsert → 모두 `STORED`/`REPLACED`, 마지막 getmeta가 그 중 하나)
  - `many_connections_search_the_same_index_at_once`(기존 유지, 새 문법으로)
- [ ] **Step 2: 실행** — `cargo build && ARCVECTOR_MEMCACHED=$PWD/data/memcached ARCVECTOR_ENGINE=$PWD/data/default_engine.so cargo test --features integration` → 전부 PASS. 실패는 해당 태스크의 코드를 고친다.
- [ ] **Step 3: 문서** — README의 명령 절을 8개 `vop` 명령으로 교체(문법·응답은 기준 PDF 그대로), `vlist`/`vstats`/`QUERY` 서술 삭제, 알려진 한계 갱신. `docs/내부구조.md` §3 표 갱신.
- [ ] **Step 4: 커밋** — `INTERNAL: Test the vop commands against the EE server`
