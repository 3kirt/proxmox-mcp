# Changelog

All notable changes to proxmox-mcp are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

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
