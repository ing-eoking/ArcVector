# Map 데이터 제어

## Creation 명령

### mop create

Map 컬렉션을 생성합니다.

```regex
mop create <key> <attributes> [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `<attributes>` (필수) : 컬렉션의 속성을 정의하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#map-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
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

### mop insert

Map 컬렉션에 새로운 요소를 삽입합니다.
대상 키가 존재하지 않을 경우, 컬렉션을 생성함과 동시에 요소를 삽입할 수 있습니다.

```regex
mop insert <key> <mkey> <bytes> [create <attributes>] [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<mkey>`](../../2-핵심%20개념/02-컬렉션.md#mkey-map-key), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 저장할 값의 크기 (Byte)
- `create ...` (선택) : 대상 키가 없을 경우 컬렉션을 생성하며, 속성을 아래 규격에 맞춰 순서대로 입력
  ```regex
  create <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#map-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

요소가 성공적으로 삽입되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

만약, `create ...` 옵션이 함께 사용되었고, Map 컬렉션이 성공적으로 생성된 후 요소가 삽입되었다면 `CREATED_STORED`를 반환합니다.

```regex
CREATED_STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 삽입 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Map 타입이 아님 |
| **`OVERFLOWED`** | Maxcount를 초과하여 삽입 실패  |
| **`ELEMENT_EXISTS`** | 동일한 MKey를 가진 요소가 이미 존재함 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### mop upsert

Map 컬렉션에 요소를 삽입하거나 변경합니다. 대상 키가 존재하지 않을 경우, 컬렉션을 생성함과 동시에 요소를 삽입할 수 있습니다.

```regex
mop upsert <key> <mkey> <bytes> [create <attributes>] [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<mkey>`](../../2-핵심%20개념/02-컬렉션.md#mkey-map-key), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 저장 또는 변경할 값의 크기 (Byte)
- `create ...` (선택) : 대상 키가 없을 경우 컬렉션을 생성하며, 속성을 아래 규격에 맞춰 순서대로 입력
  ```regex
  create <flags> <expiretime> <maxcount> [<overflowaction>] [unreadable]
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<maxcount>`](../../2-핵심%20개념/02-컬렉션.md#maxcount) (필수)
  - [`<overflowaction>`](../../2-핵심%20개념/02-컬렉션.md#map-overflowaction), [`unreadable`](../../2-핵심%20개념/02-컬렉션.md#unreadable) (선택)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

요소가 성공적으로 삽입되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

이미 존재하는 요소가 성공적으로 변경되면 `REPLACED`를 반환합니다.

```regex
REPLACED\r\n
```

만약, `create ...` 옵션이 함께 사용되었고, Map 컬렉션이 성공적으로 생성된 후 요소가 삽입되었다면 `CREATED_STORED`를 반환합니다.

```regex
CREATED_STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 삽입 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Map 타입이 아님 |
| **`OVERFLOWED`** | Maxcount를 초과하여 삽입 실패 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### mop update

Map 컬렉션에서 기존의 요소를 변경합니다.

```regex
mop update <key> <mkey> <bytes> [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<mkey>`](../../2-핵심%20개념/02-컬렉션.md#mkey-map-key), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 변경할 값의 크기 (Byte)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

요소가 성공적으로 변경되면 `UPDATED`를 반환합니다.

```regex
UPDATED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 변경 실패 |
| **`NOT_FOUND_ELEMENT`** | 대상 MKey에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Map 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Retrieval 명령

### mop get

Map 컬렉션에서 한 번에 여러 개의 MKey를 지정하여 요소를 조회합니다.

```regex
mop get <key> <lenmkeys> <nummkeys> [delete|drop]\r\n
<mkey> <mkey> ...\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `<lenmkeys>` (필수) : MKey 목록 문자열의 전체 길이 (Byte)
- `<nummkeys>` (필수) : 조회할 MKey의 총 개수
- `delete` / `drop` (선택) : 조회 후 처리 방식 (택 1)
  - `delete` : 조회한 요소 제거
  - `drop` : 조회한 요소 제거 후, 빈 컬렉션이 되면 컬렉션 자체를 제거
- `<mkey> ...` (필수) : 공백(Space)을 구분자로 사용하여 나열한 MKey 목록

> [!INFO] 🔎 참고 사항
>
> `<lenmkeys>`, `<nummkeys>`를 모두 0으로 지정하는 경우 Map 컬렉션 내부의 모든 요소를 조회합니다.

**응답(Response)**

조회된 데이터는 `VALUE` 블록으로 시작하며, 마지막 줄에는 처리 결과에 따른 종료 메시지가 출력됩니다.

```regex
VALUE <flags> <count>\r\n
<mkey> <bytes> <value>\r\n
...
END|DELETED|DELETED_DROPPED\r\n
```

- `VALUE ...`
  - 컬렉션의 `<flags>`와 조회에 성공한 요소의 개수인 `<count>`를 출력
  - `<count>`만큼 데이터 블록(`<mkey>`, `<bytes>`, `<value>`)이 반복해서 출력
- **종료 메시지**
  - `END` : 데이터 반환의 종료를 의미
  - `DELETED` : 데이터 반환 종료 후, 해당 요소가 제거되었음을 의미
  - `DELETED_DROPPED` : 데이터 반환 종료 후, 요소 제거와 컬렉션 제거가 모두 완료되었음을 의미

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 조회 실패 |
| **`NOT_FOUND_ELEMENT`** | MKey 목록에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Map 타입이 아님 |
| **`UNREADABLE`** | 대상 키가 존재하지만 조회 가능한 상태가 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Deletion 명령

### delete

Map 컬렉션 전체를 제거할 경우, Key-Value 제어의 [**delete**](../0-기본%20명령어/00-Key-Value%20제어.md#delete) 명령어를 사용합니다.

### mop delete

Map 컬렉션에서 한 번에 여러 개의 MKey를 지정하여 요소를 제거합니다.

```regex
mop delete <key> <lenmkeys> <nummkeys> [drop] [noreply]\r\n
<mkey> <mkey> ...\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<mkey>`](../../2-핵심%20개념/02-컬렉션.md#mkey-map-key) (필수)
- `<lenmkeys>` (필수) : MKey 목록 문자열의 전체 길이 (Byte)
- `<nummkeys>` (필수) : 제거할 MKey의 총 개수
- `drop` (선택) : 요소를 제거한 후, 빈 컬렉션이 되면 컬렉션 자체를 제거
- `noreply` (선택) : 설정 시 서버 응답을 생략
- `<mkey> ...` (필수) : 공백(Space)을 구분자로 사용하여 나열한 MKey 목록

> [!INFO] 🔎 참고 사항
>
> `<lenmkeys>`, `<nummkeys>`를 모두 0으로 지정하는 경우 Map 컬렉션 내부의 모든 요소를 제거합니다.

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
| **`NOT_FOUND_ELEMENT`** | MKey 목록에 해당하는 요소가 없음 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Map 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |
