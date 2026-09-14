# The whole suite runs in a container, because the integration tests need an
# arcus server that this crate does not build — and because the container builds
# the Linux .so that actually ships, not the .dylib a macOS workstation produces.
#
#   make test          everything, in the container
#   make unit          unit tests only, on the host (no server needed)
#   make lint          format check and clippy, on the host
#   make shell         a shell in the test image, for poking around
#   make sync-headers  follow a server tree: copy its config and headers, retranslate
#   make bindings      retranslate include/ (needs libclang)

IMAGE ?= arcvector-test
DOCKERFILE := docker/Dockerfile

# Everything but regen-bindings, so the integration target is type-checked here
# too, even though a plain `cargo test` does not build it -- and so linting stays
# possible on a machine with no libclang.
#
# There is nothing else to list. Which ENABLE_* flags the server was built with
# comes from the vendored config.h, not from a cargo feature.
LINT_FEATURES := integration

.PHONY: test image unit lint shell sync-headers bindings clean

## Unit and integration tests against a real server.
test: image
	docker run --rm $(IMAGE)

image:
	docker build -f $(DOCKERFILE) -t $(IMAGE) .

## Unit tests only. The integration target is not built without a server.
unit:
	cargo test --lib

lint:
	cargo fmt --check
	cargo clippy --all-targets --features $(LINT_FEATURES) -- -D warnings

## Point this crate at a server tree: copy its config and headers in, then
## retranslate them.
##
## The vtable is called by offset, so the headers and bindings/ have to come
## from the same tree, configured the same way. build.rs refuses to build when
## they drift; this is how you make them agree again.
##
##   make sync-headers TREE=../arcus-memcached-EE
sync-headers:
	@test -n "$(TREE)" || { echo "usage: make sync-headers TREE=<path to a server tree>"; exit 1; }
	@test -d "$(TREE)/include/memcached" || { echo "no $(TREE)/include/memcached"; exit 1; }
	@test -f "$(TREE)/config.h" || { echo "no $(TREE)/config.h -- configure that tree first"; exit 1; }
	cp $(TREE)/config.h $(TREE)/config_static.h include/
	cp $(TREE)/include/memcached/*.h include/memcached/
	@$(MAKE) bindings

## Retranslate include/ into bindings/engine_api.rs.
##
## Only needed after the headers change; the result is committed, which is what
## keeps libclang out of an ordinary build.
bindings:
	cargo build --features regen-bindings
	@git diff --stat -- bindings/

clean:
	cargo clean
	-docker image rm $(IMAGE)
