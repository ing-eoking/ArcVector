# ArcVector × VectorDBBench 테스트 시나리오

[VectorDBBench](https://github.com/zilliztech/VectorDBBench)로 ArcVector의
**삽입 처리량 · 검색 QPS · 지연(p99) · recall · 적재 시간**을 표준 데이터셋으로
측정한다. ArcVector의 `vadd`/`vsearch`는 커스텀 프로토콜이라, 이 디렉터리의
client 어댑터(`arcvector/`)를 VectorDBBench에 등록해서 사용한다.

> 측정은 **대상 장비**에서 수행한다. 아래는 그 장비에서의 절차다.

---

## 0. 사전 준비 (대상 장비)

- Python **3.11+** (VectorDBBench 요구사항)
- ArcVector 빌드 산출물: `cargo build --release` → `target/release/libarcusv.so`(Linux) / `.dylib`(macOS)
- arcus-memcached 실행 바이너리 + `default_engine.so` (저장소 루트에 포함)
- 데이터셋 다운로드용 네트워크(데이터셋은 S3에서 자동 다운로드, 수백 MB~수 GB)

---

## 1. ArcVector 데몬 기동

```bash
# 메모리(-m)는 데이터셋이 충분히 들어가도록 넉넉히. 포트는 11211 예시.
./memcached -E ./default_engine.so \
            -X ./target/release/libarcusv.so \
            -p 11211 -m 8192
```

> ⚠️ `max_element_bytes`(기본 16KB)보다 큰 벡터는 거부된다. 고차원(예: 1536D = 6144B)은
> 기본값으로 충분하지만, 더 큰 차원·payload를 쓸 경우 엔진 옵션으로 늘려야 한다.

---

## 2. VectorDBBench 설치 + ArcVector client 등록

```bash
git clone https://github.com/zilliztech/VectorDBBench.git
cd VectorDBBench

# (1) 어댑터 폴더 복사
cp -r /path/to/ArcVector/bench/vectordbbench/arcvector \
      vectordb_bench/backend/clients/arcvector

# (2) 개발 모드 설치
pip install -e '.[test]'
```

### (3) `vectordb_bench/backend/clients/__init__.py` 에 3곳 등록

**DB enum에 멤버 추가:**
```python
class DB(Enum):
    ...
    ArcVector = "ArcVector"
```

**`init_cls` 프로퍼티에 분기 추가:**
```python
    @property
    def init_cls(self):
        ...
        if self == DB.ArcVector:
            from .arcvector.arcvector import ArcVector
            return ArcVector
```

**`config_cls` 프로퍼티에 분기 추가:**
```python
    @property
    def config_cls(self):
        ...
        if self == DB.ArcVector:
            from .arcvector.config import ArcVectorConfig
            return ArcVectorConfig
```

**`case_config_cls` 프로퍼티(메서드)에 분기 추가:**
```python
    def case_config_cls(self, index_type=None):
        ...
        if self == DB.ArcVector:
            from .arcvector.config import ArcVectorIndexConfig
            return ArcVectorIndexConfig
```

---

## 3. 실행 (Streamlit UI)

```bash
init_bench          # 또는: python -m vectordb_bench
```

브라우저 UI에서:

1. DB 목록에서 **ArcVector** 선택
2. 접속 정보 입력: `host=127.0.0.1`, `port=11211`
3. 인덱스 설정: `index_type=HNSW`(또는 FLAT), **`max_elements` ≥ 데이터셋 벡터 수**,
   **`ef_search`**(HNSW 탐색 폭 — recall↔QPS 트레이드오프 노브)
4. **Case** 선택: 처음엔 작은 *Search Performance* 케이스로 파이프라인 검증
   (예: 50K~100K 규모) → 정상 동작 확인 후 대형(1M+)으로 확장
5. Run → 결과에서 **QPS · p99 지연 · recall · load duration** 확인

결과 원본은 `vectordb_bench/results/` 에 JSON으로도 저장된다.

---

## 3.5 recall ↔ QPS 곡선 그리기 (UI)

ArcVector는 이제 `vsearch`의 `ef` 인자를 지원하므로, `ef_search`를 바꿔가며
여러 번 실행하면 각 실행이 **(recall, QPS) 한 점**이 되고, 이를 모으면 곡선이 된다.

1. 같은 데이터셋/케이스로 `ef_search`만 바꿔 반복 실행:
   예) `16 → 32 → 64 → 128 → 256 → 512`
   (작은 ef = 빠르고 낮은 recall, 큰 ef = 느리고 높은 recall)
2. UI의 **Results** 페이지에서 여러 실행 결과를 함께 선택하면 QPS·recall·지연을
   나란히 비교할 수 있다. 각 점을 이으면 ArcVector의 recall↔QPS 곡선이다.
3. `index_type=FLAT`(완전탐색)으로 한 번 돌려 **recall 상한(=1.0)·기준 지연**을 같이 두면
   HNSW 근사 품질을 가늠하기 좋다(FLAT은 `ef` 무시).

> 삽입(vadd)은 `ef`와 무관하므로, 한 번 적재한 인덱스를 두고 검색만 반복하려면
> 두 번째 실행부터는 적재를 건너뛰는 옵션(case의 load 비활성/기존 인덱스 재사용)을 쓰면
> 빠르게 ef sweep만 돌릴 수 있다.

---

## 4. 측정되는 지표

| 지표 | 의미 |
|---|---|
| Load duration | 전체 벡터 삽입(vadd) 소요 시간 → 삽입 처리량 |
| QPS | 초당 검색(vsearch) 처리량 (멀티프로세스 동시) |
| Serial / p99 latency | 검색 지연 |
| Recall | HNSW 근사 검색 정확도 (ground truth 대비) |

메모리(모듈 측 HNSW + 벡터 HashMap)는 VectorDBBench가 측정하지 않으므로,
실행 중 별도로 데몬 프로세스 RSS를 샘플링한다(이 저장소 `rag/bench.py`의 방식 참고):
```bash
# Linux
while true; do grep VmRSS /proc/$(pgrep -f libarcusv)/status; sleep 2; done
# macOS
while true; do ps -o rss= -p $(lsof -ti tcp:11211 -sTCP:LISTEN); sleep 2; done
```

---

## 5. 주의점 / 한계

- **`max_elements`는 데이터셋 크기 이상**으로. 부족하면 인덱스 생성/삽입이 실패한다.
- **`ef_search`는 이제 조정 가능**(UI 필드 → `vsearch`의 `ef` 인자). 단 그래프 빌드
  파라미터 `M=16`, `ef_construction=200`은 여전히 코드 고정(`lib.rs`의 `HnswWrapper::new`)
  이라, 이들을 바꾼 비교가 필요하면 재빌드해야 한다.
- **COSINE**은 ArcVector가 내부 정규화하므로 `need_normalize_cosine=False`로 두었다.
  데이터셋의 거리 종류(angular→COSINE, euclidean→L2)에 맞춰 메트릭을 선택한다.
- 메모리 부족 시 ArcVector는 데몬을 죽이지 않고 `SERVER_ERROR`를 반환하므로,
  VectorDBBench에는 **삽입 에러**로 보고된다(크래시 아님). max 설정/`-m`을 늘려라.
- 인터페이스 시그니처는 VectorDBBench 버전에 따라 약간 다를 수 있다. 어댑터는
  `**kwargs`로 방어했으나, 설치한 버전의 `clients/api.py`와 대조해 확인할 것.
