# proxmox-mcp

A **read-only** [MCP](https://modelcontextprotocol.io) server that exposes
[Proxmox VE](https://www.proxmox.com/en/proxmox-virtual-environment) data as
tools for Claude and other MCP clients. Written in Rust on top of
[`rmcp`](https://crates.io/crates/rmcp).

Only `GET` endpoints are wrapped — the server cannot start, stop, create, or
delete anything. For defence in depth, give the API token the **`PVEAuditor`**
role so read-only is enforced server-side as well.

## Plugin install (recommended)

In Claude Code, this repo is its own plugin marketplace. In Claude Code 2.1.275
or later, install in one step:

```
/plugin install proxmox-mcp --marketplace 3kirt/proxmox-mcp
```

On older versions, add the marketplace first:

```
/plugin marketplace add 3kirt/proxmox-mcp
/plugin install proxmox-mcp@proxmox-mcp
```

The plugin's launcher downloads the release binary matching the plugin version
for your platform (linux or macOS, amd64 or arm64), and checks its SHA-256
against the release's `checksums.txt`. It caches the binary under
`~/.cache/proxmox-mcp/v<version>/`, or `$XDG_CACHE_HOME` if set. A plugin
update fetches the matching new binary. If there's no download for your
platform, or the download fails, it falls back to a `proxmox-mcp` on your
`PATH`. A checksum mismatch always stops it.

The plugin then reads your clusters the same way the server always does, from
`~/.config/proxmox-mcp/clusters/` (see [Configuration](#configuration)). It
also passes through `PROXMOX_URL`, `PROXMOX_TOKEN`, `PROXMOX_INSECURE` and
`PROXMOX_CLUSTER` if they're exported in the environment Claude Code was
started from. When they aren't, they arrive empty and are ignored, so the
cluster files apply. For GPG-encrypted tokens, run `proxmox-mcp login`
once from a terminal: the plugin's binary lives in the cache, so for that
step install one with `cargo install --git https://github.com/3kirt/proxmox-mcp`,
or run the cached binary directly.

## Configuration

Each Proxmox cluster gets its own JSON file in a config directory, much like a
set of kubeconfig files. The file name is the cluster's name: `prod.json`
defines the cluster `prod`.

```
~/.config/proxmox-mcp/clusters/      ($XDG_CONFIG_HOME/proxmox-mcp/clusters if set)
├── lab.json
└── prod.json
```

```json
{
  "url": "https://pve.example.com:8006/api2/json",
  "token": "USER@REALM!TOKENID=UUID",
  "insecure": false,
  "default": true
}
```

| Field | Env var | Notes |
|-------|---------|-------|
| `url` | `PROXMOX_URL` | Must be `https://`. The `/api2/json` path is appended automatically if you omit it, so `https://host:8006` also works. |
| `token` | `PROXMOX_TOKEN` | API token, format `USER@REALM!TOKENID=UUID`. |
| `pass_entry` | — | Instead of `token`: a GPG-encrypted entry in a `pass`-compatible store (see [GPG-encrypted tokens](#gpg-encrypted-tokens-pass)). Setting both is an error. |
| `password_store_dir` | — | Store root for `pass_entry`. Falls back to `$PASSWORD_STORE_DIR`, then `~/.password-store`. |
| `gnupg_home` | — | GnuPG home for decrypting `pass_entry`. Falls back to `$GNUPGHOME`, then gpg's default (`~/.gnupg`). |
| `gpg` | — | Path to the `gpg` binary (default: `gpg` from `PATH`). |
| `insecure` | `PROXMOX_INSECURE` | `true` accepts self-signed TLS certs (homelab default). |
| `default` | `PROXMOX_CLUSTER` | Marks the cluster used when a tool call omits `cluster`. Only needed when there are several files; exactly one may set it. `PROXMOX_CLUSTER=<name>` overrides it. |

- Pass `--config-dir <DIR>` to read the files from somewhere else. A
  `--config-dir` that doesn't exist is an error; the default directory may be
  missing.
- Only `*.json` files are read. Hidden files and other extensions (READMEs,
  `*.bak`) are ignored. File names may use letters, digits, `-`, `_` and `.`.
- The `PROXMOX_URL`/`PROXMOX_TOKEN`/`PROXMOX_INSECURE` env vars override the
  **default** cluster only. With no cluster files at all, they define a single
  cluster on their own (named `default`, or whatever `PROXMOX_CLUSTER` says), so
  a one-cluster setup can live entirely in the MCP client config (see
  [Build & run](#build--run)).
- A blank env var, or an unexpanded `${VAR}` placeholder an MCP client passed
  through literally, counts as unset. `PROXMOX_TOKEN` beats the default
  cluster's `token` *and* `pass_entry`.
- Unknown keys are rejected, so a typo like `"insecue"` fails at startup
  instead of silently using a default. A malformed file (bad JSON, missing
  token, `http://` URL) stops startup with an error naming the file.
  Connection and auth failures only affect calls to that cluster.

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

**Lock down the cluster files.** The server refuses to start if any of them is
world-readable, to keep the tokens from leaking:

```sh
chmod 700 ~/.config/proxmox-mcp/clusters
chmod 600 ~/.config/proxmox-mcp/clusters/*.json
```

### Multiple clusters

Every cluster file in the directory is served by one server instance:

- Every tool takes an optional `cluster` argument; omitting it uses the default
  cluster. The tool schemas list the configured names as an `enum` and name the
  default, so the assistant sees the valid choices up front. With a single
  cluster the argument is left out of the schemas entirely.
- `proxmox_clusters_list` returns the configured names and URLs (never tokens).
- `proxmox_cluster_resources_list`, `proxmox_guests_find`,
  `proxmox_cluster_tasks_list`, `proxmox_nodes_list`, and
  `proxmox_guests_without_backup_list` accept `cluster: "*"` to query every
  cluster concurrently. The result is
  `{ "<items>": [...], "unreachable": { "<name>": "<error>" } }`: each item is
  tagged with its `cluster`, and a cluster that is down or rejects its token is
  reported under `unreachable` instead of failing the call. Tasks are merged
  newest first, with `limit` applied to the merged list.

### Creating a read-only token

```sh
pveum user token add monitoring@pve mcp --privsep 0
pveum acl modify / -token 'monitoring@pve!mcp' -role PVEAuditor
```

### GPG-encrypted tokens (pass)

A cluster file can name an entry in a [`pass`](https://www.passwordstore.org/)-compatible
password store instead of holding the token in plaintext. The entry is
`<store>/<pass_entry>.gpg`, encrypted to the keys in the nearest `.gpg-id`, so
`pass show` reads it too. Only `gpg` is required, not `pass`. The entry's first
line is the full `USER@REALM!TOKENID=SECRET` token.

1. **Have a GPG key.** Check with `gpg --list-secret-keys`; if it's empty,
   create one with `gpg --quick-gen-key "Your Name <you@example.com>"`.
2. **Store the token.** Create a read-only token (above), then:

   ```sh
   proxmox-mcp login --cluster lab --url https://pve.example.com:8006 \
       --gpg-id you@example.com --insecure
   ```

   It prompts for the token without echoing it (or reads it from stdin with
   `--token-stdin`), checks it against `/version`, encrypts it to the store,
   and writes `lab.json`:

   ```json
   {
     "gpg": "/opt/homebrew/bin/gpg",
     "insecure": true,
     "pass_entry": "proxmox-mcp/pve.example.com",
     "password_store_dir": "/Users/you/.password-store",
     "url": "https://pve.example.com:8006"
   }
   ```

   - `--gpg-id` is for a new store with no `.gpg-id` yet. Drop it if you
     already use `pass`: the store's `.gpg-id` is reused, and giving both is
     an error. The `.gpg-id` is written only after encryption to that key
     works, so a mistyped key ID leaves nothing behind.
   - The entry defaults to `proxmox-mcp/<host>`, with `_<port>` appended for a
     port other than 8006. Pass `--entry` to choose another.
   - `--url` defaults to the cluster file's `url`, then `PROXMOX_URL`.
     `--cluster` defaults to the default cluster.
   - Any plaintext `token` is removed from the file. Other keys, such as
     `default`, are kept.
   - It records everything the server needs to find the token as absolute
     paths, because MCP clients launched from a GUI don't see shell-only
     exports and often start servers with a minimal `PATH` (no
     `/opt/homebrew/bin`): the store path, `gnupg_home` when `GNUPGHOME` is
     set, and the full path of the `gpg` it found on `PATH`. If the
     verification used `--insecure` or `PROXMOX_INSECURE`, it also records
     `"insecure": true`.
   - A broken file for another cluster doesn't block `login --cluster lab`
     (it's skipped with a warning).
3. **Check it.** `proxmox-mcp status` shows each cluster's URL and token
   source. For a pass entry it also shows the recipients and whether gpg-agent
   has the key unlocked. It asks gpg-agent over its socket and doesn't
   decrypt anything. A passphrase-less key or a smartcard can report
   "not cached" and still decrypt fine.
4. **Unlock when needed.** The server never prompts. If gpg-agent hasn't cached
   the passphrase, tool calls for that cluster fail with *"Proxmox token is
   locked … run `proxmox-mcp unlock`"*. Run `proxmox-mcp unlock` in a terminal,
   then retry; there's no need to restart the server. `unlock` covers every
   cluster with a `pass_entry`, or just one with `--cluster`; a cluster it
   can't unlock (e.g. a logged-out entry) is reported without stopping the
   rest. The server starts
   even while the key is locked, and keeps a decrypted token in memory after
   its first successful read.

A 401 makes the server re-read the entry on the next call, so rotating a token
with `proxmox-mcp login` takes effect without a restart. `proxmox-mcp logout
--cluster lab` deletes the stored token, leaving `pass_entry` in place for the
next `login`.

## Tools

| Tool | Endpoint |
|------|----------|
| `proxmox_clusters_list` | — (configured clusters; no API call) |
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
| `proxmox_storage_list` | `/nodes/{node}/storage` |
| `proxmox_storage_content_list` | `/nodes/{node}/storage/{storage}/content` |
| `proxmox_nodes_network_list` | `/nodes/{node}/network` |
| `proxmox_nodes_network_get` | `/nodes/{node}/network/{iface}` |
| `proxmox_tasks_status_get` | `/nodes/{node}/tasks/{upid}/status` (node read from the UPID) |
| `proxmox_tasks_log_get` | `/nodes/{node}/tasks/{upid}/log` (last 50 lines by default) |
| `proxmox_qemu_snapshots_list` | `/nodes/{node}/qemu/{vmid}/snapshot` |
| `proxmox_backup_jobs_list` | `/cluster/backup` |
| `proxmox_guests_without_backup_list` | `/cluster/backup-info/not-backed-up` |
| `proxmox_ha_status_get` | `/cluster/ha/status/current` |
| `proxmox_ha_resources_list` | `/cluster/ha/resources` |
| `proxmox_disks_list` | `/nodes/{node}/disks/list` |
| `proxmox_disks_smart_get` | `/nodes/{node}/disks/smart` |
| `proxmox_disks_zfs_list` | `/nodes/{node}/disks/zfs` |
| `proxmox_disks_zfs_get` | `/nodes/{node}/disks/zfs/{name}` |
| `proxmox_replication_list` | `/nodes/{node}/replication` |

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
- **Every call to a cluster says its token is locked.** Its `pass_entry` key
  has no cached passphrase in gpg-agent. Run `proxmox-mcp unlock` and retry.
  If the error says the gpg program wasn't found, the MCP client launched the
  server with a minimal `PATH`: set `"gpg"` in the cluster file to its full path.
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
