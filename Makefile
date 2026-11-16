# Single source of truth for local and CI checks: the workflows only call
# these targets. `make check` is the pre-release gate (it never runs a live
# suite). Every cargo invocation is --locked.

BIN ?= target/release/proxmox-mcp
MSRV := $(shell sed -n 's/^rust-version *= *"\(.*\)"/\1/p' Cargo.toml)

# A real Proxmox API token is USER@REALM!TOKENID=<uuid>. Test fixtures use
# non-UUID secrets, and docs use the xxxxxxxx-xxxx-… placeholder.
SECRET_PATTERN := ![A-Za-z0-9_.-]+=[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}

.PHONY: build release test test-real-gpg lint secrets-scan fmt fmt-check deny msrv smoke check install clean

build:
	cargo build --locked

release:
	cargo build --release --locked

test:
	cargo test --all --locked

# Classification against the real gpg (generates throwaway keys).
test-real-gpg:
	REAL_GPG=1 cargo test --locked real_gpg -- --ignored

lint: secrets-scan
	cargo clippy --all-targets --locked -- -D warnings
	cargo fmt --check
	shellcheck plugin/bin/*.sh

secrets-scan:
	@if git grep --untracked -nE '$(SECRET_PATTERN)' -- . ':!Cargo.lock'; then \
		echo "A Proxmox API token secret appears to be committed"; exit 1; \
	fi

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

deny:
	cargo deny --locked check

msrv:
	cargo +$(MSRV) test --all --locked

smoke:
	python3 scripts/smoke.py $(BIN)

check: fmt-check lint test deny release smoke

install:
	cargo install --locked --path .

clean:
	cargo clean
