# B+Tree 데이터 제어

## Creation 명령

### bop create

B+Tree 컬렉션을 생성합니다.

```regex
bop create <key> <attributes> [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `<attributes>` (필수) : 컬렉션의 속성을 정의하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#b-tree-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

컬렉션이 성공적으로 생성되면 `CREATED`를 반환합니다.

```regex
CREATED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`EXISTS`** | 대상 키가 이미 존재하여 생성 실패 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Storage 명령

### bop insert

B+Tree 컬렉션에 새로운 요소를 삽입합니다.
대상 키가 존재하지 않을 경우, 컬렉션을 생성함과 동시에 요소를 삽입할 수 있습니다.

```regex
bop insert <key> <bkey> [<eflag>] <bytes> [create <attributes>] [noreply|getrim]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- [`<eflag>`](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) (선택)
- `<bytes>` (필수) : 저장할 값의 크기 (Byte)
- `create ...` (선택) : 대상 키가 없을 경우 컬렉션을 생성하며, 속성을 아래 규격에 맞춰 순서대로 입력
  ```regex
  create <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#b-tree-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
- `noreply` / `getrim` (선택) : 응답 처리 방식 (택 1)
  - `noreply` (선택) : 설정 시 서버 응답을 생략
  - `getrim` (선택) : 설정 시 Trim된 요소가 있으면 해당 정보를 함께 반환

**응답(Response)**

요소가 성공적으로 삽입되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

만약, `create ...` 옵션이 함께 사용되었고, B+Tree 컬렉션이 성공적으로 생성된 후 요소가 삽입되었다면 `CREATED_STORED`를 반환합니다.

```regex
CREATED_STORED\r\n
```

만약, `getrim` 옵션 사용 시 요소 삽입 후 Trim된 요소가 있으면 `VALUE` 블록 및 `TRIMMED`를 반환합니다.

```regex
VALUE <flags> 1\r\n
<bkey> [<eflag>] <bytes> <value>\r\n
TRIMMED\r\n
```

- `VALUE ...`
  - 컬렉션의 `<flags>`와 1을 반환합니다.
  - Trim된 요소의 데이터 블록(`<bkey>`, `<eflag>`, `<bytes>`, `<value>`)이 출력됩니다.
  <br> (단, eflag는 설정된 요소에 한해 출력됨)
- `TRIMMED`
  - 데이터 반환의 종료를 의미

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 삽입 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`OVERFLOWED`** | Overflowaction이 ERROR일 때, Maxcount를 초과하여 삽입 실패 |
| **`OUT_OF_RANGE`** | • Maxbkeyrange에 의한 삽입 실패 <br> • 대상 BKey가 Trim 영역에 해당하여 삽입 실패 |
| **`ELEMENT_EXISTS`** | 동일한 BKey를 가진 요소가 이미 존재함 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop upsert

B+Tree 컬렉션에 요소를 삽입하며, 동일한 BKey를 가진 요소가 이미 존재하면 해당 요소를 변경합니다.
대상 키가 존재하지 않을 경우, 컬렉션을 생성함과 동시에 요소를 삽입할 수 있습니다.

```regex
bop upsert <key> <bkey> [<eflag>] <bytes> [create <attributes>] [noreply|getrim]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- [`<eflag>`](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) (선택)
- `<bytes>` (필수) : 저장할 값의 크기 (Byte)
- `create ...` (선택) : 대상 키가 없을 경우 컬렉션을 생성하며, 속성을 아래 규격에 맞춰 순서대로 입력
  ```regex
  create <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#b-tree-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
- `noreply` / `getrim` (선택) : 응답 처리 방식 (택 1)
  - `noreply` (선택) : 설정 시 서버 응답을 생략
  - `getrim` (선택) : 설정 시 Trim된 요소가 있으면 해당 정보를 함께 반환

**응답(Response)**

요소가 성공적으로 삽입되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

이미 존재하는 요소가 성공적으로 대체되면 `REPLACED`를 반환합니다.

```regex
REPLACED\r\n
```

만약, `create ...` 옵션이 함께 사용되었고, B+Tree 컬렉션이 성공적으로 생성된 후 요소가 삽입되었다면 `CREATED_STORED`를 반환합니다.

```regex
CREATED_STORED\r\n
```

만약, `getrim` 옵션 사용 시 요소 삽입 후 Trim된 요소가 있으면 `VALUE` 블록 및 `TRIMMED`를 반환합니다.

```regex
VALUE <flags> 1\r\n
<bkey> [<eflag>] <bytes> <value>\r\n
TRIMMED\r\n
```

- `VALUE ...`
  - 컬렉션의 `<flags>`와 1을 반환합니다.
  - Trim된 요소의 데이터 블록(`<bkey>`, `<eflag>`, `<bytes>`, `<value>`)이 출력됩니다.
  <br> (단, eflag는 설정된 요소에 한해 출력됨)
- `TRIMMED`
  - 데이터 반환의 종료를 의미

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 삽입 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`OVERFLOWED`** | Overflowaction이 ERROR일 때, Maxcount를 초과하여 삽입 실패 |
| **`OUT_OF_RANGE`** | • Maxbkeyrange에 의한 삽입 실패 <br> • 대상 BKey가 Trim 영역에 해당하여 삽입 실패 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop update

B+Tree 컬렉션에서 기존의 요소를 변경합니다.

```regex
bop update <key> <bkey> [<eflag_update>] <bytes> [noreply|pipe]\r\n
[<value>\r\n]
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
- `<eflag_update>` (선택) : 대상 요소의 EFlag를 변경하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  [<offset> <bitwop>] <eflag>
  ```
  - `<offset>` (선택) : 연산을 시작할 EFlag의 바이트 위치 (0 ~ 30)
  - `<bitwop>` (선택) : 비트 연산자 (`&`, `|`, `^`)
  - `<eflag>` (필수) : 변경할 EFlag (단, `<offset>`, `<bitwop>`가 주어진 경우, 비트 연산에 사용)
- `<bytes>` (필수) : 변경할 값의 바이트 크기 (-1 ~ 2,147,483,645) (단, -1 입력 시 값을 수정하지 않음)
- `noreply` (선택) : 설정 시 서버 응답을 생략
- [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (선택)

**응답(Response)**

요소가 성공적으로 변경되면 `UPDATED`를 반환합니다.

```regex
UPDATED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 변경 실패 |
| **`NOT_FOUND_ELEMENT`** | 대상 BKey에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`EFLAG_MISMATCH`** | 주어진 비트 연산이 적합하지 않음 |
| **`NOTHING_TO_UPDATE`** | 값 변경과 EFlag 변경 중 어느 것도 지정되지 않음 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Arithmetic 명령

### bop incr

B+Tree 컬렉션에서 특정 요소의 기존 값이 숫자일 경우, 지정한 값만큼 증가시킵니다.
대상 요소가 존재하지 않을 경우, 증가 연산 없이 신규 요소를 삽입할 수 있습니다.

```regex
bop incr <key> <bkey> <delta> [<new_elem>] [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
- `<delta>` (필수) : 증가시킬 값 (0 ~ 2⁶⁴-1)
  - 연산 결과가 2⁶⁴-1을 초과하면, 0에서부터 초과한 수치만큼 다시 증가
- `<new_elem>` (선택) : 대상 요소가 없을 경우 신규 요소를 삽입하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <initial> [<eflag>]
  ```
  - `<initial>` (필수) : 초기값 (0 ~ 2⁶⁴-1)
  - [`<eflag>`](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) (선택)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

연산이 성공하면 증감 후의 데이터 값을 반환합니다.

```regex
<value>\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 실패 |
| **`NOT_FOUND_ELEMENT`** | 대상 BKey에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`OVERFLOWED`** | Overflowaction이 ERROR일 때, Maxcount를 초과하여 삽입 실패 |
| **`OUT_OF_RANGE`** | • Maxbkeyrange에 의한 삽입 실패 <br> • 대상 BKey가 Trim 영역에 해당하여 삽입 실패 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

### bop decr

B+Tree 컬렉션에서 특정 요소의 기존 값이 숫자일 경우, 지정한 값만큼 감소시킵니다.
대상 요소가 존재하지 않을 경우, 감소 연산 없이 신규 요소를 삽입할 수 있습니다.

```regex
bop decr <key> <bkey> <delta> [<new_elem>] [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
- `<delta>` (필수) : 감소시킬 값 (0 ~ 2⁶⁴-1)
  - 연산 결과가 0 미만이 되면 0으로 설정
- `<new_elem>` (선택) : 대상 요소가 없을 경우 신규 요소를 삽입하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <initial> [<eflag>]
  ```
  - `<initial>` (필수) : 초기값 (0 ~ 2⁶⁴-1)
  - [`<eflag>`](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) (선택)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

연산이 성공하면 증감 후의 데이터 값을 반환합니다.

```regex
<value>\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 실패 |
| **`NOT_FOUND_ELEMENT`** | 대상 BKey에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`OVERFLOWED`** | Overflowaction이 ERROR일 때, Maxcount를 초과하여 삽입 실패 |
| **`OUT_OF_RANGE`** | • Maxbkeyrange에 의한 삽입 실패 <br> • 대상 BKey가 Trim 영역에 해당하여 삽입 실패 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |


## Retrieval 명령

### bop get

B+Tree 컬렉션에서 하나의 BKey 또는 BKey 범위에 해당하는 요소를 조회합니다.

```regex
bop get <key> <bkey>[..<bkey>] [<eflag_filter>] [<range>] [delete|drop]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
  - **단일 지정** : BKey 하나만 입력
  - **범위 지정** : 시작 BKey 뒤에 `..끝 BKey`를 붙여 범위를 지정
- `<eflag_filter>` (선택) : 조회할 요소를 [eflag](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) 값 기준으로 추가로 걸러내며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <offset> [<bitwop> <bitwvalue>] <compop> <compvalue>
  ```
  - `<offset>` (필수) : 저장된 eflag에서 연산을 시작할 바이트 위치 (0 ~ 30)
  - `<bitwop>` (선택) : 적용할 비트 연산자 (`&`, `|`, `^`)
  - `<bitwvalue>` (선택) : 비트 연산할 eflag 값
  - `<compop>` (필수) : 비교 연산자 (`EQ`, `NE`, `LT`, `LE`, `GT`, `GE`)
  - `<compvalue>` (필수) : 비교 대상 eflag 값 (비트 연산 시 그 결과와 비교)
- `<range>` (선택) : 조회 조건을 만족하는 요소 중 반환할 범위를 지정하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  [<offset>] <count>
  ```
  - `<offset>` (선택) : 건너뛸 요소 개수 (기본값 0)
  - `<count>` (필수) : 반환할 요소 최대 개수
- `delete` / `drop` (선택) : 조회 후 처리 방식 (택 1)
  - `delete` : 조회한 요소 제거
  - `drop` : 조회한 요소 제거 후, 빈 컬렉션이 되면 컬렉션 자체를 제거

**응답(Response)**

조회된 데이터는 `VALUE` 블록으로 시작하며, 마지막 줄에는 처리 결과에 따른 종료 메시지가 출력됩니다.

```regex
VALUE <flags> <count>\r\n
<bkey> [<eflag>] <bytes> <data>\r\n
...
END|TRIMMED|DELETED|DELETED_DROPPED\r\n
```

- `VALUE ...`
  - 컬렉션의 `<flags>`와 조회에 성공한 요소의 개수인 `<count>`를 반환
  - `<count>`만큼 데이터 블록(`<bkey>`, `<eflag>`, `<bytes>`, `data`)이 반복해서 출력
  <br>(단, `<eflag>`는 설정된 요소에 한해 출력됨)
- **종료 메시지**
  - `END` : 데이터 반환의 종료를 의미
  - `TRIMMED` : 데이터 반환 종료 및 조회 범위 일부가 Trim 영역과 겹침을 의미
  - `DELETED` : 데이터 반환 종료 후, 해당 요소가 제거되었음을 의미
  - `DELETED_DROPPED` : 데이터 반환 종료 후, 요소 제거와 컬렉션 제거가 모두 완료되었음을 의미

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 조회 실패 |
| **`NOT_FOUND_ELEMENT`** | 조회 조건을 만족하는 요소가 없음 |
| **`OUT_OF_RANGE`** | 조회 조건을 만족하는 요소가 없으며, 조회 범위가 Trim 영역과 겹침 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`UNREADABLE`** | 대상 키가 존재하지만 조회 가능한 상태가 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop count

B+Tree 컬렉션에서 하나의 BKey 또는 BKey 범위에 해당하는 요소의 개수를 조회합니다.

```regex
bop count <key> <bkey>[..<bkey>] [<eflag_filter>]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
  - **단일 지정** : BKey 하나만 입력
  - **범위 지정** : 시작 BKey 뒤에 `..끝 BKey`를 붙여 범위를 지정
- `<eflag_filter>` (선택) : 조회할 요소를 [eflag](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) 값 기준으로 추가로 걸러내며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <offset> [<bitwop> <bitwvalue>] <compop> <compvalue>
  ```
  - `<offset>` (필수) : 저장된 eflag에서 연산을 시작할 바이트 위치 (0 ~ 30)
  - `<bitwop>` (선택) : 적용할 비트 연산자 (`&`, `|`, `^`)
  - `<bitwvalue>` (선택) : 비트 연산할 eflag 값
  - `<compop>` (필수) : 비교 연산자 (`EQ`, `NE`, `LT`, `LE`, `GT`, `GE`)
  - `<compvalue>` (필수) : 비교 대상 eflag 값 (비트 연산 시 그 결과와 비교)

**응답(Response)**

조회에 성공하면 조건을 만족하는 요소의 개수를 반환합니다.

```regex
COUNT=<count>\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 조회 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`UNREADABLE`** | 대상 키가 존재하지만 조회 가능한 상태가 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop mget

여러 B+Tree 컬렉션에서 하나의 BKey 또는 BKey 범위에 해당하는 요소를 한꺼번에 조회합니다.

```regex
bop mget <lenkeys> <numkeys> <bkey>[..<bkey>] [<eflag_filter>] <range>\r\n
<key> ...\r\n
```

**매개변수(Parameters)**

- `<lenkeys>` (필수) : 키 목록 문자열의 전체 길이 (Byte)
- `<numkeys>` (필수) : 조회할 키의 총 개수 (0 ~ 200)
- [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
  - **단일 지정** : BKey 하나만 입력
  - **범위 지정** : 시작 BKey 뒤에 `..끝 BKey`를 붙여 범위를 지정
- `<eflag_filter>` (선택) : 조회할 요소를 [eflag](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) 값 기준으로 추가로 걸러내며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <offset> [<bitwop> <bitwvalue>] <compop> <compvalue>
  ```
  - `<offset>` (필수) : 저장된 eflag에서 연산을 시작할 바이트 위치 (0 ~ 30)
  - `<bitwop>` (선택) : 적용할 비트 연산자 (`&`, `|`, `^`)
  - `<bitwvalue>` (선택) : 비트 연산할 eflag 값
  - `<compop>` (필수) : 비교 연산자 (`EQ`, `NE`, `LT`, `LE`, `GT`, `GE`)
  - `<compvalue>` (필수) : 비교 대상 eflag 값 (비트 연산 시 그 결과와 비교)
- `<range>` (필수) : 조회 조건을 만족하는 요소 중 반환할 범위를 지정하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  [<offset>] <count>
  ```
  - `<offset>` (선택) : 건너뛸 요소 개수 (기본값 0)
  - `<count>` (필수) : 반환할 요소 최대 개수 (1 ~ 50)
- `<key> ...` (필수) : 공백(Space)을 구분자로 사용하여 나열한 키 목록

**응답(Response)**

조회된 데이터는 대상 키마다 `VALUE` 블록으로 반환되며, 모든 키의 출력이 끝나면 `END`로 마무리됩니다.

```regex
VALUE <key> <status> [<flags> <count>]\r\n
ELEMENT <bkey> [<eflag>] <bytes> <data>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 키마다 반복 출력되며, 각 키와 조회 상태(`status`)를 함께 반환
    - `OK` : 정상 조회 완료
    - `TRIMMED` : 정상 조회되었으나 조회 범위 일부가 Trim 영역과 겹침
    - 이 외는 [bop get](#bop-get)의 예외 및 오류 응답 메시지와 동일 (`CLIENT_ERROR`, `SERVER_ERROR` 제외)
  - 정상 조회 시 `<flags>`와 조회에 성공한 요소의 개수(`<count>`)도 포함
- `ELEMENT ...`
  - 대상 키가 정상 조회된 경우 `<count>`만큼 반복 출력
  - `<bkey>`, `<eflag>`, `<bytes>`, `data`를 포함 (단, `<eflag>`는 설정된 요소에 한해 출력)
- `END`
  - 데이터 반환의 종료를 의미

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop smget

여러 B+Tree 컬렉션에서 하나의 BKey 또는 BKey 범위에 해당하는 요소를 정렬 병합(Sort Merge) 방식으로 한꺼번에 조회합니다.

```regex
bop smget <lenkeys> <numkeys> <bkey>[..<bkey>] [<eflag_filter>] <count> duplicate|unique\r\n
<key> ...\r\n
```

**매개변수(Parameters)**

- `<lenkeys>` (필수) : 키 목록 문자열의 전체 길이 (Byte)
- `<numkeys>` (필수) : 조회할 키의 총 개수 (0 ~ 10,000)
- [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
  - **단일 지정** : BKey 하나만 입력
  - **범위 지정** : 시작 BKey 뒤에 `..끝 BKey`를 붙여 범위를 지정
- `<eflag_filter>` (선택) : 조회할 요소를 [eflag](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) 값 기준으로 추가로 걸러내며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <offset> [<bitwop> <bitwvalue>] <compop> <compvalue>
  ```
  - `<offset>` (필수) : 저장된 eflag에서 연산을 시작할 바이트 위치 (0 ~ 30)
  - `<bitwop>` (선택) : 적용할 비트 연산자 (`&`, `|`, `^`)
  - `<bitwvalue>` (선택) : 비트 연산할 eflag 값
  - `<compop>` (필수) : 비교 연산자 (`EQ`, `NE`, `LT`, `LE`, `GT`, `GE`)
  - `<compvalue>` (필수) : 비교 대상 eflag 값 (비트 연산 시 그 결과와 비교)
- `<count>` (필수) : 반환할 요소 최대 개수 (1 ~ 2,000)
- `duplicate` / `unique` (필수) : 조회 과정에서 중복 BKey가 존재할 경우의 처리 방식 (택 1)
  - `duplicate` : 중복 BKey 허용
  - `unique` : 중복 BKey 제외
- `<key> ...` (필수) : 공백(Space)을 구분자로 사용하여 나열한 키 목록

**응답(Response)**

조회된 데이터는 `ELEMENTS` 블록으로 시작하며, 마지막 줄에는 처리 결과에 따른 종료 메시지가 출력됩니다.

```regex
ELEMENTS <count>\r\n
<key> <flags> <bkey> [<eflag>] <bytes> <data>\r\n
...
MISSED_KEYS <count>\r\n
<key> <cause>\r\n
...
TRIMMED_KEYS <count>\r\n
<key> <bkey>\r\n
...
END|DUPLICATED\r\n
```

- `ELEMENTS ...`
  - 조회에 성공한 요소의 개수(`<count>`)를 반환
  - `<count>`만큼 데이터 블록(`<key>`, `<flags>`, `<bkey>`, `<eflag>`, `<bytes>`, `data`)이 반복해서 출력
  <br>(단, `<eflag>`는 설정된 요소에 한해 출력됨)
- `MISSED_KEYS ...`
  - 조회에 실패한 키의 개수(`<count>`)를 반환
  - `<count>`만큼 키와 실패 원인(`cause`)이 반복해서 출력
    - `NOT_FOUND` : 대상 키가 존재하지 않아 조회 실패
    - `UNREADABLE` : 대상 키가 존재하지만 조회 가능한 상태가 아님
    - `OUT_OF_RANGE` : 조회 조건을 만족하는 요소가 없으며, 조회 범위가 Trim 영역과 겹침
- `TRIMMED_KEYS ...`
  - 조회 범위 일부가 Trim 영역과 겹치는 키의 개수(`<count>`)를 반환
  - `<count>`만큼 키와 대상 키에서 Trim 영역에 가장 인접한 BKey를 함께 출력
- **종료 메시지**
  - `END` : 데이터 반환 종료 후, 중복 BKey가 존재하지 않음
  - `DUPLICATED` : 데이터 반환 종료 후, 중복 BKey가 존재함

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`TYPE_MISMATCH`** | 대상 키 중 B+Tree 타입이 아닌 키가 존재 |
| **`BKEY_MISMATCH`** | 모든 대상 B+Tree의 BKey 타입이 일치하지 않음 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop position

B+Tree 컬렉션에서 특정 요소의 위치를 조회합니다.

```regex
bop position <key> <bkey> <order>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
- `<order>` (필수) : 대상 요소의 위치를 산출할 순서
  - `asc` : 작은 BKey부터 0번으로 계산 (오름차순)
  - `desc` : 큰 BKey부터 0번으로 계산 (내림차순)

**응답(Response)**

조회에 성공하면 대상 요소의 위치를 반환합니다.

```regex
POSITION=<position>\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 조회 실패 |
| **`NOT_FOUND_ELEMENT`** | 대상 BKey에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`UNREADABLE`** | 대상 키가 존재하지만 조회 가능한 상태가 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop gbp

B+Tree 컬렉션에서 위치 기반으로 요소를 조회합니다.

```regex
bop gbp <key> <order> <position>[..<position>]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `<order>` (필수) : 요소의 위치를 산출할 정렬 순서
  - `asc` : 작은 BKey부터 0번으로 계산 (오름차순)
  - `desc` : 큰 BKey부터 0번으로 계산 (내림차순)
- `<position>` (필수) : 조회할 요소의 위치 (`<order>` 기준으로 산출된 0부터 시작하는 값)
  - **단일 지정** : 위치 하나만 입력
  - **범위 지정** : 시작 위치 뒤에 `..끝 위치`를 붙여 범위를 지정

**응답(Response)**

조회된 데이터는 `VALUE` 블록으로 시작하며, 마지막 줄에 `END`가 출력됩니다.

```regex
VALUE <flags> <count>\r\n
<bkey> [<eflag>] <bytes> <data>\r\n
...
END\r\n
```

- `VALUE ...`
  - 컬렉션의 `<flags>`와 조회에 성공한 요소 개수(`<count>`)를 반환
  - `<count>`만큼 데이터 블록(`<bkey>`, `<eflag>`, `<bytes>`, `data`)이 반복해서 출력 (단, `<eflag>`는 설정된 요소에 한해 출력됨)

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 조회 실패 |
| **`NOT_FOUND_ELEMENT`** | 대상 위치에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`UNREADABLE`** | 대상 키가 존재하지만 조회 가능한 상태가 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### bop pwg

B+Tree 컬렉션에서 특정 요소의 위치를 조회하면서, 앞뒤 양방향으로 위치한 요소들도 함께 조회합니다.

```regex
bop pwg <key> <bkey> <order> [<count>]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
- `<order>` (필수) : 대상 요소의 위치를 산출할 순서
  - `asc` : 작은 BKey부터 0번으로 계산 (오름차순)
  - `desc` : 큰 BKey부터 0번으로 계산 (내림차순)
- `<count>` (선택) : 대상 요소의 앞뒤에서 각각 조회할 요소 개수 (0 ~ 100)

**응답(Response)**

조회된 데이터는 `VALUE` 블록으로 시작하며, 마지막 줄에 `END`가 출력됩니다.

```regex
VALUE <position> <flags> <count> <index>\r\n
<bkey> [<eflag>] <bytes> <data>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 요소의 위치(`<position>`), 컬렉션의 `<flags>`, 조회에 성공한 요소 개수(`<count>`), 반환된 데이터 블록 내 대상 요소의 순번(`index`, 0부터 시작)을 반환
  - `<count>`만큼 데이터 블록(`<bkey>`, `<eflag>`, `<bytes>`, `data`)이 반복해서 출력
  <br>(단, `<eflag>`는 설정된 요소에 한해 출력됨)

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 조회 실패 |
| **`NOT_FOUND_ELEMENT`** | 대상 BKey에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`UNREADABLE`** | 대상 키가 존재하지만 조회 가능한 상태가 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Deletion 명령

### delete

B+Tree 컬렉션 전체를 제거할 경우, Key-Value 제어의 [delete](../0-기본%20명령어/00-Key-Value%20제어.md#delete) 명령어를 사용합니다.

---

### bop delete

B+Tree 컬렉션에서 하나의 BKey 또는 BKey 범위에 해당하는 요소를 제거합니다.

```regex
bop delete <key> <bkey>[..<bkey>] [<eflag_filter>] [<count>] [drop] [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- [`<bkey>`](../../2-핵심%20개념/02-컬렉션.md#bkey-b-tree-key) (필수)
  - **단일 지정** : BKey 하나만 입력
  - **범위 지정** : 시작 BKey 뒤에 `..끝 BKey`를 붙여 범위를 지정
- `<eflag_filter>` (선택) : 제거할 요소를 [eflag](../../2-핵심%20개념/02-컬렉션.md#eflag-element-flag) 값 기준으로 추가로 걸러내며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <offset> [<bitwop> <bitwvalue>] <compop> <compvalue>
  ```
  - `<offset>` (필수) : 저장된 eflag에서 연산을 시작할 바이트 위치 (0 ~ 30)
  - `<bitwop>` (선택) : 적용할 비트 연산자 (`&`, `|`, `^`)
  - `<bitwvalue>` (선택) : 비트 연산할 eflag 값
  - `<compop>` (필수) : 비교 연산자 (`EQ`, `NE`, `LT`, `LE`, `GT`, `GE`)
  - `<compvalue>` (필수) : 비교 대상 eflag 값 (비트 연산 시 그 결과와 비교)
- `<count>` (선택) : 제거할 요소 개수 지정
- `drop` (선택) : 요소를 제거한 후, 빈 컬렉션이 되면 컬렉션 자체를 제거
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

요소가 성공적으로 제거되면 `DELETED`를 반환합니다.

```regex
DELETED\r\n
```

만약, `drop` 옵션을 함께 사용한 경우, 요소 제거 후 컬렉션이 비어 있으면 컬렉션까지 제거되며 `DELETED_DROPPED`를 반환합니다.

```regex
DELETED_DROPPED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 제거 실패 |
| **`NOT_FOUND_ELEMENT`** | 제거 조건을 만족하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 B+Tree 타입이 아님 |
| **`BKEY_MISMATCH`** | 대상 BKey 타입이 저장된 BKey 타입과 다름 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |
