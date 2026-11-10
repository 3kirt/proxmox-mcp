# Changelog

All notable changes to proxmox-mcp are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

## [0.15.0] — 2026-11-09

rmcp 3.9 for a RustSec advisory, and blank env vars no longer override the
cluster files.

### Fixed
- A set-but-empty `PROXMOX_URL`, `PROXMOX_TOKEN`, `PROXMOX_INSECURE` or
  `PROXMOX_CLUSTER` now counts as unset, instead of overriding the cluster file
  (and sending an empty token). MCP clients pass `""` when a `${VAR}` they
  expand isn't exported.

### Security
- rmcp 3.9 (floor raised from 3.8) for RUSTSEC-2026-0462, unbounded response
  buffering in rmcp's Streamable HTTP client. This server doesn't use that
  client, but the advisory flagged the lockfile.

### Internal
- CI also runs weekly, so new RustSec advisories surface without a push.

## [0.14.0] — 2026-11-03

Per-cluster config files, cluster-aware tool schemas, and `"*"` on more tools.

**Breaking:** cluster configuration moved from `~/.proxmox_mcp.json` to a
directory with one file per cluster. See "Migrating" below.

### Added
- `cluster: "*"` now also works on `proxmox_cluster_tasks_list` (merged newest
  first across clusters, `limit` applied to the merged list),
  `proxmox_nodes_list`, and `proxmox_guests_without_backup_list`.

### Changed
- **One config file per cluster.** Clusters are read from
  `~/.config/proxmox-mcp/clusters/` (`$XDG_CONFIG_HOME/proxmox-mcp/clusters` if
  set), one `<name>.json` per cluster holding `url`/`token`/`insecure`, much
  like a set of kubeconfig files. The file name is the cluster name. With
  several files, the one with `"default": true` serves calls that omit
  `cluster`; `PROXMOX_CLUSTER=<name>` overrides that. `--config-dir <DIR>`
  replaces `--config <FILE>`.
- **The `cluster` argument lists the configured clusters.** With several
  clusters, each tool's schema gives `cluster` an `enum` of the configured
  names (plus `"*"` where supported) and names the default, so the assistant
  no longer needs `proxmox_clusters_list` to discover valid values. With a
  single cluster, `cluster` is omitted from the schemas altogether.
- `tools/list` serves the router built at startup instead of rebuilding every
  tool schema on each request.

### Removed
- `~/.proxmox_mcp.json` (both the single-cluster form and the `clusters` map)
  and the `--config` flag. The server no longer reads that file.

### Migrating
- Single cluster: move `~/.proxmox_mcp.json` to
  `~/.config/proxmox-mcp/clusters/<name>.json` unchanged; the old top-level
  fields are exactly the new file format. Or skip the file and keep using
  `PROXMOX_URL`/`PROXMOX_TOKEN`.
- `clusters` map: write each entry to its own `<name>.json`, and add
  `"default": true` to the one that `"default"` named.

## [0.13.0] — 2026-11-02

Security dependency updates, stricter config and argument validation,
cancellation support, and compact tool output.

### Changed
- **Tool output is compact JSON** instead of pretty-printed, cutting the token
  cost of every response. The content is unchanged.
- **Closed-value filters advertise their allowed values.** The `type` filter of
  `proxmox_cluster_resources_list`, `proxmox_disks_list`, and
  `proxmox_nodes_network_list` is now an `enum` in the tool schema, and an
  unknown value is rejected with the list of valid ones. The network filter
  gains `fabric`, `any_local_bridge`, and `include_sdn` (per the PVE 9.2 API).
- **`limit`, `start`, and `vmid` must be non-negative.** The schema advertises
  `minimum: 0`; a negative value is rejected up front instead of being clamped
  to 0 or passed through to Proxmox.
- A client's `notifications/cancelled` now stops the tool call at once and
  drops its in-flight Proxmox request, instead of letting it run to completion.
- The stdio transport uses rmcp's `stdio_pipes` (non-blocking pipe I/O on
  Linux; unchanged elsewhere). Requires rmcp 3.8.

### Fixed
- An explicit `--config` path that doesn't exist is now an error, instead of
  being silently ignored in favor of env vars. Only the default
  `~/.proxmox_mcp.json` may be absent, and an unreadable config file reports
  its real I/O error rather than being treated as missing.
- Unknown keys in the config file are rejected, so a typo such as `"insecue"`
  or `"cluster"` fails at startup instead of silently resolving to a default.

### Security
- Dependency updates clear RUSTSEC-2026-0346, -0347, -0348, and -0349
  (rustls 0.23.46), RUSTSEC-2026-0382 (rustls-webpki 0.103.16),
  RUSTSEC-2026-0383 (webpki-root-certs 1.0.11), and the RUSTSEC-2026-0386
  unsoundness warning (tokio 1.54.1).
- The `PVEAPIToken` header is marked sensitive, and the raw config's `Debug`
  output redacts tokens.

### Internal
- New `cargo audit` CI job, which also gates releases.
- Declares `rust-version = 1.88`; license identifier is now `GPL-3.0-only`.

## [0.12.0] — 2026-10-05

Build and release pipeline fixes only; tools and behavior are unchanged.

### Internal
- Lints clean on Rust 1.99: clippy's new `unused_async_trait_impl` lint is
  allowed on the `ServerHandler` impl, where it fires on the `list_tools` that
  rmcp's `#[tool_handler]` macro generates.
- The release workflow now runs the full CI checks (tests, clippy, fmt) as a
  required job before building, so a tag whose lint or tests fail no longer
  publishes a release. `/release` updates the local stable toolchain first and
  lints with `--all-targets`, matching CI.

## [0.11.0] — 2026-10-05

Multi-cluster support, task/backup/HA/disk-health tools, and rmcp 3.5. Removes
the LXC tools.

### Added
- **Multiple clusters in one server instance** (#4). The config file accepts a
  `clusters` map (name → `url`/`token`/`insecure`) plus a `default`; the
  existing single-cluster form keeps working as a cluster named `default`.
  Every tool takes an optional `cluster` argument (omit for the default), and
  `PROXMOX_*` env vars override the default cluster only.
- `proxmox_clusters_list` — the configured cluster names, URLs, `insecure`
  flags, and which is the default (tokens are never returned).
- `proxmox_guests_find` and `proxmox_cluster_resources_list` accept
  `cluster: "*"` to query every cluster concurrently, tagging each result with
  its `cluster`. A cluster that is down or rejects its token is reported under
  `unreachable` rather than failing the whole call.
- **Task drill-down:** `proxmox_tasks_status_get` and `proxmox_tasks_log_get`
  take a task's `upid` (the node is read from it), so a failed backup or
  migration found in a task list can be explained without leaving the client.
  The log returns the last 50 lines by default, or a page via `start`/`limit`.
- `proxmox_qemu_snapshots_list` — a VM's snapshots and the snapshot tree.
- **Backups:** `proxmox_backup_jobs_list` (scheduled vzdump jobs) and
  `proxmox_guests_without_backup_list` (guests no job covers).
- **High availability:** `proxmox_ha_status_get` (manager, quorum, per-guest
  state) and `proxmox_ha_resources_list` (HA-managed guests).
- **Disk health:** `proxmox_disks_list` (disks with SMART health and SSD
  wear-out), `proxmox_disks_smart_get`, `proxmox_disks_zfs_list`, and
  `proxmox_disks_zfs_get` (`zpool status`-style detail).
- `proxmox_replication_list` — replication job status per node, optionally for
  one guest.
- All new tools work with the `PVEAuditor` role.

### Changed
- With more than one cluster configured, tool error messages are prefixed with
  the cluster name (`[site-b] getting VM config: …`), and the server
  instructions list the configured clusters.
- `Connection`'s `Debug` output redacts the API token.
- **rmcp 3.1 → 3.5.** `get_info` now uses `ServerConfig` (the deprecated
  `ServerInfo` alias's replacement; same type). rmcp 3.5.1 also started
  returning undeserializable tool arguments as an in-band error result rather
  than a JSON-RPC invalid-params error, which bypassed the "Expected fields: …"
  hint appended to those errors; the `call_tool` override now enriches both
  forms. Also refreshed `clap`, `reqwest`, `thiserror`, and `tokio` to their
  latest patch releases.

### Removed
- **Breaking:** the LXC container tools `proxmox_lxc_list`,
  `proxmox_lxc_config_get`, and `proxmox_lxc_status_get`. Containers still
  appear in `proxmox_cluster_resources_list`, `proxmox_guests_find`, and the
  backup tools.

### Security
- Updated `rustls` to 0.23.45 (RUSTSEC-2026-0285, medium: TLS 1.3 handshake
  messages accepted across encryption-level boundaries) and `h2` to 0.4.16
  (RUSTSEC-2026-0258: unbounded empty DATA frames), both pulled in via
  `reqwest`. This also replaces the yanked `chacha20` 0.10.1.

### Internal
- Tool shims call generic `scoped()` / `any_scoped()` methods instead of the
  `respond!` / `respond_any!` macros; `cluster: "*"` calls now run concurrently
  via `futures::join_all` on borrowed clients rather than spawned tasks.
- String tool arguments (`NodeId`, `Upid`, `ClusterId`, `ClusterSelector`) are
  generated by one `string_param!` macro in the new `tools/params.rs`, which
  also holds the cluster wrappers, `QueryBuilder`, and `encode_seg`.
- Removed the unused `url` dependency and a stale clippy allow; the rmcp
  requirement is now `3.5`, matching the APIs used. CI lints test code
  (`--all-targets`), and the Makefile's dead `docker-build` target is gone.
- Tests share `mount_data` / `mount_failure` mock helpers and build arguments
  from JSON.

## [0.10.0] — 2026-08-06

Upgrade to rmcp 3.x.

### Changed
- **rmcp 2.1 → 3.1** (major). `ServerHandler::call_tool` now returns the
  MRTR-aware `CallToolResponse` enum instead of the bare `CallToolResult`; the
  overridden dispatch in `tools/mod.rs` was updated accordingly. Also picks up
  rmcp 3.1.1's handler-macro fix that stamps a `ttl_ms`/`cache_scope`
  (SEP-2549) hint on the macro-generated `list_tools` response — free for this
  server, since it never overrides `list_tools` itself.
- Routine dependency refresh: `anyhow`, `clap`, `schemars`, `serde`,
  `serde_json`, `thiserror`, and `tokio` bumped to their latest in-range patch
  versions.

## [0.9.0] — 2026-07-04

Upgrade to rmcp 2.x and tighten the lint gate.

### Added
- **Read-only annotation guard test** — `every_tool_is_annotated_read_only`
  asserts every registered tool declares `readOnlyHint = true` and
  `openWorldHint = false`. Fail-closed: a newly added tool whose `#[tool]` omits
  `annotations(...)`, or that ships a write-capable hint, trips the test.

### Changed
- **rmcp 1.8 → 2.1** (major). The only 2.x break that touched this crate is the
  `model::Content` → `ContentBlock` rename (`json_result` / `tool_error`);
  `ProgressNotificationParam` is unused here, so the other 2.x migration point
  does not apply.
- **Clippy runs at pedantic strictness** — the `pedantic`, `nursery`, and `cargo`
  lint groups are enabled in `Cargo.toml` under `[lints.clippy]`, so every clippy
  run (CI, rust-analyzer, and the release gate) enforces them. A curated,
  commented allow-list covers the intentional/unfixable lints
  (`multiple_crate_versions`, `doc_markdown`, `wildcard_imports`,
  `struct_field_names`). Added package `keywords`/`categories` metadata and
  strengthened `make lint` to run `--all-targets`.

## [0.8.0] — 2026-06-25

### Removed
- **MCP logging capability** — the server no longer advertises `logging`, sends
  `notifications/message` log entries, or handles `logging/setLevel`. MCP's
  Logging feature was deprecated by [SEP-2577](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2577);
  the per-tool-error notification it sent was redundant with the error already
  returned in the tool result.

### Changed
- **rmcp 1.7 → 1.8**, which marks the Logging APIs deprecated (prompting the
  removal above).
- Tool errors are now logged via `tracing::error!` (operator-facing, through the
  existing `--debug` / `--log-file` pipeline) instead of being pushed to the
  client over the MCP logging channel. The error is still returned in-band as
  the tool result, so MCP clients see no change.

## [0.7.0] — 2026-06-25

### Added
- `proxmox_nodes_network_list` and `proxmox_nodes_network_get` — enumerate a
  node's network interfaces, bridges, bonds, and VLANs (`/nodes/{node}/network`,
  optional `type` filter) and read one interface's config
  (`/nodes/{node}/network/{iface}`). Lets you discover available bridges (e.g.
  for picking a VM network) without inspecting existing guest configs. (#3)

### Security
- Updated `quinn-proto` to 0.11.15, resolving RUSTSEC-2026-0185 (high-severity
  remote memory exhaustion; pulled in transitively via `reqwest`). Also bumped
  `anyhow` to 1.0.103. `rmcp` is held at 1.7 for now: 1.8 deprecates the MCP
  logging capability (SEP-2577) this server uses, to be migrated separately.

## [0.6.0] — 2026-06-21

### Added
- `--debug` and `--log-file` flags for request tracing: `--debug` logs every
  Proxmox request (method + URL) and full, untruncated error bodies (sets
  `proxmox_mcp=debug` unless `RUST_LOG` is set); `--log-file` routes the JSON
  trace to a file — the reliable capture path when an MCP client owns stdio. The
  API token is never logged, and the log file is created `0600` on Unix.
- `initialize` now returns server instructions describing the read-only model
  and the `proxmox_cluster_resources_list` → `proxmox_guests_find` →
  per-guest-tool workflow.
- Invalid-parameter errors now append the tool's accepted fields (required
  first) so a caller that guessed a wrong/missing name can self-correct without
  a schema round-trip.

### Changed
- Tool descriptions reworked for LLM discoverability: Proxmox jargon bridged to
  common terms, sibling tools cross-referenced, and every per-guest tool points
  at `proxmox_guests_find` for resolving a name to node + vmid.

### Internal
- `node` parameters use a `NodeId` newtype so the field description lives in one
  place (was duplicated across six param structs).

## [0.5.0] — 2026-06-10

### Added
- Tool failures are now emitted as MCP `notifications/message` logs (the server
  advertises the `logging` capability and honors the client's `logging/setLevel`
  threshold), so clients see *why* a call failed, not just an error result.

### Changed
- **Breaking:** all tool names now follow the `proxmox_<resource>_<verb>`
  convention shared with gitlab-mcp — resource segments are plural and every
  tool ends in an action verb. Renames: `proxmox_version` →
  `proxmox_version_get`, `proxmox_cluster_status` → `proxmox_cluster_status_get`,
  `proxmox_cluster_resources` → `proxmox_cluster_resources_list`,
  `proxmox_cluster_tasks` → `proxmox_cluster_tasks_list`, `proxmox_guest_find` →
  `proxmox_guests_find`, `proxmox_node_status` → `proxmox_nodes_status_get`,
  `proxmox_node_tasks` → `proxmox_nodes_tasks_list`, `proxmox_qemu_config` →
  `proxmox_qemu_config_get`, `proxmox_qemu_status` → `proxmox_qemu_status_get`,
  `proxmox_lxc_config` → `proxmox_lxc_config_get`, `proxmox_lxc_status` →
  `proxmox_lxc_status_get`, `proxmox_node_storage_list` → `proxmox_storage_list`,
  and `proxmox_storage_content` → `proxmox_storage_content_list`.
  `proxmox_nodes_list`, `proxmox_qemu_list`, and `proxmox_lxc_list` are
  unchanged. MCP clients referencing the old names must update.

## [0.4.0] — 2026-06-07

### Changed
- Internal refactor only, no behavior change: the per-tool
  `Result → CallToolResult` conversion is now a single shared `into_response`
  helper, a `QueryBuilder::flag` helper centralizes Proxmox `1`/`0` booleans,
  and `humanize_value`'s epoch-to-ISO insertion was simplified.

## [0.3.0] — 2026-06-02

### Added
- `proxmox_guest_find` — resolve a guest name (VM or container) to its node +
  vmid across the whole cluster (case-insensitive substring; omit the name to
  list every guest). One call replaces a full `proxmox_cluster_resources` dump
  scanned by hand.
- `proxmox_cluster_tasks` now accepts `limit` (default 50), `errors`, `since`,
  and `node` filters (applied client-side, as the API takes no parameters).
- `proxmox_node_tasks` gained a `type` filter (Proxmox `typefilter`).
- ISO 8601 `<field>_iso` siblings are added next to epoch fields (`ctime`,
  `starttime`, `endtime`) so timestamps aren't opaque numbers.

### Changed
- `proxmox_qemu_list` with `full=true` no longer includes per-VM `blockstat`
  (raw QEMU block-I/O counters), which could exceed MCP context limits on busy
  clusters. The same data is available per-VM via `proxmox_qemu_status`.
- The configured `url` now has `/api2/json` appended automatically when missing,
  so a bare `https://host:8006` works instead of failing with a misleading 500.

### Fixed
- Responses with an empty `data` and a non-empty `errors` map (e.g. a storage
  content list denied for want of `Datastore.Audit`) now surface as an error
  instead of silently returning `[]`. Aggregating endpoints that return useful
  `data` alongside per-item `errors` (a node down in `cluster/resources`) are
  left untouched.

### Documentation
- README: documented the `/api2/json` requirement, the `chmod 600` config
  permission check, email-style (`@`-in-username) token format, and a
  troubleshooting section.

## [0.2.0] — 2026-06-02

First release: a read-only MCP server exposing Proxmox VE data.

### Added
- stdio MCP server (`rmcp`) wrapping Proxmox VE `GET` endpoints only.
- 15 core-inventory tools across cluster, nodes, QEMU, LXC, and storage:
  `proxmox_version`, `proxmox_cluster_status`, `proxmox_cluster_resources`,
  `proxmox_cluster_tasks`, `proxmox_nodes_list`, `proxmox_node_status`,
  `proxmox_node_tasks`, `proxmox_qemu_list`, `proxmox_qemu_config`,
  `proxmox_qemu_status`, `proxmox_lxc_list`, `proxmox_lxc_config`,
  `proxmox_lxc_status`, `proxmox_node_storage_list`, `proxmox_storage_content`.
- `PVEAPIToken` authentication; pair the token with the `PVEAuditor` role for
  server-side read-only enforcement.
- Configuration via `~/.proxmox_mcp.json` or `PROXMOX_URL` / `PROXMOX_TOKEN` /
  `PROXMOX_INSECURE` env vars; HTTPS enforced, world-readable config rejected.
- `insecure` flag (`danger_accept_invalid_certs`) for self-signed homelab TLS.
- Response slimming (`slim_value`) that recursively drops null fields, and
  `encode_seg()` path-segment encoding to prevent path injection.
- wiremock pipeline test suite covering the full HTTP path: `{ "data": ... }`
  envelope unwrapping, the `PVEAPIToken` auth header, query-param forwarding,
  path interpolation, `slim_value` null-stripping, and tool-level
  success/error responses.
- GPL-3.0 `LICENSE`.
- GitHub Actions CI (test + clippy/fmt) and release workflows; the release
  workflow builds linux amd64/arm64 and darwin arm64 binaries with checksums
  on tagged pushes.

### Documentation
- README, `CLAUDE.md`, and this changelog.
- `/release` slash command automating the version bump, dependency audit,
  quality gate, changelog update, and tag.
