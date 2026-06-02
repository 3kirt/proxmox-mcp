# Changelog

All notable changes to proxmox-mcp are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added
- wiremock pipeline tests covering the full HTTP path: `{ "data": ... }`
  envelope unwrapping, the `PVEAPIToken` auth header, query-param forwarding,
  path interpolation, `slim_value` null-stripping, and tool-level
  success/error responses.
- GPL-3.0 `LICENSE` and GitHub Actions CI (test + clippy/fmt) and release
  (tagged multi-target builds) workflows.

---

## [0.1.0] — 2026-06-02

Initial release: a read-only MCP server exposing Proxmox VE data.

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
