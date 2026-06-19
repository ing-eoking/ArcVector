# List 데이터 제어

## Creation 명령

### lop create

List 컬렉션을 생성합니다.

```regex
lop create <key> <attributes> [noreply]\r\n
```

**매개변수(Parameters)**
- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `<attributes>` (필수) : 컬렉션의 속성을 정의하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#list-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
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

### lop insert

List 컬렉션의 특정 인덱스 위치에 새로운 요소를 삽입합니다. 대상 키가 존재하지 않을 경우, 컬렉션을 생성함과 동시에 요소를 삽입할 수 있습니다.

```regex
lop insert <key> <index> <bytes> [create <attributes>] [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<index>`](../../2-핵심%20개념/02-컬렉션.md#%E1%84%8B%E1%85%B5%E1%86%AB%E1%84%83%E1%85%A6%E1%86%A8%E1%84%89%E1%85%B3-index
), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 저장할 값의 크기 (Byte)
- `create ...` (선택) : 대상 키가 없을 경우 컬렉션을 생성하며, 속성을 아래 규격에 맞춰 순서대로 입력
  ```regex
  create <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#list-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

요소가 성공적으로 삽입되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

만약, `create ...` 옵션이 함께 사용되었고, List 컬렉션이 성공적으로 생성된 후 요소가 삽입되었다면 `CREATED_STORED`를 반환합니다.

```regex
CREATED_STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 삽입 실패 |
| **`OVERFLOWED`** | Overflowaction이 ERROR일 때, Maxcount를 초과하여 삽입 실패 |
| **`OUT_OF_RANGE`** | 인덱스가 유효한 범위를 벗어남 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 List 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Retrieval 명령

### lop get

List 컬렉션에서 지정한 인덱스 또는 인덱스 범위에 해당하는 요소를 조회합니다.

```regex
lop get <key> <index>[..<index>] [delete|drop]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- [`<index>`](../../2-핵심%20개념/02-컬렉션.md#%E1%84%8B%E1%85%B5%E1%86%AB%E1%84%83%E1%85%A6%E1%86%A8%E1%84%89%E1%85%B3-index
) (필수)
  - **단일 지정** : 인덱스 숫자를 하나만 입력
  - **범위 지정** : 시작 인덱스 뒤에 `..끝 인덱스`를 붙여 범위를 지정
- `delete` / `drop` (선택) : 조회 후 처리 방식 (택 1)
  - `delete` : 조회한 요소 제거
  - `drop` : 조회한 요소 제거 후, 빈 컬렉션이 되면 컬렉션 자체를 제거

**응답(Response)**

조회된 데이터는 `VALUE` 블록으로 시작하며, 마지막 줄에는 처리 결과에 따른 종료 메시지가 출력됩니다.

```regex
VALUE <flags> <count>\r\n
<bytes> <value>\r\n
...
END|DELETED|DELETED_DROPPED\r\n
```

- `VALUE ...`
  - 컬렉션의 `<flags>`와 조회에 성공한 요소의 개수인 `<count>`를 반환
  - `<count>`만큼 데이터 블록(`<bytes>`, `<value>`)이 반복해서 출력
- **종료 메시지**
  - `END` : 데이터 반환의 종료를 의미
  - `DELETED` : 데이터 반환 종료 후, 해당 요소가 제거되었음을 의미
  - `DELETED_DROPPED` : 데이터 반환 종료 후, 요소 제거와 컬렉션 제거가 모두 완료되었음을 의미

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 조회 실패 |
| **`NOT_FOUND_ELEMENT`** | 인덱스 또는 인덱스 범위에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 List 타입이 아님 |
| **`UNREADABLE`** | 대상 키가 존재하지만 조회 가능한 상태가 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Deletion 명령

### delete

List 컬렉션 전체를 제거할 경우, Key-Value 제어의 [**delete**](../0-기본%20명령어/00-Key-Value%20제어.md#delete) 명령어를 사용합니다.

---

### lop delete

List 컬렉션에서 하나의 인덱스 또는 인덱스 범위에 해당하는 요소를 제거합니다.

```regex
lop delete <key> <index>[..<index>] [drop] [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- [`<index>`](../../2-핵심%20개념/02-컬렉션.md#%E1%84%8B%E1%85%B5%E1%86%AB%E1%84%83%E1%85%A6%E1%86%A8%E1%84%89%E1%85%B3-index
) (필수)
  - **단일 지정** : 인덱스 숫자를 하나만 입력
  - **범위 지정** : 시작 인덱스 뒤에 `..끝 인덱스`를 붙여 범위를 지정
- `drop` (선택) : 요소 제거 후, 빈 컬렉션이 되면 컬렉션 자체를 제거
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
| **`NOT_FOUND_ELEMENT`** | 인덱스 또는 인덱스 범위에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 List 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |
