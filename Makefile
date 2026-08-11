# The whole suite runs in a container, because the integration tests need an
# arcus daemon that this crate does not build — and because the container builds
# the Linux .so that actually ships, not the .dylib a macOS workstation produces.
#
#   make test     everything, in the container
#   make unit     unit tests only, on the host (no daemon needed)
#   make lint     format check and clippy, on the host
#   make shell    a shell in the test image, for poking around

IMAGE ?= arcvector-test
DOCKERFILE := docker/Dockerfile

.PHONY: test image unit lint shell clean

## Unit and integration tests against a real daemon.
test: image
	docker run --rm $(IMAGE)

image:
	docker build -f $(DOCKERFILE) -t $(IMAGE) .

## Unit tests only. The integration target is not built without a daemon.
unit:
	cargo test --lib

# --all-features so the integration target is type-checked here too, even though
# it is not built by a plain `cargo test`.
lint:
	cargo fmt --check
	cargo clippy --all-targets --all-features -- -D warnings

## The image with a shell instead of the test run, daemon paths already set.
shell: image
	docker run --rm -it $(IMAGE) bash

clean:
	cargo clean
	-docker image rm $(IMAGE)
