# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
make build       # cargo build --release
make test        # cargo test --all
make lint        # cargo clippy -- -D warnings && cargo fmt --check
make install     # cargo install --path . (installs to ~/.cargo/bin)
make clean       # remove build artifacts

cargo test <test_name>   # run a single test
```

Formatting and lint must be clean before every commit: `cargo fmt`, then
`cargo clippy -- -D warnings`.

## Architecture

A single read-only Rust binary (`rmcp`, stdio transport). Only Proxmox `GET`
endpoints are wrapped.

```
src/
  main.rs          — CLI (clap), tracing, stdio serve loop
  config.rs        — Config::load(~/.proxmox_mcp.json) + env override → Connection{url,token,insecure}
  client.rs        — reqwest wrapper; get(path,params) -> unwrapped `data` Value; ProxmoxError
  tools/
    mod.rs         — ProxmoxMcpServer, QueryBuilder, encode_seg(), json_result(), #[tool] shims, ServerHandler
    slim.rs        — slim_value(): drops null fields recursively
    cluster.rs     — cluster-scoped domain fns + param structs
    nodes.rs       — node/qemu/lxc/storage domain fns + param structs
```

## Proxmox API specifics (differ from a typical REST API)

- **Auth header:** `Authorization: PVEAPIToken=USER@REALM!TOKENID=UUID` (not Bearer/Token).
- **Read-only role:** give the token the `PVEAuditor` role for server-side enforcement.
- **Response envelope:** every response is `{ "data": ... }`; `client.get()` unwraps it.
- **No pagination:** list endpoints return plain arrays — there is no count/next/limit/offset machinery.
- **Path params:** `{node}`, `{vmid}`, `{storage}` are URL segments, interpolated in domain fns. Always wrap string segments with `encode_seg()` to prevent path injection.
- **TLS:** self-signed certs are normal; the `insecure` flag sets `danger_accept_invalid_certs`. URL scheme must still be `https`.

## Adding a tool

1. Add a `*Params` struct (schemars-described) + an async domain fn in `tools/cluster.rs` or `tools/nodes.rs` that builds the path/query and calls `client.get`.
2. Add a `#[tool(... annotations(read_only_hint = true, open_world_hint = false))]` shim in `tools/mod.rs` using the `respond!` macro (or `get_simple` for fixed zero-param paths).
3. No routing table to update — `#[tool_router]` handles registration.

The full Proxmox API schema is at `~/source/repos/pve-docs/api-viewer/apidata.js`
(a JSON tree; `apiSchema = [...]`). 340 GET endpoints exist; exclude `*/rrd`
(PNG), `*/vncwebsocket`/`mtunnelwebsocket` (websockets), and `qemu/*/agent/*`
(executes guest-agent commands) from the read-only set.

## Testing

Unit tests live beside the code: `config.rs` (loading, env override, HTTPS),
`client.rs` (`unwrap_data`, error truncation), `tools/slim.rs` (`slim_value`),
`tools/mod.rs` (`encode_seg`, `QueryBuilder`). `wiremock` is available for
HTTP-pipeline tests.
