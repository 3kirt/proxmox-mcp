# Changelog

All notable changes to proxmox-mcp are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

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
