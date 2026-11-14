# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
make check          # pre-release gate: fmt-check lint test deny release smoke
make build          # cargo build (debug); `make release` for the release build
make test           # cargo test --all (includes tests/launch_path.rs)
make lint           # secrets-scan + clippy --all-targets -D warnings + fmt --check
make fmt            # cargo fmt
make deny           # cargo deny check (advisories, bans, sources, licenses)
make msrv           # cargo +<rust-version> test --all
make smoke          # scripts/smoke.py against BIN (default target/release/proxmox-mcp)
make test-real-gpg  # REAL_GPG=1: classification against the real gpg (makes throwaway keys)
make install        # cargo install --path . (installs to ~/.cargo/bin)

cargo test <test_name>   # run a single test
```

Every cargo invocation in the Makefile is `--locked`. Formatting and lint must be
clean before every commit: `make fmt`, then `make lint`.

**CI/CD** (shared shape with gitlab-mcp and netbox-mcp): the Makefile is the single
source of truth, and the workflows only call its targets.
- `ci.yml` runs lint, test on ubuntu and macos, real-gpg and deny, plus msrv
  except on PRs. `release.yml` calls it as a gate.
- `release.yml` runs verify (tag == Cargo.toml version, the tag is on main, and
  the CHANGELOG has a section), then the 4-target build with `make smoke` on each
  artifact, then publish (checksums, provenance attestation, notes from
  `scripts/changelog-section.sh`).
- `scheduled.yml` runs weekly: deny, msrv, stable/beta drift and real gpg. A
  failure opens a `scheduled-failure` issue.
- Actions are pinned by commit SHA, with a `# vX.Y.Z` comment.

The toolchain is pinned in `rust-toolchain.toml` (1.100.0). The MSRV is
`rust-version` in Cargo.toml (1.88); keep it buildable. TLS is rustls on **ring**
only: reqwest uses `rustls-no-provider`, and `client::install_crypto_provider()`
installs ring. cargo-deny bans aws-lc-sys/aws-lc-rs; check with
`cargo tree -i aws-lc-sys`. A new license in the tree must be GPL-3.0-compatible
before it goes into `deny.toml`. The tool count lives in two places, which must
change together: `EXPECTED_TOOLS` in `tools/mod.rs` and in `scripts/smoke.py`.
CHANGELOG headings are `## [X.Y.Z] - YYYY-MM-DD`.

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
  main.rs          — CLI (clap; login/logout/unlock/status subcommands), tracing, stdio serve loop
  config.rs        — Config::load(dir: one <cluster>.json per cluster, default ~/.config/proxmox-mcp/clusters) + env override → Clusters{default, name → Connection{url, Credential::{Env,File,Pass}, insecure}}; env_var()/non_blank() treat blank and unexpanded ${VAR} as unset
  credentials/     — pass-compatible GPG store: PassStore/PassEntry (gpg --pinentry-mode error, status-fd classification → PassError::Locked/NoSecretKey); agent.rs = Assuan KEYINFO client, used only by `status`
  commands.rs      — login / logout / unlock / status subcommands (stderr output; no subcommand = serve stdio)
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
2. Add a `#[tool(... annotations(read_only_hint = true, open_world_hint = false))]` shim in `tools/mod.rs` taking `Parameters<Scoped<YourParams>>` whose body is `self.scoped(p, "doing x", nodes::your_fn).await` (or `get_simple` with `Scoped<NoParams>` for fixed zero-param paths). Domain fns never see `cluster`; `scoped` resolves it to a client. Use `AnyScoped` + `any_scoped` (or `get_any` for fixed paths, `any_scoped_merged` when the merged list needs re-sorting/limiting) only for cluster-wide list tools whose results make sense merged across clusters (`cluster: "*"`); the `"*"` error message and server instructions pick the tool up automatically. At startup `adapt_cluster_param` rewrites every `cluster` schema to the configured names (dropping it for a single cluster), so `#[tool_handler(router = self.tool_router)]` must keep serving the stored router. A string argument that needs a shared description or validation gets a `string_param!` type in `tools/params.rs`. The `annotations(...)` is mandatory — the `every_tool_is_annotated_read_only` test fails closed if a new tool omits it or ships a write-capable hint.
3. No routing table to update — `#[tool_router]` handles registration.

The full Proxmox API schema is at `~/source/repos/pve-docs/api-viewer/apidata.js`
(a JSON tree; `apiSchema = [...]`). 342 GET endpoints exist (PVE 9.2.13); exclude `*/rrd`
(PNG), `*/vncwebsocket`/`mtunnelwebsocket` (websockets), and `qemu/*/agent/*`
(executes guest-agent commands) from the read-only set.

## Pass-backed tokens

- **Lazy auth:** a `Credential::Pass` client decrypts on the first request under a
  mutex, caches only success, and marks the token stale on a 401 (re-reading the
  entry next time, but reusing the old token if that re-read fails so the caller
  sees the real 401). Never add a startup-time decrypt or token check: the server
  must start while the key is locked. Decrypt failures are
  `ProxmoxError::Credentials`; fan-out reports them under `unreachable`.
- **Never prompt** in the server path and never match gpg's localized text:
  classify by the numeric `[GNUPG:] ERROR` codes (low 16 bits: 85/99 locked, 17 no
  secret key), and require `DECRYPTION_OKAY` for success.
- `unlock`/`status` use the cluster file's `pass_entry` directly
  (`Config::configured_pass_entry`), not the resolved credential, so an exported
  `PROXMOX_TOKEN` can't hide the entry the server uses.

## Testing

Unit tests live beside the code: `config.rs` (loading, clusters, env override,
HTTPS, pass_entry, `${VAR}` placeholders, a lint of the README's MCP configs),
`credentials/` (one shared fake `gpg` script in `test_support` that *sources* the
entry file, so each test's "ciphertext" is a shell snippet emitting canned
`[GNUPG:]` status lines), `commands.rs` (entry naming, token parsing), `client.rs` (envelope handling, error truncation), `tools/slim.rs`,
`tools/params.rs` (`encode_seg`, `QueryBuilder`, UPID parsing), and
`tools/mod.rs` (tool schemas, domain fns and tool calls against a `wiremock`
Proxmox). In `mod.rs` tests, use `mount_data`/`mount_failure` for mocks and
build params from JSON with `params(json!(..))` / `args(json!(..))`.

`tests/launch_path.rs` spawns the built binary over MCP stdio with a pass-entry
cluster file, a fake `gpg`, and a small self-signed HTTPS stand-in for Proxmox
(the server refuses plain HTTP, so wiremock can't be used there). It asserts the
pass token is sent when `PROXMOX_TOKEN` is unset, empty, or a literal
`${PROXMOX_TOKEN}`. Tests that touch `PROXMOX_*` env vars hold
`config::tests::lock_env()`.
