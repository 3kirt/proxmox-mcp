# proxmox-mcp

A **read-only** [MCP](https://modelcontextprotocol.io) server that exposes
[Proxmox VE](https://www.proxmox.com/en/proxmox-virtual-environment) data as
tools for Claude and other MCP clients. Written in Rust on top of
[`rmcp`](https://crates.io/crates/rmcp).

Only `GET` endpoints are wrapped — the server cannot start, stop, create, or
delete anything. For defence in depth, give the API token the **`PVEAuditor`**
role so read-only is enforced server-side as well.

## Configuration

Settings come from `~/.proxmox_mcp.json` (env vars override the file):

```json
{
  "url": "https://pve.example.com:8006/api2/json",
  "token": "USER@REALM!TOKENID=UUID",
  "insecure": false
}
```

| Field | Env var | Notes |
|-------|---------|-------|
| `url` | `PROXMOX_URL` | Must be `https://`. Include the `/api2/json` path. |
| `token` | `PROXMOX_TOKEN` | API token, format `USER@REALM!TOKENID=UUID`. |
| `insecure` | `PROXMOX_INSECURE` | `true` accepts self-signed TLS certs (homelab default). |

The config file must not be world-readable (the server refuses `o+r`).

### Creating a read-only token

```sh
pveum user token add monitoring@pve mcp --privsep 0
pveum acl modify / -token 'monitoring@pve!mcp' -role PVEAuditor
```

## Tools

| Tool | Endpoint |
|------|----------|
| `proxmox_version` | `/version` |
| `proxmox_cluster_status` | `/cluster/status` |
| `proxmox_cluster_resources` | `/cluster/resources` |
| `proxmox_cluster_tasks` | `/cluster/tasks` |
| `proxmox_nodes_list` | `/nodes` |
| `proxmox_node_status` | `/nodes/{node}/status` |
| `proxmox_node_tasks` | `/nodes/{node}/tasks` |
| `proxmox_qemu_list` | `/nodes/{node}/qemu` |
| `proxmox_qemu_config` | `/nodes/{node}/qemu/{vmid}/config` |
| `proxmox_qemu_status` | `/nodes/{node}/qemu/{vmid}/status/current` |
| `proxmox_lxc_list` | `/nodes/{node}/lxc` |
| `proxmox_lxc_config` | `/nodes/{node}/lxc/{vmid}/config` |
| `proxmox_lxc_status` | `/nodes/{node}/lxc/{vmid}/status/current` |
| `proxmox_node_storage_list` | `/nodes/{node}/storage` |
| `proxmox_storage_content` | `/nodes/{node}/storage/{storage}/content` |

`proxmox_cluster_resources` is the best starting point — it returns every VM,
container, storage, and node across the cluster in a single call.

## Build & run

```sh
make build      # cargo build --release
make test       # cargo test --all
make lint       # clippy -D warnings + fmt --check
make install    # installs to ~/.cargo/bin
```

Register with an MCP client (stdio transport):

```json
{
  "mcpServers": {
    "proxmox": {
      "type": "stdio",
      "command": "/path/to/proxmox-mcp",
      "env": {
        "PROXMOX_URL": "https://pve.example.com:8006/api2/json",
        "PROXMOX_TOKEN": "monitoring@pve!mcp=...",
        "PROXMOX_INSECURE": "true"
      }
    }
  }
}
```
