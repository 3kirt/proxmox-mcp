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
| `url` | `PROXMOX_URL` | Must be `https://`. The `/api2/json` path is appended automatically if you omit it, so `https://host:8006` also works. |
| `token` | `PROXMOX_TOKEN` | API token, format `USER@REALM!TOKENID=UUID`. |
| `insecure` | `PROXMOX_INSECURE` | `true` accepts self-signed TLS certs (homelab default). |

**The Proxmox API lives under `/api2/json`.** You can point `url` at the bare
host (`https://host:8006`) — the server appends the path for you — but a config
pointed at the wrong path is the most common setup mistake.

**Token format with `@` in the username.** The full `USER@REALM` is preserved
verbatim, *including* any `@` inside the username. Custom realms with
email-style usernames therefore look doubled, and that is correct:

```
user@example.com@pve!mcp=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

Don't truncate the first `@` segment.

**Lock down the config file.** The server refuses to start if the file is
world-readable, to keep the token from leaking:

```sh
chmod 600 ~/.proxmox_mcp.json
```

### Creating a read-only token

```sh
pveum user token add monitoring@pve mcp --privsep 0
pveum acl modify / -token 'monitoring@pve!mcp' -role PVEAuditor
```

## Tools

| Tool | Endpoint |
|------|----------|
| `proxmox_version_get` | `/version` |
| `proxmox_cluster_status_get` | `/cluster/status` |
| `proxmox_cluster_resources_list` | `/cluster/resources` |
| `proxmox_cluster_tasks_list` | `/cluster/tasks` |
| `proxmox_guests_find` | `/cluster/resources?type=vm` (name filter) |
| `proxmox_nodes_list` | `/nodes` |
| `proxmox_nodes_status_get` | `/nodes/{node}/status` |
| `proxmox_nodes_tasks_list` | `/nodes/{node}/tasks` |
| `proxmox_qemu_list` | `/nodes/{node}/qemu` |
| `proxmox_qemu_config_get` | `/nodes/{node}/qemu/{vmid}/config` |
| `proxmox_qemu_status_get` | `/nodes/{node}/qemu/{vmid}/status/current` |
| `proxmox_lxc_list` | `/nodes/{node}/lxc` |
| `proxmox_lxc_config_get` | `/nodes/{node}/lxc/{vmid}/config` |
| `proxmox_lxc_status_get` | `/nodes/{node}/lxc/{vmid}/status/current` |
| `proxmox_storage_list` | `/nodes/{node}/storage` |
| `proxmox_storage_content_list` | `/nodes/{node}/storage/{storage}/content` |
| `proxmox_nodes_network_list` | `/nodes/{node}/network` |
| `proxmox_nodes_network_get` | `/nodes/{node}/network/{iface}` |

`proxmox_cluster_resources_list` is the best starting point — it returns every VM,
container, storage, and node across the cluster in a single call.
`proxmox_guests_find` resolves a guest name (VM or container) to its node + vmid
(call it with no name to list every guest cluster-wide).

UNIX-epoch fields (`ctime`, `starttime`, `endtime`) are returned with an
`<field>_iso` ISO 8601 sibling alongside the raw number.

### Troubleshooting

- **`proxmox_storage_content_list` returns an empty list.** If a storage shows space
  used but the content list is empty, the token most likely lacks
  `Datastore.Audit` on that storage. When Proxmox reports a partial permission
  failure the server now surfaces it as an error rather than returning `[]`
  silently. Grant the read-only `PVEAuditor` role — on `/` to cover everything,
  or scoped to the one storage:

  ```sh
  pveum acl modify /storage/<id> -token '<user>@<realm>!<tokenid>' -role PVEAuditor
  ```
- **Every call fails with `no such file '/version'`.** The `url` is missing the
  `/api2/json` path — recent versions append it automatically; upgrade or add it.
- **Diagnosing a failing call.** Run with `--debug` to log every Proxmox request
  (method + URL) and the full, untruncated error body. Since an MCP client owns
  the server's stdio, pass `--log-file <path>` to capture the trace to a file
  (created `0600`; the API token is never logged). `RUST_LOG` overrides `--debug`
  for finer control, e.g. `RUST_LOG=proxmox_mcp=trace`.

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
