# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
make build       # cargo build --release
make test        # cargo test --all
make lint        # cargo clippy --all-targets -D warnings && cargo fmt --check
make install     # cargo install --path . (installs to ~/.cargo/bin)
make clean       # remove build artifacts

cargo test <test_name>   # run a single test
```

Formatting and lint must be clean before every commit: `cargo fmt`, then
`cargo clippy --all-targets -- -D warnings`.

Clippy runs at **pedantic** strictness: the `pedantic`, `nursery`, and `cargo`
lint groups are enabled in `Cargo.toml` under `[lints.clippy]`, with a curated,
commented allow-list for the intentional/unfixable ones (`multiple_crate_versions`,
`doc_markdown`, `wildcard_imports`). New warnings must be
fixed, not silenced, unless they belong in that list.

## Architecture

A single read-only Rust binary (`rmcp`, stdio transport). Only Proxmox `GET`
endpoints are wrapped.

```
src/
  main.rs          — CLI (clap), tracing, stdio serve loop
  config.rs        — Config::load(~/.proxmox_mcp.json) + env override → Clusters{default, name → Connection}
  client.rs        — reqwest wrapper; get(path,params) -> unwrapped `data` Value; ProxmoxError
  tools/
    mod.rs         — ProxmoxMcpServer (one client per cluster), scoped()/any_scoped() call helpers, #[tool] shims, ServerHandler
    params.rs      — string_param! types (NodeId, Upid, ClusterId…), Scoped<P>/AnyScoped<P> cluster wrappers, QueryBuilder, encode_seg()
    slim.rs        — slim_value(): drops null fields recursively
    cluster.rs     — cluster-scoped domain fns + param structs
    nodes.rs       — node/qemu/storage/disk/task domain fns + param structs
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
2. Add a `#[tool(... annotations(read_only_hint = true, open_world_hint = false))]` shim in `tools/mod.rs` taking `Parameters<Scoped<YourParams>>` whose body is `self.scoped(p, "doing x", nodes::your_fn).await` (or `get_simple` with `Scoped<NoParams>` for fixed zero-param paths). Domain fns never see `cluster`; `scoped` resolves it to a client. Use `AnyScoped` + `any_scoped` only for cluster-wide list tools whose results make sense merged across clusters (`cluster: "*"`). A string argument that needs a shared description or validation gets a `string_param!` type in `tools/params.rs`. The `annotations(...)` is mandatory — the `every_tool_is_annotated_read_only` test fails closed if a new tool omits it or ships a write-capable hint.
3. No routing table to update — `#[tool_router]` handles registration.

The full Proxmox API schema is at `~/source/repos/pve-docs/api-viewer/apidata.js`
(a JSON tree; `apiSchema = [...]`). 342 GET endpoints exist (PVE 9.2.13); exclude `*/rrd`
(PNG), `*/vncwebsocket`/`mtunnelwebsocket` (websockets), and `qemu/*/agent/*`
(executes guest-agent commands) from the read-only set.

## Testing

Unit tests live beside the code: `config.rs` (loading, clusters, env override,
HTTPS), `client.rs` (envelope handling, error truncation), `tools/slim.rs`,
`tools/params.rs` (`encode_seg`, `QueryBuilder`, UPID parsing), and
`tools/mod.rs` (tool schemas, domain fns and tool calls against a `wiremock`
Proxmox). In `mod.rs` tests, use `mount_data`/`mount_failure` for mocks and
build params from JSON with `params(json!(..))` / `args(json!(..))`.
