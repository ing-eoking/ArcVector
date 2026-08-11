# Running the tests against a real daemon

The integration tests in `tests/` need a memcached binary and a
`default_engine.so`, which this crate does not build. Without them they **skip**
— the suite stays green but `store.rs`, the engine call path, goes unverified.

There are two ways to supply them.

## Locally (fast loop)

Point the harness at an arcus build:

```sh
export ARCVECTOR_MEMCACHED=/path/to/arcus/bin/memcached
export ARCVECTOR_ENGINE=/path/to/arcus/lib/default_engine.so
# Only if you run as root: memcached refuses to without -u.
export ARCVECTOR_MEMCACHED_ARGS="-u root"

cargo build && cargo test
```

The defaults are `./memcached` and `./default_engine.so` at the crate root, so
dropping a build there needs no environment at all.

**`cargo build` first, every time.** `cargo test` builds the rlib the harness
links against but *not* the `cdylib` the daemon loads. The harness compares
timestamps and fails loudly rather than silently verifying a stale library.

## In a container (reproducible, and the real target)

```sh
docker build -f docker/Dockerfile -t arcvector-test .
docker run --rm arcvector-test
```

Worth doing even when the local path works, because development on macOS
produces a `.dylib` while deployment is a Linux `.so`. This is the only place
that artifact gets exercised.

The image takes the daemon from `jam2in/arcus-memcached:latest` and builds the
module with the Rust toolchain in the same layer, so the tests spawn the daemon
locally exactly as they do on a workstation.

If that image is unavailable, build one from an arcus checkout — its `Dockerfile`
installs into `/arcus`, which is the layout expected here:

```sh
docker build -t arcus-local /path/to/arcus-memcached
# then change the first FROM in docker/Dockerfile to arcus-local
```

## How the harness works

Each test starts its own daemon on its own **unix socket** and kills it on drop.
Sockets rather than TCP ports: tests run in parallel, and allocating a free port
before spawning leaves a window for another test to take it — which showed up as
connections being refused partway through the suite.

`Client` reads until a reply is terminated, so no test sleeps or races. Lengths
that the protocol requires (`veclen`, `attrlen`, filter term counts) are derived
from what is actually sent by `Client::vadd` and friends, because a test that
miscounts tests the wrong thing. The tests that *want* a wrong length call
`send_body` directly and say so.

## Skip versus fail

A **missing** daemon or engine skips: this crate cannot build them. Anything that
is present but does not work **fails**, with the daemon's own output attached.
`Daemon::start` checks two things beyond "the socket accepted a connection":

- the process did not exit, and
- `vlist` answers `END`, which only happens if our extension registered.

That second check exists because it was needed. The first version of this image
shipped a library with no `memcached_extensions_initialize` at all — the
dependency-caching layer left a placeholder artifact in place — and the daemon
refused to start. Every test skipped and the suite reported 18 passed. A setup
that cannot answer is a broken run, not an absent one.

## Status

Verified. `docker build -f docker/Dockerfile -t arcvector-test . && docker run
--rm arcvector-test` runs 111 unit and 18 integration tests, all passing, against
a Linux `.so` — the artifact that ships.
