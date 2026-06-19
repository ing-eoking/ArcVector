"""
ArcVector 벤치마크 하니스.

vadd / vsearch 의 처리량(ops/sec)과 지연(p50/p95/p99)을 측정하고,
- 엔진(Map) 메모리: memcached `stats` 의 bytes / curr_items
- 모듈 메모리(HNSW + 벡터 HashMap): memcached 프로세스 RSS(ps)
증가량을 함께 보고한다.

vadd/vsearch 는 커스텀 바이너리 nread 프로토콜이라 표준 부하 툴을 못 쓰므로
arcus_vector.py 클라이언트를 그대로 재사용한다.

사용법:
  python bench.py --dim 128 --n 10000 --queries 2000 --k 10 --type HNSW
  python bench.py --type both --threads 4          # FLAT/HNSW 비교 + 4 커넥션 동시

주의:
  - 이 수치는 "Python 클라이언트 경유 end-to-end" 기준이다(엔진 최대 처리량 ≠).
    엔진을 더 압박하려면 --threads 로 커넥션 수를 늘려라.
  - 측정 전 데몬이 떠 있어야 한다:
      memcached -E default_engine.so -X <repo>/target/release/libarcusv.dylib -p 11211 -m 4096
"""

import argparse
import os
import random
import socket
import struct
import subprocess
import threading
import time

from arcus_vector import ArcusVector


# ---------- 보조 유틸 ----------

def gen_vector(dim):
    return [random.gauss(0.0, 1.0) for _ in range(dim)]


def percentile(sorted_lat, p):
    if not sorted_lat:
        return 0.0
    idx = min(len(sorted_lat) - 1, int(p / 100.0 * len(sorted_lat)))
    return sorted_lat[idx]


def summarize(name, latencies_ms, wall_s):
    n = len(latencies_ms)
    if n == 0:
        print(f"  [{name}] 실행된 연산 없음")
        return
    s = sorted(latencies_ms)
    ops = n / wall_s if wall_s > 0 else 0.0
    print(f"  [{name}] {n}건 / {wall_s:.2f}s  →  {ops:,.0f} ops/sec")
    print(f"        지연(ms)  p50={percentile(s,50):.3f}  "
          f"p95={percentile(s,95):.3f}  p99={percentile(s,99):.3f}  "
          f"max={s[-1]:.3f}")


# ---------- 메모리 측정 ----------

def find_daemon_pid(port):
    """포트를 LISTEN 중인 memcached PID 를 찾는다(macOS/Linux 공통: lsof)."""
    try:
        out = subprocess.check_output(
            ["lsof", "-ti", f"tcp:{port}", "-sTCP:LISTEN"],
            stderr=subprocess.DEVNULL,
        ).decode().split()
        return int(out[0]) if out else None
    except (subprocess.CalledProcessError, FileNotFoundError, ValueError):
        return None


def rss_kb(pid):
    """프로세스 RSS(KB). macOS/Linux 의 ps 는 RSS 를 KB 로 보고한다."""
    if pid is None:
        return None
    try:
        out = subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(pid)], stderr=subprocess.DEVNULL
        ).decode().strip()
        return int(out) if out else None
    except (subprocess.CalledProcessError, ValueError):
        return None


def engine_stats(host, port):
    """memcached `stats` 의 bytes / curr_items 를 읽는다(엔진 Map 메모리)."""
    try:
        s = socket.create_connection((host, port), timeout=5)
        s.sendall(b"stats\r\n")
        buf = b""
        while b"END\r\n" not in buf:
            chunk = s.recv(4096)
            if not chunk:
                break
            buf += chunk
        s.close()
    except OSError:
        return {}
    stats = {}
    for line in buf.decode(errors="replace").split("\r\n"):
        parts = line.split(" ")
        if len(parts) == 3 and parts[0] == "STAT":
            stats[parts[1]] = parts[2]
    return stats


# ---------- 벤치 본체 ----------

def raw_cmd(host, port, line):
    """단일 라인 명령(vdrop 등)을 보내고 한 줄 응답을 받는다."""
    s = socket.create_connection((host, port), timeout=5)
    s.sendall((line + "\r\n").encode())
    buf = b""
    while not buf.endswith(b"\r\n"):
        b1 = s.recv(1)
        if not b1:
            break
        buf += b1
    s.close()
    return buf.decode(errors="replace").strip()


def run_worker(host, port, index, op, items, k, threshold, out_latencies):
    """워커 1개(= 커넥션 1개)가 자기 몫의 연산을 수행하고 지연을 모은다."""
    cli = ArcusVector(host, port)
    lat = []
    for vec_id, vector in items:
        t0 = time.perf_counter()
        if op == "vadd":
            cli.vadd(index, vec_id, vector, payload=f"p:{vec_id}")
        else:  # vsearch
            cli.vsearch(index, k, vector, threshold=threshold)
        lat.append((time.perf_counter() - t0) * 1000.0)
    cli.close()
    out_latencies.extend(lat)


def parallel_run(host, port, index, op, work, k, threshold, threads):
    """work(=[(id, vec), ...]) 를 threads 개로 쪼개 동시 실행, (지연리스트, wall초) 반환."""
    chunks = [work[i::threads] for i in range(threads)]
    results = [[] for _ in range(threads)]
    th = [
        threading.Thread(
            target=run_worker,
            args=(host, port, index, op, chunks[i], k, threshold, results[i]),
        )
        for i in range(threads)
    ]
    t0 = time.perf_counter()
    for t in th:
        t.start()
    for t in th:
        t.join()
    wall = time.perf_counter() - t0
    latencies = [x for r in results for x in r]
    return latencies, wall


def bench_index(args, index_type):
    host, port = args.host, args.port
    index = f"{args.index}_{index_type.lower()}"
    pid = find_daemon_pid(port)

    print(f"\n===== {index_type} 인덱스 벤치 (dim={args.dim}, n={args.n}, "
          f"queries={args.queries}, k={args.k}, threads={args.threads}) =====")
    print(f"  대상 memcached PID = {pid if pid else '?(lsof 실패 — RSS 측정 생략)'}")

    # 0) 깨끗한 인덱스 준비
    raw_cmd(host, port, f"vdrop {index}")  # 없으면 무시됨
    cli = ArcusVector(host, port)
    resp = cli.vcreate(index, args.dim, args.metric, index_type, args.n + 1000)
    cli.close()
    if "CREATED" not in resp and "exists" not in resp:
        print(f"  vcreate 실패: {resp!r}")
        return

    rss0 = rss_kb(pid)
    st0 = engine_stats(host, port)

    # 1) vadd 단계: 미리 데이터 생성(생성 시간은 측정에서 제외)
    add_work = [(f"v{i}", gen_vector(args.dim)) for i in range(args.n)]
    add_lat, add_wall = parallel_run(host, port, index, "vadd",
                                     add_work, args.k, None, args.threads)
    summarize("vadd", add_lat, add_wall)

    rss1 = rss_kb(pid)
    st1 = engine_stats(host, port)

    # 2) vsearch 단계: 질의 벡터 미리 생성
    q_work = [(f"q{i}", gen_vector(args.dim)) for i in range(args.queries)]
    srch_lat, srch_wall = parallel_run(host, port, index, "vsearch",
                                       q_work, args.k, args.threshold, args.threads)
    summarize("vsearch", srch_lat, srch_wall)

    # 3) 메모리 보고
    print("  --- 메모리 ---")
    if rss0 is not None and rss1 is not None:
        delta = rss1 - rss0
        per = (delta * 1024.0 / args.n) if args.n else 0.0
        print(f"  모듈 측 RSS: {rss0/1024:.1f}MB → {rss1/1024:.1f}MB "
              f"(Δ {delta/1024:.1f}MB, 벡터당 ~{per:,.0f} B)")
    else:
        print("  모듈 측 RSS: 측정 불가(PID 미확인). lsof 권한/경로 확인.")
    eb0, eb1 = int(st0.get("bytes", 0)), int(st1.get("bytes", 0))
    ci0, ci1 = int(st0.get("curr_items", 0)), int(st1.get("curr_items", 0))
    print(f"  엔진 Map 메모리(stats bytes): {eb0/1024/1024:.1f}MB → "
          f"{eb1/1024/1024:.1f}MB  (curr_items {ci0} → {ci1})")


def main():
    ap = argparse.ArgumentParser(description="ArcVector vadd/vsearch 벤치마크")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=11211)
    ap.add_argument("--index", default="bench")
    ap.add_argument("--dim", type=int, default=128)
    ap.add_argument("--n", type=int, default=10000, help="삽입할 벡터 수")
    ap.add_argument("--queries", type=int, default=2000, help="검색 질의 수")
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--metric", default="COSINE", choices=["L2", "COSINE", "IP"])
    ap.add_argument("--type", default="HNSW", choices=["FLAT", "HNSW", "both"])
    ap.add_argument("--threads", type=int, default=1, help="동시 커넥션 수")
    ap.add_argument("--threshold", type=float, default=None)
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    random.seed(args.seed)

    # 연결 가능 여부 사전 점검
    try:
        socket.create_connection((args.host, args.port), timeout=3).close()
    except OSError as e:
        print(f"[에러] {args.host}:{args.port} 연결 실패: {e}")
        print("데몬을 먼저 띄우세요. 예:")
        print("  memcached -E default_engine.so \\")
        print(f"            -X {os.path.abspath('../target/release/libarcusv.dylib')} \\")
        print("            -p 11211 -m 4096")
        return

    types = ["FLAT", "HNSW"] if args.type == "both" else [args.type]
    for t in types:
        bench_index(args, t)


if __name__ == "__main__":
    main()
