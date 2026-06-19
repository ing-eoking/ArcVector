# 로깅 설정 (Syslog & Userlog Extension)

## 기본 로그

별도의 로깅 설정을 지정하지 않으면 `stderr`로 로그를 기록합니다.

`stderr`는 별도의 파일이나 데몬 설정 없이 터미널 또는 프로세스의 표준 에러 출력으로 로그가 직접 출력되는 방식입니다. 주로 개발 환경이나 디버깅 용도로 사용되며, 로그 파일 관리나 순환 기능은 제공되지 않습니다.

## Syslog Logger Extension

> [!TIP] 💡 도움말
>
> Syslog Logger는 [구동 옵션](../0-서버%20환경%20설정/01-구동%20옵션.md)에서 확장 모듈 인자로 `syslog_logger` 모듈이 지정되어야 사용할 수 있습니다.


Syslog Logger Extension은 Linux/Unix 시스템의 syslog를 통해 로그를 기록하는 확장 모듈입니다.

### 로그 저장 및 관리 방식

로그는 시스템의 syslog 데몬을 통해 관리됩니다. 저장 경로, 파일 순환 정책, 보관 기간 등은 시스템의 syslog 설정에 따르며, 별도의 로그 파일을 직접 관리할 필요가 없습니다.

일반적으로 `/var/log/syslog` 또는 `/var/log/messages` 등의 경로에 기록되며, rsyslog나 syslog-ng 등의 설정을 통해 로그 출력 경로나 필터링 규칙을 변경할 수 있습니다.

상세한 가이드는 [syslog](https://docs.rsyslog.com/doc/index.html) 문서를 확인하시길 바랍니다.

## Userlog Logger Extension

> [!TIP] 💡 도움말
>
> Userlog Logger는 [구동 옵션](../0-서버%20환경%20설정/01-구동%20옵션.md)에서 확장 모듈 인자로 `userlog_logger` 모듈이 지정되어야 사용할 수 있습니다.

Userlog Logger Extension은 루트 권한 없이도 접근 가능한 파일 기반 로그 기록을 지원하는 확장 모듈입니다.

### 로그 저장 및 관리 방식

로그 파일은 memcached를 실행한 위치를 기준으로 자동 생성되는 **ARCUSlog** 디렉토리에 저장됩니다.

#### 로그 파일 이름 형식

```regex
arcus<index>_<YYYY><MM><DD>
```

- `<index>` : 0~4 사이 값을 가지는 로그 파일 인덱스
- `<YYYY>` : 파일 생성 년도
- `<MM>` : 파일 생성 월
- `<DD>` : 파일 생성 일

#### 로그 파일 순환

최대 5개(0~4)까지 로그 파일을 유지하며, 각 파일은 최대 20MB까지 저장합니다.
모든 파일이 용량 한도에 도달하면, 가장 오래된 파일을 삭제하고 새 로그 파일을 생성합니다.

#### 로그 동작 설정

Userlog Logger는 다음 환경 변수를 통해 로그 출력 동작을 제어할 수 있습니다.

**UserLogRateLimitInterval**

일정 시간 동안 기록되는 로그 개수를 제한하는 간격(초)입니다.
이 값을 0으로 설정하면 로그 제한 기능이 비활성화되어, 개수 제한 없이 로그가 기록됩니다.

- 기본값 : 5
- 최대값 : 60

```bash
export UserLogRateLimitInterval=10
```

**UserLogRateLimitBurst**

`UserLogRateLimitInterval` 내에 허용되는 최대 로그 개수입니다.

- 기본값 : 200
- 최대값 : 50,000

```bash
export UserLogRateLimitBurst=1000
```

**UserLogReduction**

동일한 로그 메시지가 반복될 경우 하나로 압축해 출력하는 기능입니다.
연속된 동일 메시지는 1회만 출력하며, 반복 종료 시 반복 횟수를 함께 출력합니다.

- 기본값 : on

```bash
export UserLogReduction=on
```
