# The whole suite runs in a container, because the integration tests need an
# arcus server that this crate does not build — and because the container builds
# the Linux .so that actually ships, not the .dylib a macOS workstation produces.
#
#   make test      everything, in the container
#   make unit      unit tests only, on the host (no server needed)
#   make lint      format check and clippy, on the host
#   make shell     a shell in the test image, for poking around
#   make bindings  retranslate include/memcached (needs libclang)

IMAGE ?= arcvector-test
DOCKERFILE := docker/Dockerfile

# Every combination of the flags that shape the structs this crate binds.
# `base` is the empty one, which cargo spells by taking no --features at all.
#
# Six, not eight: `replication` implies `cluster-aware` (see Cargo.toml), so
# replication-without-it is not a server that exists and names no variant. Asking
# for `replication` alone still works -- cargo resolves it to the pair -- it just
# would regenerate the same file twice, so it is not listed.
ABI_COMBOS := \
	base \
	migration \
	cluster-aware \
	migration,cluster-aware \
	replication,cluster-aware \
	replication,migration,cluster-aware

# Everything except regen-bindings, which is the one feature that wants libclang.
LINT_FEATURES := integration,replication-tests,migration,cluster-aware,persistence

.PHONY: test image unit lint shell bindings clean

## Unit and integration tests against a real server.
test: image
	docker run --rm $(IMAGE)

image:
	docker build -f $(DOCKERFILE) -t $(IMAGE) .

## Unit tests only. The integration target is not built without a server.
unit:
	cargo test --lib

# Every feature but regen-bindings, so the integration target is type-checked
# here too, even though it is not built by a plain `cargo test` -- and so linting
# stays possible on a machine with no libclang.
lint:
	cargo fmt --check
	cargo clippy --all-targets --features $(LINT_FEATURES) -- -D warnings

## Retranslate include/memcached into bindings/, one file per flag combination.
## Only needed after the headers change; the results are committed, which is what
## keeps libclang out of an ordinary build.
bindings:
	@for combo in $(ABI_COMBOS); do \
		if [ "$$combo" = base ]; then f=regen-bindings; else f="regen-bindings,$$combo"; fi; \
		echo "  $$combo"; \
		cargo build --features "$$f" >/dev/null || exit 1; \
	done
	@git diff --stat -- bindings/

## The image with a shell instead of the test run, server paths already set.
shell: image
	docker run --rm -it $(IMAGE) bash

clean:
	cargo clean
	-docker image rm $(IMAGE)
