#!/usr/bin/env bash
#
# ArcVector 벤치 환경 셋업 (AWS RockyLinux 9)
#
#   1) 빌드 패키지(dnf)  2) Rust(rustup)
#   3) arcus-memcached(호스트 데몬) 빌드·설치
#   4) libarcusv(ArcVector 모듈) 빌드
#
# 사용법:
#   ./bench/setup_rocky.sh
#
# 경로는 환경변수로 덮어쓸 수 있다:
#   ARCVECTOR_SRC   ArcVector 소스 루트         (기본: 이 스크립트의 상위 디렉터리)
#   ARCUS_SRC       arcus-memcached 소스 루트    (기본: ArcVector 의 형제 디렉터리)
#   ARCUS_HOME      arcus 설치 경로(prefix)      (기본: $HOME/arcus)
#   ARCUS_REPO      소스가 없을 때 clone 할 URL  (기본: naver/arcus-memcached)
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ARCVECTOR_SRC="${ARCVECTOR_SRC:-$(cd "$SCRIPT_DIR/.." && pwd)}"
ARCUS_SRC="${ARCUS_SRC:-$(cd "$ARCVECTOR_SRC/.." && pwd)/arcus-memcached}"
ARCUS_HOME="${ARCUS_HOME:-$HOME/arcus}"
ARCUS_REPO="${ARCUS_REPO:-https://github.com/naver/arcus-memcached.git}"

log()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m[주의] %s\033[0m\n' "$*"; }

log "설정"
echo "  ARCVECTOR_SRC = $ARCVECTOR_SRC"
echo "  ARCUS_SRC     = $ARCUS_SRC"
echo "  ARCUS_HOME    = $ARCUS_HOME"

# --- 1. 빌드 패키지 -----------------------------------------------------------
log "1/4 패키지 설치 (dnf)"
sudo dnf groupinstall -y "Development Tools"
sudo dnf install -y which clang clang-devel git nmap-ncat \
                    python3.11 python3.11-devel python3.11-pip

# --- 2. Rust ------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    log "2/4 Rust 설치 (rustup)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
else
    log "2/4 Rust 이미 설치됨 — 건너뜀"
fi
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

# bindgen 이 libclang.so 를 못 찾을 때를 대비해 경로를 잡아준다.
if [ -z "${LIBCLANG_PATH:-}" ]; then
    lc="$(find /usr/lib64 /usr/lib -maxdepth 2 -name 'libclang.so*' 2>/dev/null | head -1)"
    [ -n "$lc" ] && export LIBCLANG_PATH="$(dirname "$lc")"
fi

# --- 3. arcus-memcached (호스트 데몬) ----------------------------------------
log "3/4 arcus-memcached 빌드 → $ARCUS_HOME"
if [ ! -d "$ARCUS_SRC" ]; then
    warn "arcus-memcached 소스가 없어 clone 합니다: $ARCUS_REPO"
    warn "ArcVector(src/c/engine.h)와 엔진 API ABI가 맞는 버전인지 확인하세요."
    git clone "$ARCUS_REPO" "$ARCUS_SRC"
fi
cd "$ARCUS_SRC"
[ -x ./configure ] || ./config/autorun.sh
./deps/install.sh "$ARCUS_HOME"                                   # libevent / zookeeper-c / cyrus-sasl
./configure --prefix="$ARCUS_HOME" --with-libevent="$ARCUS_HOME"
make -j"$(nproc)"
make install

# --- 4. libarcusv (ArcVector 모듈) -------------------------------------------
log "4/4 libarcusv 빌드"
cd "$ARCVECTOR_SRC"
cargo build --release

MODULE="$ARCVECTOR_SRC/target/release/libarcusv.so"
[ -f "$MODULE" ] || { warn "모듈 빌드 산출물을 찾을 수 없습니다: $MODULE"; exit 1; }

# --- 안내 --------------------------------------------------------------------
log "완료!"
cat <<EOF

[빌드 산출물]
  데몬 : $ARCUS_HOME/bin/memcached
  엔진 : $ARCUS_HOME/lib/default_engine.so
  모듈 : $MODULE

[데몬 기동]  (-d 는 백그라운드. 처음엔 빼고 로그를 보는 것을 권장)
  $ARCUS_HOME/bin/memcached \\
    -E $ARCUS_HOME/lib/default_engine.so \\
    -X $MODULE \\
    -p 11211 -m 8192 -d

[기동 확인]
  printf 'version\\r\\n' | nc localhost 11211

[1차 검증 / 벤치]
  cd $ARCVECTOR_SRC/rag && python3.11 bench.py --dim 64 --n 2000 --queries 500 --type both

[VectorDBBench]
  $ARCVECTOR_SRC/bench/vectordbbench/SCENARIO.md 참고

[보안] 11211 은 외부 노출 금지(무인증·DDoS 증폭 표적). VectorDBBench·UI는 SSH 터널로.
EOF
