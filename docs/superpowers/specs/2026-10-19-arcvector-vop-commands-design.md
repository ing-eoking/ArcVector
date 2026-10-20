# vop 명령어 체계로의 전환

설계 기록. 상태: **승인**.

기준 인터페이스는 **`Arcus 서버 - 벡터 명령어.pdf` 10월 15일 판**
(`Desktop/GS인증/pdf-build/임시/`)이다. 다른 판(10월 16일 판 PDF, `샘플/*.md`)과 다른 곳이
있어도 **이 판을 그대로 따른다.**

동시에 붙는 서버를 커스텀 트리에서 **arcus-memcached-EE(`vectest` 브랜치)** 로 옮긴다.

---

## 1. 왜

- 명령어 인터페이스가 확정됐다. 지금의 `vcreate`/`vadd`/`vsearch`/`vgetattr`/...는 이름,
  인자 순서, 응답 형식이 모두 다르다.
- 속성(JSON)은 255바이트 고정 상한, 공백 없는 단일 토큰이라는 제약이 있었다. 확정
  인터페이스는 속성을 길이를 앞세운 별도 줄로 받으므로 이 제약이 사라진다.
- EE가 확장용 비동기 처리(`waitfor_io_complete` + `on_io_complete` + `link_io_wait`),
  Map 아이템 값 저장(`map_struct_create_with_value`), Map 단위 이벤트 플래그
  (`item_attr.event`, `target_ref`/`source_ref`)를 제공한다. ArcVector가 기대던 커스텀
  트리의 `block` 훅, `arcus_event:` 키 접두사, `EVENT_LINK_ITEM` 계열 이벤트는 EE에 없다.

## 2. 목표 / 비목표 / 반목표

**목표**
- 기준 PDF의 8개 명령(`vop create|upsert|setmeta|search(vkey)|search(vector)|getmeta|delete|drop`)을
  문법, 응답, 오류 메시지까지 그대로 구현한다.
- 속성은 모두 **BSON** 으로 저장한다(벡터 속성, 인덱스 설정 모두).
- 여러 줄 본문(속성 + 벡터, 필터 + 벡터)은 **한 덩어리 버퍼로 한 번에 받고 확장이 나눈다.**
- HNSW 그래프를 건드리는 일(삽입, 삭제, 키 이동, 검색)은 전부 스레드 풀에서 한다.
- 같은 (index, vkey)의 그래프 작업은 해시로 고른 **같은 풀 스레드**에서, 커넥션을
  `link_io_wait`로 이어 **도착 순서대로** 처리한다.
- EE 헤더로 바인딩을 다시 만들고 EE 서버에 적재돼 돈다.

**비목표**
- `vlist`, `vstats`는 없앤다. 대체 명령도 만들지 않는다.
- 여러 질의 벡터를 한 번에 보내는 기존 다중 질의 검색은 없앤다.
- 필터의 괄호, 중첩 필드 경로는 지원하지 않는다.
- EE 코어(`conn_io_callback`)는 고치지 않는다(§6.3이 그것 없이 되게 짠다).

**반목표 (기술적으로 돌아가도 실패인 것)**
- 엔진 쓰기를 풀 스레드에서 하는 것. EE 복제는 sync 대기용 `last_cset_seq`를
  `get_thread_index(cookie)`, 즉 그 커넥션의 워커 슬롯에 기록한다. 풀 스레드가 쓰면 같은
  워커의 다른 커넥션 쓰기와 그 슬롯을 덮어써 sync 복제의 ack 대기가 틀어진다.
  **엔진 쓰기는 반드시 `execute` 안에서, 그 커넥션의 워커 스레드에서 한다.**
- 엔진의 cache lock 아래(이벤트 콜백)에서 HNSW 작업이나 할당을 하는 것.
- 클라이언트가 고칠 수 없는 이유로 데몬이 abort하는 것(기존 `room` 원칙 유지).

## 3. 서버 쪽 전제

### 3.1 EE 헤더로 이전

`include/memcached/*.h`, `config.h`, `config_static.h`를 EE `vectest` 트리에서 가져오고
`cargo build --features regen-bindings`로 `bindings/engine_api.rs`를 다시 만든다.

| 기존(커스텀 트리) | EE |
|---|---|
| 설명자의 `block` 훅 + `conn_waking` | `waitfor_io_complete` + `on_io_complete(cookie, cb, data)` → `conn_io_callback` |
| `EVENT_LINK_ITEM` ... `EVENT_REPLACE_ELEM`, `ret`/`new_it`/`old_it`/`coll_it` | `EVENT_ITEM_LINK/UNLINK`, `EVENT_ELEM_INSERT/DELETE/REPLACE`, `coll_item`/`target`/`source`/`target_ref`/`source_ref` |
| 키 접두사 `arcus_event:`로 이벤트 대상 결정 | `item_attr.event = 1`로 만든 Map만 이벤트 발생(복제 시 플래그와 Map 값도 복제됨) |
| 메타 요소(`" meta"` 필드) | Map 아이템 값(`map_struct_create_with_value`) |

`data/`의 `memcached`, `default_engine.so`는 EE `vectest`로 다시 빌드한 것으로 바꾼다.

### 3.2 참조 규칙 (`target_ref` / `source_ref`)

엔진은 콜백이 돌아온 뒤 INSERT·LINK에서는 `target += target_ref`, DELETE·UNLINK에서는
`target -= target_ref`, REPLACE에서는 `target += target_ref`, `source -= source_ref`를 한다.

ArcVector의 그래프 키는 요소 주소이고, 검색 중 필터·vkey·withmeta가 그 주소의 요소를
제자리에서 읽는다(벡터 좌표는 usearch가 따로 복사해 가진다). 그래서 그래프에 있던 요소는
진행 중인 검색이 끝난 뒤(배리어) sweeper가 `map_elem_release`로 놓는다.

| 이벤트 | 할 일 | 답 |
|---|---|---|
| `ITEM_LINK` | 값의 BSON 설정을 읽어 인덱스 등록(빌드는 풀) | `target_ref = 0` |
| `ITEM_UNLINK` | 인덱스를 레지스트리에서 내림 | `target_ref = 0` |
| `ELEM_INSERT` | 주소를 `waiting`에 적음. 이 스레드의 명령이 쓴 것이면 그 명령에게 돌려줌(`capture`) | 붙잡으면 1, 못 붙잡으면 0 |
| `ELEM_DELETE` | 그래프에 있던 것: `held`에서 빼고 그래프 삭제는 풀, 놓는 것은 sweeper. 아직 그래프에 안 들어간 것(`waiting`): 장부에서 지우고 지금 놓음 | 그래프: 0 / waiting: 1 / 안 잡던 것: 0 |
| `ELEM_REPLACE` | new는 붙잡음. old는 DELETE와 같은 규칙 | `target_ref` 1(못 잡으면 0), `source_ref`는 DELETE 규칙 |

`ELEM_REPLACE`로 new를 붙잡을 수 없으면(할당 실패) new는 그래프 밖에 남는다(stored but not
indexed). 기존 정책대로 로그를 남긴다.

## 4. 저장 형식

### 4.1 인덱스

- **키 = 인덱스 이름 그대로.** 인터페이스의 `TYPE_MISMATCH`("대상 키가 존재하지만 인덱스
  타입이 아님")가 인덱스와 일반 키가 한 키 공간을 쓴다는 뜻이기 때문이다.
  최대 길이는 엔진 키 상한(250)이다.
- Map 하나, `item_attr.event = 1`, `readable = 1`, `maxcount`, `exptime`으로
  `map_struct_create_with_value`로 만든다.
- Map 아이템 값 = 인덱스 설정 BSON 문서 + `\r\n`.
  `{ "v": 1, "dim": i32, "metric": str, "quant": str, "M": i32, "efc": i32, "efs": i32 }`
- 우리 인덱스인지는 "이벤트 플래그가 선 Map이고, 값이 위 BSON으로 읽힌다"로 판단한다.
  아니면 `TYPE_MISMATCH`.
- 복제본에는 `vop create`가 오지 않는다. `EVENT_ITEM_LINK`에서 `get_item_info`로 값을 읽어
  설정을 얻고 인덱스를 세운다. 설정이 Map 자체에 있으므로 요소가 설정보다 먼저 오는
  일("메타 전 도착")이 없어진다.

### 4.2 벡터 요소

- 필드 = vkey (최대 250바이트).
- 값 = `[BSON 속성 문서][양자화된 벡터]`. 속성이 없으면 빈 문서(5바이트). BSON은 첫 4바이트가
  문서 길이이므로 벡터 시작 위치를 따로 적지 않는다.
- 상한은 요소 전체가 `max_element_bytes`에 들어가는가 하나다.
  - `vop create`의 차원 검사: `5 + 벡터 바이트 + 2 <= max_element_bytes`.
  - `vop upsert`/`setmeta`의 크기 검사: `BSON 길이 + 벡터 바이트 + 2 <= max_element_bytes`,
    넘으면 `CLIENT_ERROR`.

### 4.3 JSON ↔ BSON

- 입력 JSON은 **객체**여야 한다. 아니면 `CLIENT_ERROR`.
- 정수는 i32에 들면 int32, 아니면 int64(그 밖이면 double). 소수는 double. 문자열, bool,
  null, 중첩 객체, 배열을 그대로 대응한다.
- `vop getmeta`와 `withmeta`는 BSON을 **공백 없는 JSON**으로 내보낸다. 키 순서는 유지,
  숫자 표기는 정규화된다.
- 변환·인코딩·디코딩은 `bson` 크레이트가 한다. 필드 값의 MongoDB 확장 JSON 표기
  (`{"$numberLong":"5"}` 등)는 그 타입으로 저장하고, 최상위 키는 언제나 필드 이름이다.
  (2026-10-20 변경: 처음에는 직접 작성하기로 했다.)

## 5. 명령

명령 이름과 키워드는 대소문자를 구분하지 않는다.

### 5.1 본문 받기 — 한 덩어리

`accept`가 줄을 파싱해 본문 전체 크기를 계산하고 버퍼 하나를 연다. 각 구간은 자기
`\r\n`까지 포함한다. **구간 순서는 기준 PDF 그대로다.**

| 명령 | 본문 | `ndata` |
|---|---|---|
| `upsert` (meta 없음) | `<vector_data>\r\n` | `vector_bytes + 2` |
| `upsert meta <m>` | `<metadata>\r\n<vector_data>\r\n` | `m + 2 + vector_bytes + 2` |
| `setmeta` | `<metadata>\r\n` | `m + 2` |
| `search vkey` + filter | `<term...>\r\n` | `f + 2` |
| `search vector` | `[<term...>\r\n]<vector_data>\r\n` | `[f + 2] + vector_bytes + 2` |

`execute`가 선언된 길이대로 자르고 각 구간 끝이 `\r\n`인지 확인한다. 아니면
`CLIENT_ERROR bad data chunk`. 전체 크기 상한(`MAX_BODY_BYTES`)을 넘으면 본문은 버리고
`CLIENT_ERROR`.

### 5.2 벡터 데이터 형식

- `bin`(기본): **항상 little-endian f32**, `vector_bytes == dim * 4`여야 한다. 양자화는 서버가 한다.
- `str`: 공백으로 구분된 10진수 텍스트, 개수가 `dim`이어야 한다.

### 5.3 명령별 동작

**`vop create <index> <dim> <expiretime> [<maxcount>] [metric m] [quant q] [M n] [efc c] [efs s]`**
- 기본값: maxcount **4000**, metric cos, quant f32, M 16, efc 128, efs 64.
- `b1`은 `hamming`/`tanimoto`만, 그 둘은 `b1`만.
- maxcount가 서버 `max_map_size`를 넘으면 `CLIENT_ERROR`.
- `<expiretime>`은 memcached exptime 규칙(0 = 만료 없음, 작은 값 = 초, 큰 값 = 유닉스 시각).
- 응답: `CREATED` · `EXISTS`(같은 이름의 인덱스) · `TYPE_MISMATCH`(키는 있는데 인덱스 아님).

**`vop upsert <index> <vkey> <vector_bytes> [meta <metabytes>] [bin|str]`**
- 인덱스가 없으면 `NOT_FOUND`. 자동 생성하지 않는다.
- 엔진에 replace 허용 insert(`map_elem_insert` replace=true)로 한 번에 쓴다. 새 vkey면
  `STORED`, 있던 vkey면 `REPLACED`.
- 그래프 반영이 끝난 뒤에 답한다(답을 받은 클라이언트의 다음 검색은 그 벡터를 본다).
- maxcount에 닿으면 기준 PDF에 해당 응답이 없으므로 `SERVER_ERROR index is full`
  (구현 판단, 사용자 확인 필요).

**`vop setmeta <index> <vkey> <metabytes>`** + `<metadata>`
- 요소를 읽어 벡터 바이트는 그대로, 속성만 새 BSON으로 바꿔 replace로 쓴다.
- 그래프는 다시 삽입하지 않는다. 풀에서 그래프 키를 old 주소에서 new 주소로 옮긴다
  (usearch `rename`). 복제본도 REPLACE 이벤트를 보므로 같은 길로 옮긴다 — 벡터 바이트가
  같으면 이동, 다르면 재삽입.
- 응답: `STORED` · `NOT_FOUND` · `NOT_FOUND_VECTOR` · `TYPE_MISMATCH`.

**`vop search <index> vkey <vkey> <num> [filter <filter_bytes>] [withmeta]`** (+ 필터 줄)
**`vop search <index> vector <num> <vector_bytes> [filter <f>] [withmeta] [bin|str]`** (+ 필터 줄 + 벡터)
- `filter`, `withmeta`, `bin|str`은 순서를 가리지 않는다.
- 필터: `<field>[ ]<op>[ ]<value>`, op는 `= != < <= > >=`, 공백이 든 값은 `'...'`, 여러 조건은
  `and`/`or`로 연결한다. 기준 PDF에 우선순위가 적혀 있지 않으므로 **일반 관례대로 `and`가
  `or`보다 먼저 묶인다**(기존 `Filter` 파서의 동작). 평가 대상은 BSON 최상위 필드이고, 없는
  필드에 대한 조건은 거짓.
- vkey 검색에서 기준 vkey가 없으면 `NOT_FOUND_VECTOR`.
- 응답:
  ```
  VECTORS <count>\r\n
  <vkey> <score>\r\n                       (withmeta 없음)
  <vkey> <score> <metabytes>\r\n<json>\r\n (withmeta)
  ...
  END\r\n
  ```
  score는 metric별 거리(cos: 1−cos, l2: 제곱 유클리드, ip: 1−내적, hamming, tanimoto: 1−유사도).
  `metabytes`는 내보내는 JSON의 바이트 수다.

**`vop getmeta <index> <vkey>`** → `META <vkey> <metabytes>\r\n<json>\r\n`
**`vop delete <index> <vkey>`** → `DELETED` · `NOT_FOUND` · `NOT_FOUND_VECTOR` · `TYPE_MISMATCH`
**`vop drop <index>`** → `DROPPED` · `NOT_FOUND` · `TYPE_MISMATCH`

`vop delete`는 엔진 삭제가 끝나면 답한다. 그래프에서의 제거는 풀이 나중에 한다 — 검색은
결과를 낼 때 `held`에 없는 주소를 이미 걸러내므로, 답 직후의 검색도 지운 벡터를 내지 않는다.

### 5.4 오류 응답

- `NOT_FOUND`: 인덱스 없음 · `NOT_FOUND_VECTOR`: vkey 없음 · `TYPE_MISMATCH`: 키는 있는데 인덱스 아님.
- `CLIENT_ERROR <reason>` / `SERVER_ERROR <reason>`: 기존 `Error::blame` 구분 그대로.

## 6. 처리 경로와 스레드

### 6.1 누가 무엇을 하나

| 일 | 스레드 |
|---|---|
| 파싱, 본문 자르기, JSON→BSON, 벡터 디코드·양자화 | 워커(`execute`) |
| 엔진 쓰기(insert/replace, delete, Map 생성·삭제), 엔진 단건 읽기 | **워커(`execute`)** |
| HNSW 삽입, 삭제, 키 이동, 검색(필터 평가·결과 조립 포함) | **풀** |
| 이벤트 콜백(cache lock 아래) | 엔진이 부른 스레드. O(1) 장부 기록과 풀 제출만 |
| 응답 작성 | 워커(`on_io_complete` 콜백, 또는 기다릴 것이 없으면 `execute`) |

### 6.2 비동기 처리 — `block` 훅 대신

```
execute (워커)
  엔진 쓰기까지 끝낸다
  waitfor_io_complete(c)              ← 풀 제출보다 먼저
  on_io_complete(c, done_cb, task)
  풀에 제출 → true 반환               ← 코어가 conn_io_callback으로 보내고 재움
풀
  일을 하고 task에 결과를 적음 → notify_io_complete(c)
done_cb (워커, conn_io_callback)
  response_handler로 응답. 엔진 쓰기는 하지 않는다
```

- 콜백 안에서는 엔진 쓰기도 다시 기다리기도 하지 않는다. EE `conn_io_callback`은 그것을
  지원하지 않는다.
- sync 복제 마스터에서 `execute`의 엔진 쓰기가 `ENGINE_EWOULDBLOCK`을 돌려주면(복제 ack
  대기, 엔진이 이미 `waitfor_io_complete`를 불렀다) 쓰기는 성공한 것으로 본다.
  `MULTI_NOTIFY_IO_COMPLETE`가 기다림을 세므로, 복제 ack와 풀 작업이 **둘 다** 끝나야
  `done_cb`가 돈다. 기다릴 풀 작업이 없는 명령(delete, drop, create)도 응답은 `done_cb`에서
  쓴다 — 그래야 ack 전에 응답이 나가지 않는다.
- 풀 대기열이 가득 차면 그 자리(워커)에서 처리하고, 응답은 똑같이 `on_io_complete`로 쓴다
  (`waitfor` 없이 등록하면 코어가 재우지 않고 바로 콜백을 부른다).
- 본문 버퍼는 `accept`→`execute` 사이에 연결 슬롯(`store_engine_specific`)에 둔다.
  대기 중 상태는 `cb_data`로 넘긴다. `abort`는 슬롯에 남은 것을 버린다.

### 6.3 같은 (index, vkey) 그래프 작업의 순서

엔진 쓰기는 `execute`에서 바로 하고(엔진 안에서 원자적), **그래프 작업만** 줄을 세운다.

- 풀은 스레드마다 대기열을 갖는다. vkey가 있는 작업(upsert, setmeta)은
  `hash(index, vkey) % 풀 스레드 수`의 스레드로 간다. 검색은 아무 스레드나 쓴다.
- **소유 표**: 샤드 락 아래 `hash(index, vkey) → 지금 그 키의 그래프 작업을 가진 커넥션`.
  - 비어 있으면 자기를 적고 그 풀 스레드에 제출한다.
  - 차 있으면 `link_io_wait(owner, me, task)`로 소유자 뒤에 잇는다(샤드 락 안에서).
    풀에는 제출하지 않는다. `waitfor`/`on_io_complete`는 이미 해 둔 상태다.
- 풀 스레드는 소유자의 작업을 끝내면 샤드 락을 잡고 `next_io_wait(owner)`로 다음 커넥션과
  그 task를 꺼낸다. 남은 대기열을 다음 커넥션에게 옮기고(`next_io_wait`로 마저 꺼내
  `link_io_wait(next, ...)`), 소유를 넘기고, `notify_io_complete(owner)`로 끝난 커넥션을 깨운
  뒤 **같은 스레드에서 이어서** 다음 작업을 한다. 다음이 없으면 표에서 지운다.
- 그래서 같은 키의 그래프 작업은 한 스레드에서 도착 순서대로 돌고, 줄 선 커넥션은
  한 번만 기다린다.

엔진 이벤트와 그래프 작업 사이의 경합(예: A가 넣은 주소를 그래프에 넣기 전에 B가 replace)은
기존 `waiting` 장부의 상태 전이(`Pending`/`Reserved`/`Claimed`/`Dropped`)가 처리한다. 소유
표는 순서를 정할 뿐 정확성을 대신하지 않는다.

## 7. 지워지는 것

- `vlist`, `vstats`와 관련 코드.
- `block` 훅 관련 코드(`block_vector_cmd`, `wake_vector_cmd`, `conn_waking` 전제 주석).
- `arcus_event:` 접두사, 메타 요소(`META_ID`, `is_meta`, 요소에 담던 `MetaRecord`),
  "메타 전 도착" 처리.
- 1바이트 길이 + 255바이트 ATTR 형식.
- 다중 질의 검색, `QUERY`/`VALUE` 응답 형식.

## 8. 테스트

- 단위: 파서(8개 명령, 대소문자, 옵션 순서, 오류), 본문 자르기(구간별 `\r\n` 위반),
  JSON↔BSON 왕복, BSON 위 필터 평가(and/or, 따옴표, 공백 생략, 없는 필드), 요소 코덱,
  이벤트별 `target_ref`/`source_ref` 결정.
- 통합(`--features integration`): EE로 다시 빌드한 `data/`의 서버로 명령별 정상/오류 응답,
  upsert의 STORED→REPLACED, setmeta 후 필터 검색 결과 변화, 같은 vkey 동시 upsert 순서,
  풀 포화 시 그 자리 처리.

## 9. 사용자 확인이 필요한 구현 판단

1. upsert가 maxcount에 닿았을 때 `SERVER_ERROR index is full`.
2. 필터 `and`/`or` 우선순위: `and` 먼저(기준 PDF에 명시 없음).

## 10. 구현자에게 맡기는 것

- 모듈 구성, 타입 이름, 파일 배치.
- 풀 스레드 수와 스레드별 대기열 크기, 소유 표 샤드 수.
- 필터와 본문의 크기 상한 수치.
- 기존 retire/sweeper/배리어/waiting 구조는 유지하고 이벤트 API 이름과 참조 규칙만 맞춘다.
