"""VectorDBBench용 ArcVector 설정 클래스.

이 파일은 VectorDBBench 패키지 내부
  vectordb_bench/backend/clients/arcvector/config.py
위치에 복사되어야 상대 임포트(`..api`)가 동작한다.
"""

from ..api import DBCaseConfig, DBConfig, MetricType


class ArcVectorConfig(DBConfig):
    """접속 정보(데몬 host/port). UI에서 입력 필드로 노출된다."""

    host: str = "127.0.0.1"
    port: int = 11211

    def to_dict(self) -> dict:
        return {"host": self.host, "port": self.port}


_METRIC_MAP = {
    MetricType.L2: "L2",
    MetricType.COSINE: "COSINE",
    MetricType.IP: "IP",
}


class ArcVectorIndexConfig(DBCaseConfig):
    """인덱스 파라미터. UI에서 index_type / max_elements 조정 가능."""

    metric_type: MetricType | None = None
    index_type: str = "HNSW"          # "HNSW" 또는 "FLAT"
    max_elements: int = 2_000_000     # ⚠️ 반드시 데이터셋 벡터 수 이상으로 설정
    ef_search: int = 100              # HNSW 탐색 폭. ↑recall ↓QPS — 이 값을 바꿔가며 곡선을 그린다.

    def parse_metric(self) -> str:
        return _METRIC_MAP.get(self.metric_type, "L2")

    def index_param(self) -> dict:
        return {
            "metric": self.parse_metric(),
            "index_type": self.index_type,
            "max_elements": self.max_elements,
        }

    def search_param(self) -> dict:
        # FLAT(완전탐색)에는 무시되고 HNSW에만 적용된다.
        return {"ef": self.ef_search}
