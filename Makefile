BINARY     := proxmox-mcp
IMAGE      := proxmox-mcp
VERSION    := $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)

.PHONY: build clean lint test install docker-build

build:
	cargo build --release

clean:
	cargo clean

lint:
	cargo clippy -- -D warnings
	cargo fmt --check

test:
	cargo test --all

install:
	cargo install --path .

docker-build:
	docker build -t $(IMAGE):$(VERSION) -t $(IMAGE):latest .
