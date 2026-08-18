# Running the tests

```sh
make test
```

That is the whole thing: it builds the image and runs **all** the tests, unit and
integration, in the container.

## Why the container

The integration tests need a memcached binary and a `default_engine.so`, which
this crate does not build. The container supplies them, so the suite does not
depend on whatever happens to be lying in the working tree.

It also builds the artifact that ships. Development on macOS produces a `.dylib`;
deployment is a Linux `.so`, and this is the only place that one runs. The
generated bindings are not even the same size on the two platforms — bindgen reads
the target's own system headers — so "it works locally" is not the same claim.

The image takes the server from `jam2in/arcus-memcached:latest` and builds the
module with the Rust toolchain in the same layer, so the tests spawn the server as
a local subprocess exactly as they would on a workstation. There is no separate
server service; Docker only supplies the binaries and the platform.

If that image is unavailable, build one from an arcus checkout — its `Dockerfile`
installs into `/arcus`, the layout expected here:

```sh
docker build -t arcus-local /path/to/arcus-memcached
# then change the first FROM in docker/Dockerfile to arcus-local
```

## On the host

```sh
make unit     # unit tests only; no server involved
make lint     # fmt --check and clippy -D warnings
```

The integration target sits behind the `integration` feature, so a plain
`cargo test` does not build it and reports nothing about it. That is deliberate:
the alternative was listing tests that never ran, which this suite has already
mistaken for success twice.

To run them on the host, point the harness at an arcus build and ask for the
feature:

```sh
export ARCVECTOR_MEMCACHED=/path/to/arcus/bin/memcached
export ARCVECTOR_ENGINE=/path/to/arcus/lib/default_engine.so
cargo build && cargo test --features integration
```

Asking for the feature asserts that a server is reachable, so an unset variable
fails rather than skipping. There is no skip path left.

**`cargo build` first.** `cargo test` builds the rlib the harness links against
but *not* the `cdylib` the server loads; the harness compares timestamps and fails
rather than verifying a stale library.

## How the harness works

Each test starts its own server on its own **unix socket** and kills it on drop.
Sockets rather than TCP ports: tests run in parallel, and allocating a free port
before spawning leaves a window for another test to take it — which showed up as
connections being refused partway through the suite.

`Client` reads until a reply is terminated, so no test sleeps or races. Lengths
that the protocol requires (`veclen`, `attrlen`, filter term counts) are derived
from what is actually sent by `Client::vadd` and friends, because a test that
miscounts tests the wrong thing. The tests that *want* a wrong length call
`send_body` directly and say so.

## Skip versus fail

A **missing** server or engine skips: this crate cannot build them. Anything that
is present but does not work **fails**, with the server's own output attached.
`Server::start` checks two things beyond "the socket accepted a connection":

- the process did not exit, and
- `vlist` answers `END`, which only happens if our extension registered.

That second check exists because it was needed. The first version of this image
shipped a library with no `memcached_extensions_initialize` at all — the
dependency-caching layer left a placeholder artifact in place — and the server
refused to start. Every test skipped and the suite reported 18 passed. A setup
that cannot answer is a broken run, not an absent one.

## Status

Verified. `docker build -f docker/Dockerfile -t arcvector-test . && docker run
--rm arcvector-test` runs 111 unit and 18 integration tests, all passing, against
a Linux `.so` — the artifact that ships.
