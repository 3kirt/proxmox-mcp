.PHONY: build clean lint test install

build:
	cargo build --release

clean:
	cargo clean

lint:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

test:
	cargo test --all

install:
	cargo install --path .
