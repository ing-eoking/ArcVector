# Runs the whole suite — unit and integration — against a real arcus daemon.
#
#   docker build -f docker/Dockerfile -t arcvector-test .
#   docker run --rm arcvector-test
#
# Two reasons this exists alongside the local path:
#
#   1. Reproducibility. The local harness needs a memcached binary and
#      default_engine.so that this crate does not build, so on a fresh machine or
#      in CI the integration tests would simply skip.
#   2. It tests the artifact that actually ships. Development on macOS produces a
#      .dylib; deployment is a Linux .so. Only this path exercises that.

# --- arcus -----------------------------------------------------------------
# The published image already carries a built daemon under /arcus and loads
# extensions with -X, which is the mechanism this module relies on.
FROM jam2in/arcus-memcached:latest AS arcus

# --- toolchain + tests -----------------------------------------------------
FROM rust:1-slim AS test

# clang and libclang for bindgen; g++ because usearch compiles a C++ core.
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang libclang-dev g++ \
 && rm -rf /var/lib/apt/lists/*

COPY --from=arcus /arcus /arcus
ENV ARCVECTOR_MEMCACHED=/arcus/bin/memcached \
    ARCVECTOR_ENGINE=/arcus/lib/default_engine.so

WORKDIR /src

# Dependencies first, so editing sources does not rebuild usearch's C++ core.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "" > src/lib.rs && cargo fetch

COPY build.rs ./
COPY include ./include
COPY src ./src
COPY tests ./tests

# cargo test does not build the cdylib the daemon loads, and the harness refuses
# to run against a stale one — so build it explicitly first.
CMD ["sh", "-c", "cargo build && cargo test"]
