"""VectorDBBench용 ArcVector client.

ArcVector의 ASCII 프로토콜(vcreate/vadd/vsearch)을 VectorDBBench의 VectorDB
인터페이스에 매핑한다. 외부에서 구동 중인 arcus-memcached(+libarcusv) 데몬에
TCP로 접속한다.

이 파일은 VectorDBBench 패키지 내부
  vectordb_bench/backend/clients/arcvector/arcvector.py
위치에 복사되어야 한다.
"""

import logging
import socket
import struct
from contextlib import contextmanager

import numpy as np

from ..api import VectorDB

log = logging.getLogger(__name__)


class _Wire:
    """ArcVector ASCII 프로토콜용 경량 클라이언트(버퍼드 리더)."""

    def __init__(self, host: str, port: int, timeout: float = 60.0):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._buf = b""

    def _readline(self) -> str:
        while b"\r\n" not in self._buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("ArcVector 연결이 끊겼습니다")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\r\n", 1)
        return line.decode(errors="replace")

    def vdrop(self, index: str) -> str:
        self.sock.sendall(f"vdrop {index}\r\n".encode())
        return self._readline()

    def vcreate(self, index: str, dim: int, metric: str, itype: str, max_elements: int) -> str:
        self.sock.sendall(
            f"vcreate {index} {dim} {metric} {itype} {max_elements}\r\n".encode()
        )
        return self._readline()

    def vadd(self, index: str, vec_id: str, blob: bytes) -> str:
        self.sock.sendall(f"vadd {index} {vec_id} {len(blob)}\r\n".encode() + blob + b"\r\n")
        return self._readline()

    def vsearch(self, index: str, k: int, blob: bytes, ef: int | None = None) -> list[str]:
        # threshold 자리에 "-"(임계값 없음) 를 주고 ef 를 6번째 토큰으로 전달한다.
        if ef is not None:
            hdr = f"vsearch {index} {k} {len(blob)} - {ef}\r\n".encode()
        else:
            hdr = f"vsearch {index} {k} {len(blob)}\r\n".encode()
        self.sock.sendall(hdr + blob + b"\r\n")
        ids: list[str] = []
        while True:
            line = self._readline()
            if (line in ("END", "") or line == "ERROR"
                    or line.startswith("CLIENT_ERROR") or line.startswith("SERVER_ERROR")):
                break
            parts = line.split(" ", 2)
            if len(parts) >= 2:
                ids.append(parts[0])
                payload_len = int(parts[2]) if len(parts) > 2 and parts[2].isdigit() else 0
                if payload_len > 0:
                    self._readline()  # payload 줄 소비
        return ids

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class ArcVector(VectorDB):
    def __init__(
        self,
        dim: int,
        db_config: dict,
        db_case_config,
        collection_name: str = "arcvector_bench",
        drop_old: bool = False,
        **kwargs,
    ):
        self.dim = dim
        self.host = db_config["host"]
        self.port = db_config["port"]
        self.index = collection_name
        ip = db_case_config.index_param() if db_case_config else {}
        self.metric = ip.get("metric", "L2")
        self.index_type = ip.get("index_type", "HNSW")
        self.max_elements = ip.get("max_elements", 2_000_000)
        sp = db_case_config.search_param() if db_case_config else {}
        self.ef = sp.get("ef")
        self.conn: _Wire | None = None

        if drop_old:
            w = _Wire(self.host, self.port)
            try:
                w.vdrop(self.index)  # 없으면 무시됨
                r = w.vcreate(self.index, dim, self.metric, self.index_type, self.max_elements)
                if "CREATED" not in r and "exists" not in r:
                    raise RuntimeError(f"vcreate 실패: {r!r}")
                log.info("ArcVector 인덱스 준비: %s (%s, dim=%d, max=%d)",
                         self.index, self.index_type, dim, self.max_elements)
            finally:
                w.close()

    @contextmanager
    def init(self):
        # VectorDBBench는 멀티프로세스로 동작하므로 프로세스마다 새 커넥션을 연다.
        self.conn = _Wire(self.host, self.port)
        try:
            yield
        finally:
            self.conn.close()
            self.conn = None

    def insert_embeddings(self, embeddings, metadata, **kwargs):
        count = 0
        try:
            for vec, mid in zip(embeddings, metadata):
                blob = np.asarray(vec, dtype="<f4").tobytes()
                r = self.conn.vadd(self.index, str(mid), blob)
                if "STORED" not in r:
                    return count, RuntimeError(f"vadd 실패(id={mid}): {r!r}")
                count += 1
            return count, None
        except Exception as e:  # noqa: BLE001
            return count, e

    def search_embedding(self, query, k: int = 100, **kwargs) -> list[int]:
        blob = np.asarray(query, dtype="<f4").tobytes()
        ids = self.conn.vsearch(self.index, k, blob, self.ef)
        out: list[int] = []
        for s in ids:
            try:
                out.append(int(s))
            except ValueError:
                pass
        return out

    def optimize(self, data_size: int | None = None):
        # HNSW는 vadd 시 점진적으로 구축되므로 별도 최적화 단계가 없다.
        pass

    def need_normalize_cosine(self) -> bool:
        # COSINE은 ArcVector가 내부에서 정규화하므로 사전 정규화가 필요 없다.
        return False
