# Key-Value 제어

## Storage 명령

### set

대상 키의 존재 여부와 상관없이 데이터를 저장하거나 새로운 값으로 변경합니다.

```regex
set <key> <flags> <expiretime> <bytes> [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 저장할 값의 실제 크기 (Byte)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

데이터가 성공적으로 저장되거나 변경되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### add

대상 키가 **존재하지 않을 때만** 신규 데이터를 저장합니다.

```regex
add <key> <flags> <expiretime> <bytes> [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 저장할 값의 실제 크기 (Byte)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

조건을 만족하여 데이터가 성공적으로 저장되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_STORED`** | 대상 키가 존재하여 저장 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### replace

대상 키가 **이미 존재할 때만** 새로운 값으로 변경합니다.

```regex
replace <key> <flags> <expiretime> <bytes> [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 저장할 값의 실제 크기 (Byte)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

조건을 만족하여 데이터가 성공적으로 변경되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_STORED`** | 대상 키가 존재하지 않아 변경 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### prepend

기존 값의 앞에 새로운 값을 추가합니다.

```regex
prepend <key> <flags> <expiretime> <bytes> [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 추가할 값의 실제 크기 (Byte)
- `noreply` (선택) : 설정 시 서버 응답을 생략

> [!NOTE] 🔎 참고 사항
>
> `flags`와 `expiretime`은 필수 인자이나, 실제로는 적용되지 않습니다.

**응답(Response)**

조건을 만족하여 데이터가 성공적으로 변경되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_STORED`** | 대상 키가 존재하지 않아 변경 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### append

기존 값의 뒤에 새로운 값을 추가합니다.

```regex
append <key> <flags> <expiretime> <bytes> [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 추가할 값의 실제 크기 (Byte)
- `noreply` (선택) : 설정 시 서버 응답을 생략

> [!NOTE] 🔎 참고 사항
>
> `flags`와 `expiretime`은 필수 인자이나, 실제로는 적용되지 않습니다.

**응답(Response)**

조건을 만족하여 데이터가 성공적으로 변경되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_STORED`** | 대상 키가 존재하지 않아 변경 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### cas

데이터를 조회한 이후부터 변경하려는 순간까지, 다른 사용자가 데이터를 변경하지 않았을 때만 안전하게 새로운 값으로 변경하는 명령어입니다.

```regex
cas <key> <flags> <expiretime> <bytes> <cas unique> [noreply]\r\n
<value>\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<value>`](../../2-핵심%20개념/01-데이터%20모델.md#value) (필수)
- `<bytes>` (필수) : 저장할 값의 실제 크기 (Byte)
- `cas unique` (필수)
  - 데이터를 조회([**gets**](#gets))했을 때 받은 고유 식별 번호
  - 이 번호가 서버의 현재 번호와 일치해야만 저장
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

조건을 만족하여 데이터가 성공적으로 변경되면 `STORED`를 반환합니다.

```regex
STORED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 변경 실패 |
| **`EXISTS`** | `cas unique` 값이 일치하지 않아 변경 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

### touch

기존 Expiretime을 새로운 Expiretime으로 변경합니다.

```regex
touch <key> <expiretime> [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes) (필수)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

Expiretime이 성공적으로 변경되면 `TOUCHED`를 반환합니다.

```regex
TOUCHED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 변경 실패 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Arithmetic 명령

### incr

기존 값이 숫자일 경우, 지정한 값만큼 증가시킵니다.
대상 키가 존재하지 않을 경우, 증가 연산 없이 신규 데이터를 저장할 수 있습니다.

```regex
incr <key> <delta> [<new_data>] [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `<delta>` (필수) : 증가시킬 값 (0 ~ 18,446,744,073,709,551,615)
  - 연산 결과가 18,446,744,073,709,551,615를 초과하면, 0에서부터 초과한 수치만큼 다시 증가
- `<new_data>` (선택) : 대상 키가 없을 경우 신규 데이터를 저장하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <flags> <expiretime> <initial>
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes) (필수)
  - `<initial>` (필수) : 초기값 (0 ~ 18,446,744,073,709,551,615)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

연산이 성공하면 변경 후의 값을 반환합니다.

```regex
<value>\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 실패 |
| **`TYPE_MISMATCH`** | 대상 키의 값이 숫자가 아니거나 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### decr

기존 값이 숫자일 경우, 지정한 값만큼 감소시킵니다.
대상 키가 존재하지 않을 경우, 감소 연산 없이 신규 데이터를 저장할 수 있습니다.

```regex
decr <key> <delta> [<new_data>] [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `<delta>` (필수) : 감소시킬 값 (0 ~ 18,446,744,073,709,551,615)
  - 연산 결과가 0 미만이 되면 0으로 설정
- `<new_data>` (선택) : 대상 키가 없을 경우 신규 데이터를 저장하며, 아래 규격에 맞춰 순서대로 입력
  ```regex
  <flags> <expiretime> <initial>
  ```
  - [`<flags>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes), [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes) (필수)
  - `<initial>` (필수) : 초기값 (0 ~ 18,446,744,073,709,551,615)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

연산이 성공하면 변경 후의 값을 반환합니다.

```regex
<value>\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 실패 |
| **`TYPE_MISMATCH`** | 대상 키가 존재하지만 Key-Value 타입이 아님 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Retrieval 명령

### get

하나 또는 여러 개의 키를 지정하여 데이터를 조회합니다.

```regex
get <key>[ <key> ...]\r\n
```

> [!WARNING] ⚠️ 주의 사항
>
> 여러 개의 키를 동시에 조회해야 할 경우, **get** 명령어보다는 [**mget**](#mget) 명령어 사용을 권장합니다.

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
  - 공백(Space)을 구분자로 사용하여 여러 키를 한 번에 전달 가능

**응답(Response)**

서버에 존재하는 데이터는 `VALUE` 블록으로 반환하며, 조회가 모두 완료되면 `END`를 반환합니다.

```regex
VALUE <key> <flags> <bytes>\r\n
<value>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 키가 존재할 경우 반환하는 메타데이터와 실제 데이터
  - 조회에 성공한 키의 개수만큼 이 블록이 반복해서 출력
  - 요청한 키가 서버에 존재하지 않거나 만료된 경우, 에러 없이 해당 키의 `VALUE` 블록만 생략
- `END`
  - 데이터 반환의 종료를 의미
  - 요청한 키가 모두 서버에 없을 경우, `END`만 단독으로 반환

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### gets

기본적인 기능은 [**get**](#get) 명령어와 동일하지만, 추후 안전하게 데이터를 저장([**cas**](#cas))하기 위해 고유 식별 번호(`cas unique`)를 함께 받아올 때 사용하는 명령어입니다.

```regex
gets <key>[ <key> ...]\r\n
```

> [!WARNING] ⚠️ 주의 사항
>
> 여러 개의 키를 동시에 조회해야 할 경우, **gets** 명령어보다는 [**mgets**](#mgets) 명령어 사용을 권장합니다.

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
  - 공백(Space)을 구분자로 사용하여 여러 키를 한 번에 전달 가능

**응답(Response)**

서버에 존재하는 데이터는 `VALUE` 블록으로 반환하며, 조회가 모두 완료되면 `END`를 반환합니다.

```regex
VALUE <key> <flags> <bytes> <cas unique>\r\n
<value>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 키가 존재할 경우 반환하는 메타데이터와 실제 데이터
  - 조회에 성공한 키의 개수만큼 이 블록이 반복해서 출력
  - 요청한 키가 서버에 존재하지 않거나 만료된 경우, 에러 없이 해당 키의 `VALUE` 블록만 생략
- `END`
  - 데이터 반환의 종료를 의미
  - 요청한 키가 모두 서버에 없을 경우, `END`만 단독으로 반환

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

---

### mget

한 번에 여러 개의 키를 지정하여 데이터를 조회합니다.

```regex
mget <lenkeys> <numkeys>\r\n
<key> <key> ...\r\n
```

**매개변수(Parameters)**

- `<lenkeys>` (필수) : 전체 키 목록 문자열의 길이 (Byte)
- `<numkeys>` (필수) : 조회할 키의 총 개수
- `<key> ...` (필수) : 공백(Space)을 구분자로 사용하여 나열한 키 목록

**응답(Response)**

서버에 존재하는 데이터는 `VALUE` 블록으로 반환하며, 조회가 모두 완료되면 `END`를 반환합니다.

```regex
VALUE <key> <flags> <bytes>\r\n
<value>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 키가 존재할 경우 반환하는 메타데이터와 실제 데이터
  - 조회에 성공한 키의 개수만큼 이 블록이 반복해서 출력
  - 요청한 키가 서버에 존재하지 않거나 만료된 경우, 에러 없이 해당 키의 `VALUE` 블록만 생략
- `END`
  - 데이터 반환의 종료를 의미
  - 요청한 키가 모두 서버에 없을 경우, `END`만 단독으로 반환

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |
---

### mgets

기본적인 기능은 [**mget**](#mget) 명령어와 동일하지만, 추후 안전하게 데이터를 저장([**cas**](#cas))하기 위해 고유 식별 번호(`cas unique`)를 함께 받아올 때 사용하는 명령어입니다.

```regex
mgets <lenkeys> <numkeys>\r\n
<key> <key> ...\r\n
```

**매개변수(Parameters)**

- `<lenkeys>` (필수) : 전체 키 목록 문자열의 길이 (Byte)
- `<numkeys>` (필수) : 조회할 키의 총 개수
- `<key> ...` (필수) : 공백(Space)을 구분자로 사용하여 나열한 키 목록

**응답(Response)**

서버에 존재하는 데이터는 `VALUE` 블록으로 반환하며, 조회가 모두 완료되면 `END`를 반환합니다.

```regex
VALUE <key> <flags> <bytes> <cas unique>\r\n
<value>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 키가 존재할 경우 반환하는 메타데이터와 실제 데이터
  - 조회에 성공한 키의 개수만큼 이 블록이 반복해서 출력
  - 요청한 키가 서버에 존재하지 않거나 만료된 경우, 에러 없이 해당 키의 `VALUE` 블록만 생략
- `END`
  - 데이터 반환의 종료를 의미
  - 요청한 키가 모두 서버에 없을 경우, `END`만 단독으로 반환

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

### gat

하나 또는 여러 개의 키를 지정하여 데이터를 조회하는 동시에 만료 시간을 변경합니다.

```regex
gat <expiretime> <key> [<key> ...]\r\n
```

**매개변수(Parameters)**

- [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes) (필수)
- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
  - 공백(Space)을 구분자로 사용하여 여러 키를 한 번에 전달 가능

**응답(Response)**

서버에 존재하며 만료 시간이 성공적으로 갱신된 데이터는 `VALUE` 블록으로 반환하며, 조회가 모두 완료되면 `END`를 반환합니다.

```regex
VALUE <key> <flags> <bytes>\r\n
<value>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 키가 존재할 경우 반환하는 메타데이터와 실제 데이터
  - 조회에 성공한 키의 개수만큼 이 블록이 반복해서 출력
  - 요청한 키가 서버에 존재하지 않거나 만료된 경우, 에러 없이 해당 키의 `VALUE` 블록만 생략
- `END`
  - 데이터 반환의 종료를 의미
  - 요청한 키가 모두 서버에 없을 경우, `END`만 단독으로 반환

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

### gats

기본적인 기능은 [**gat**](#gat) 명령어와 동일하지만, 추후 안전하게 데이터를 저장([**cas**](#cas))하기 위해 고유 식별 번호(`cas unique`)를 함께 받아올 때 사용하는 명령어입니다.

```regex
gats <expiretime> <key> [<key> ...]\r\n
```

**매개변수(Parameters)**

- [`<expiretime>`](../../2-핵심%20개념/01-데이터%20모델.md#attributes) (필수)
- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
  - 공백(Space)을 구분자로 사용하여 여러 키를 한 번에 전달 가능

**응답(Response)**

서버에 존재하며 만료 시간이 성공적으로 갱신된 데이터는 `VALUE` 블록으로 반환하며, 조회가 모두 완료되면 `END`를 반환합니다.

```regex
VALUE <key> <flags> <bytes> <cas unique>\r\n
<value>\r\n
...
END\r\n
```

- `VALUE ...`
  - 대상 키가 존재할 경우 반환하는 메타데이터와 실제 데이터
  - 조회에 성공한 키의 개수만큼 이 블록이 반복해서 출력
  - 요청한 키가 서버에 존재하지 않거나 만료된 경우, 에러 없이 해당 키의 `VALUE` 블록만 생략
- `END`
  - 데이터 반환의 종료를 의미
  - 요청한 키가 모두 서버에 없을 경우, `END`만 단독으로 반환

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |

## Deletion 명령

### delete

하나의 키를 지정하여 데이터를 제거합니다.

> [!TIP] 💡 도움말
>
> 이 명령어는 Key-Value뿐만 아니라 모든 컬렉션 타입의 키에 사용할 수 있습니다.

```regex
delete <key> [noreply]\r\n
```

**매개변수(Parameters)**

- [`<key>`](../../2-핵심%20개념/01-데이터%20모델.md#key) (필수)
- `noreply` (선택) : 설정 시 서버 응답을 생략

**응답(Response)**

데이터가 성공적으로 제거되면 `DELETED`를 반환합니다.

```regex
DELETED\r\n
```

단, **예외 및 오류 응답 메시지**는 다음과 같습니다.

| 응답 메시지 | 설명 |
| :--- | :--- |
| **`NOT_FOUND`** | 대상 키가 존재하지 않아 제거 실패 |
| **`CLIENT_ERROR {reason}`** | 사용자의 잘못된 요청. `{reason}`에 구체적인 원인이 포함됨 |
| **`SERVER_ERROR {reason}`** | 서버 내부 오류. `{reason}`에 구체적인 원인이 포함됨 |
