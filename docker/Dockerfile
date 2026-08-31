# Runs the whole suite — unit and integration — against a real arcus server.
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
# The published image already carries a built server under /arcus and loads
# extensions with -X, which is the mechanism this module relies on.
FROM jam2in/arcus-memcached:latest AS arcus

# --- toolchain + tests -----------------------------------------------------
FROM rust:1-slim AS test

# g++ because usearch compiles a C++ core. No clang: the engine bindings are
# committed under bindings/ rather than translated here, so bindgen -- and the
# libclang it looks for -- is not in the build at all.
RUN apt-get update \
 && apt-get install -y --no-install-recommends g++ \
 && rm -rf /var/lib/apt/lists/*

COPY --from=arcus /arcus /arcus

# ARCVECTOR_MEMCACHED_ARGS carries "-u root" because memcached refuses to run as
# root without it, and this image is root. The harness does not guess at a uid.
ENV ARCVECTOR_MEMCACHED=/arcus/bin/memcached \
    ARCVECTOR_ENGINE=/arcus/lib/default_engine.so \
    ARCVECTOR_MEMCACHED_ARGS="-u root"

WORKDIR /src

# Compile the dependencies against a placeholder crate first, so editing sources
# does not recompile usearch's C++ core. build.rs and the headers come along
# because bindgen runs during this build too.
COPY Cargo.toml Cargo.lock build.rs ./
COPY include ./include
RUN mkdir src && echo "" > src/lib.rs && cargo build --lib

COPY src ./src
COPY tests ./tests

# `cargo clean -p` is not optional. COPY stamps files with their mtime from the
# build context, which is *older* than the placeholder build above, so cargo sees
# the real sources as unchanged and skips them — shipping a library with no
# extension entry point at all. Dropping just this crate's artifacts forces the
# rebuild while leaving the dependency cache, and usearch's C++ core, intact.
#
# cargo test would not rebuild the cdylib either: it builds the rlib the harness
# links against, not the library the server loads.
RUN cargo clean -p arcusv \
 && cargo build \
 && cargo build --features integration --tests

# The integration target is behind a feature because it needs a server; this is
# the environment that has one.
CMD ["cargo", "test", "--features", "integration"]
