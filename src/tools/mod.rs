use std::collections::BTreeMap;
use std::sync::Arc;

use crate::client::{ProxmoxClient, ProxmoxError};
use crate::config::Clusters;
use anyhow::Context as _;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters, model::*, service::RequestContext, tool, tool_handler,
    tool_router,
};
use serde_json::{Value, json};

mod slim;
use slim::{humanize_value, slim_value};

pub mod cluster;
pub mod nodes;
pub mod params;
use params::{ALL_CLUSTERS, AnyScoped, NoParams, Scoped};

// --------------------------------------------------------------------------
// Shared helpers
// --------------------------------------------------------------------------

fn json_result(v: Value) -> Result<CallToolResult, McpError> {
    let v = slim_value(humanize_value(v));
    let text = serde_json::to_string_pretty(&v)
        .map_err(|e| McpError::internal_error(format!("marshalling response: {e}"), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn tool_error(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg)])
}

/// Summarize a tool's accepted fields ("required first") from its JSON input
/// schema, for echoing back on an invalid-params error. Returns `None` when the
/// tool takes no parameters.
fn expected_fields_summary(schema: &JsonObject) -> Option<String> {
    let props = schema.get("properties")?.as_object()?;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let (req, opt): (Vec<&String>, Vec<&String>) =
        props.keys().partition(|k| required.contains(&k.as_str()));
    let fields: Vec<String> = req
        .into_iter()
        .map(|k| format!("{k} (required)"))
        .chain(opt.into_iter().cloned())
        .collect();
    if fields.is_empty() {
        None
    } else {
        Some(fields.join(", "))
    }
}

/// Append the tool's accepted fields to an invalid-params error so a caller that
/// guessed a wrong or missing parameter name can self-correct from the error
/// alone, without a separate schema lookup. A no-arg tool is left unchanged.
fn enrich_invalid_params(error: McpError, tool: Option<&Tool>) -> McpError {
    let Some(suffix) = expected_fields_suffix(tool) else {
        return error;
    };
    McpError::invalid_params(format!("{}{suffix}", error.message), error.data)
}

/// rmcp's tool router (since 3.5.1) reports undeserializable arguments in-band:
/// an error `CallToolResult` whose text starts with this prefix.
const ARGUMENT_ERROR_PREFIX: &str = "failed to deserialize parameters:";

/// The in-band counterpart of [`enrich_invalid_params`]. Leaves every other
/// result — including ordinary tool errors — untouched.
fn enrich_argument_error(mut result: CallToolResult, tool: Option<&Tool>) -> CallToolResult {
    if result.is_error == Some(true)
        && let Some(ContentBlock::Text(text)) = result.content.first_mut()
        && text.text.starts_with(ARGUMENT_ERROR_PREFIX)
        && let Some(suffix) = expected_fields_suffix(tool)
    {
        text.text.push_str(&suffix);
    }
    result
}

fn expected_fields_suffix(tool: Option<&Tool>) -> Option<String> {
    let summary = expected_fields_summary(&tool?.input_schema)?;
    Some(format!(". Expected fields: {summary}"))
}

// --------------------------------------------------------------------------
// Server struct
// --------------------------------------------------------------------------

struct ClusterEntry {
    client: ProxmoxClient,
    url: String,
    insecure: bool,
}

struct ClusterSet {
    default: String,
    entries: BTreeMap<String, ClusterEntry>,
}

/// The MCP server — holds one Proxmox client per configured cluster and the
/// tool router.
#[derive(Clone)]
pub struct ProxmoxMcpServer {
    clusters: Arc<ClusterSet>,
    /// Held so the `call_tool` override can look up a tool's schema to enrich
    /// invalid-params errors; reused per call rather than rebuilt each time.
    tool_router: ToolRouter<Self>,
}

impl ProxmoxMcpServer {
    pub fn new(clusters: Clusters) -> anyhow::Result<Self> {
        let entries = clusters
            .entries
            .into_iter()
            .map(|(name, conn)| {
                let url = conn.url.clone();
                let insecure = conn.insecure;
                let client =
                    ProxmoxClient::new(conn).with_context(|| format!("cluster \"{name}\""))?;
                Ok((
                    name,
                    ClusterEntry {
                        client,
                        url,
                        insecure,
                    },
                ))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            clusters: Arc::new(ClusterSet {
                default: clusters.default,
                entries,
            }),
            tool_router: Self::tool_router(),
        })
    }

    fn is_multi_cluster(&self) -> bool {
        self.clusters.entries.len() > 1
    }

    fn cluster_names(&self) -> String {
        self.clusters
            .entries
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Resolve a `cluster` argument (None = default) to its name and client, or
    /// a caller-facing message naming the valid choices.
    fn client_for(&self, cluster: Option<&str>) -> Result<(&str, &ProxmoxClient), String> {
        let name = cluster.unwrap_or(&self.clusters.default);
        if let Some((name, entry)) = self.clusters.entries.get_key_value(name) {
            return Ok((name, &entry.client));
        }
        if name == ALL_CLUSTERS {
            return Err(format!(
                "cluster \"*\" is only supported by proxmox_guests_find and \
                 proxmox_cluster_resources_list; pass one cluster name ({})",
                self.cluster_names()
            ));
        }
        Err(format!(
            "unknown cluster \"{name}\"; configured clusters: {}",
            self.cluster_names()
        ))
    }

    /// Run a domain call against one cluster (None = default) and convert the
    /// result into a tool response. An unknown cluster is an in-band tool error.
    async fn call<'s, P, Fut>(
        &'s self,
        cluster: Option<&str>,
        noun: &str,
        p: P,
        f: impl FnOnce(&'s ProxmoxClient, P) -> Fut,
    ) -> Result<CallToolResult, McpError>
    where
        Fut: Future<Output = Result<Value, ProxmoxError>>,
    {
        let (name, client) = match self.client_for(cluster) {
            Ok(found) => found,
            Err(msg) => return Ok(tool_error(&msg)),
        };
        match f(client, p).await {
            Ok(v) => json_result(v),
            Err(e) => {
                let msg = self.error_message(name, noun, &e);
                tracing::error!("{msg}");
                Ok(tool_error(&msg))
            }
        }
    }

    /// `"{noun}: {error}"`, prefixed with the cluster once there is more than one
    /// to tell apart. Also what gets logged (operator-facing, via `--debug` /
    /// `--log-file`); the same text is returned in-band to the client.
    fn error_message(&self, cluster: &str, noun: &str, e: &ProxmoxError) -> String {
        if self.is_multi_cluster() {
            format!("[{cluster}] {noun}: {}", e.to_tool_message())
        } else {
            format!("{noun}: {}", e.to_tool_message())
        }
    }

    /// Body of a tool taking `Scoped<P>`: run `f` against the selected cluster.
    async fn scoped<'s, P, Fut>(
        &'s self,
        args: Scoped<P>,
        noun: &str,
        f: impl FnOnce(&'s ProxmoxClient, P) -> Fut,
    ) -> Result<CallToolResult, McpError>
    where
        Fut: Future<Output = Result<Value, ProxmoxError>>,
    {
        self.call(args.cluster.as_deref(), noun, args.inner, f)
            .await
    }

    /// Body of the "GET this fixed path" tools that take only `cluster`.
    async fn get_simple(
        &self,
        args: Scoped<NoParams>,
        path: &str,
        noun: &str,
    ) -> Result<CallToolResult, McpError> {
        self.call(args.cluster.as_deref(), noun, (), |client, ()| {
            client.get(path, &[])
        })
        .await
    }

    /// Body of a tool taking `AnyScoped<P>`. With `cluster: "*"`, runs `f`
    /// against every cluster concurrently and merges the array results into
    /// `{ <key>: [...each item tagged with "cluster"], "unreachable": {name: error} }`;
    /// fails only if every cluster failed. Otherwise behaves like [`Self::scoped`].
    async fn any_scoped<'s, P, Fut>(
        &'s self,
        args: AnyScoped<P>,
        noun: &str,
        key: &str,
        f: impl Fn(&'s ProxmoxClient, P) -> Fut,
    ) -> Result<CallToolResult, McpError>
    where
        P: Clone,
        Fut: Future<Output = Result<Value, ProxmoxError>>,
    {
        if args.cluster.as_deref() != Some(ALL_CLUSTERS) {
            return self
                .call(args.cluster.as_deref(), noun, args.inner, f)
                .await;
        }
        let calls = self.clusters.entries.iter().map(|(name, entry)| {
            let call = f(&entry.client, args.inner.clone());
            async move { (name, call.await) }
        });
        let results = futures::future::join_all(calls).await;

        let total = results.len();
        let mut items = Vec::new();
        let mut unreachable = serde_json::Map::new();
        for (name, result) in results {
            match result {
                Ok(Value::Array(list)) => items.extend(list.into_iter().map(|mut item| {
                    if let Value::Object(map) = &mut item {
                        map.insert("cluster".to_string(), Value::String(name.clone()));
                    }
                    item
                })),
                Ok(other) => items.push(json!({ "cluster": name, "data": other })),
                Err(e) => {
                    tracing::error!("{}", self.error_message(name, noun, &e));
                    unreachable.insert(name.clone(), Value::String(e.to_tool_message()));
                }
            }
        }
        if total > 0 && unreachable.len() == total {
            return Ok(tool_error(&format!(
                "{noun}: every cluster failed: {}",
                Value::Object(unreachable)
            )));
        }
        let mut out = serde_json::Map::new();
        out.insert(key.to_string(), Value::Array(items));
        if !unreachable.is_empty() {
            out.insert("unreachable".to_string(), Value::Object(unreachable));
        }
        json_result(Value::Object(out))
    }
}

// --------------------------------------------------------------------------
// Tool shims — one per endpoint
// --------------------------------------------------------------------------

#[tool_router]
impl ProxmoxMcpServer {
    // ---- configured clusters ----
    #[tool(
        description = "List the Proxmox clusters this server is configured for: name, API URL, whether TLS verification is disabled (insecure), and which one is the default. Pass a name as the `cluster` argument of any other tool to target that cluster.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_clusters_list(&self) -> Result<CallToolResult, McpError> {
        let list = self
            .clusters
            .entries
            .iter()
            .map(|(name, entry)| {
                json!({
                    "name": name,
                    "url": entry.url,
                    "insecure": entry.insecure,
                    "default": *name == self.clusters.default,
                })
            })
            .collect();
        json_result(Value::Array(list))
    }

    // ---- cluster / global ----
    #[tool(
        description = "Get the Proxmox VE API version and basic datacenter info. Doubles as a quick connectivity/health check that the server is reachable and the token works.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_version_get(
        &self,
        Parameters(p): Parameters<Scoped<NoParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.get_simple(p, "/version", "getting version").await
    }

    #[tool(
        description = "Get cluster health: quorum state, member nodes, and cluster name. Use this to check whether the cluster is quorate and every node is online.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_status_get(
        &self,
        Parameters(p): Parameters<Scoped<NoParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.get_simple(p, "/cluster/status", "getting cluster status")
            .await
    }

    #[tool(
        description = "List the entire cluster inventory in one call — every VM, container, storage, and node, with live CPU/memory/disk usage. The best starting point for \"what's running\" or for locating where a guest lives. Optional type filter: vm, storage, node, sdn. To search by guest name, use proxmox_guests_find. With cluster \"*\", queries every configured cluster and returns {resources: [...each tagged with cluster], unreachable: {cluster: error}}.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_resources_list(
        &self,
        Parameters(p): Parameters<AnyScoped<cluster::ClusterResourcesParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.any_scoped(
            p,
            "listing cluster resources",
            "resources",
            cluster::cluster_resources,
        )
        .await
    }

    #[tool(
        description = "List recent tasks (jobs/operations — backups, migrations, snapshots, start/stop) across the whole cluster, most recent first. Use this for \"what happened recently\" or to hunt failures. Filters: limit (default 50), errors (only failures), since (UNIX epoch), node. For a single node, use proxmox_nodes_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_tasks_list(
        &self,
        Parameters(p): Parameters<Scoped<cluster::ClusterTasksParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing cluster tasks", cluster::cluster_tasks)
            .await
    }

    #[tool(
        description = "Find VMs/containers anywhere in the cluster by name (case-insensitive substring), resolving each to its node and vmid. Omit name to list every guest cluster-wide. Use this to turn a hostname into the node+vmid that the per-VM tools require. With cluster \"*\", searches every configured cluster and returns {guests: [...each tagged with cluster], unreachable: {cluster: error}}; pass a match's cluster to the follow-up per-guest tools.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_guests_find(
        &self,
        Parameters(p): Parameters<AnyScoped<cluster::GuestFindParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.any_scoped(p, "finding guests", "guests", cluster::guest_find)
            .await
    }

    // ---- nodes ----
    #[tool(
        description = "List the cluster's nodes (the physical hosts/servers running Proxmox) with status, CPU, and memory.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_list(
        &self,
        Parameters(p): Parameters<Scoped<NoParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.get_simple(p, "/nodes", "listing nodes").await
    }

    #[tool(
        description = "Get detailed status for one node (a physical host): CPU, memory, load average, uptime, and kernel. Node names come from proxmox_nodes_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_status_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NodeParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "getting node status", nodes::node_status)
            .await
    }

    #[tool(
        description = "List recent tasks (jobs/operations: backups/vzdump, migrations, start/stop) that ran on one node, most recent first. Filters: limit, errors (only failures), since (UNIX epoch), type (e.g. vzdump for backups). For the whole cluster, use proxmox_cluster_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_tasks_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NodeTasksParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing node tasks", nodes::node_tasks)
            .await
    }

    // ---- QEMU VMs ----
    #[tool(
        description = "List QEMU/KVM virtual machines (VMs) on a node. Set full=true for live status of running VMs (per-VM blockstat is omitted; use proxmox_qemu_status_get for it). To find a VM by name across the cluster, use proxmox_guests_find.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::QemuListParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing VMs", nodes::qemu_list).await
    }

    #[tool(
        description = "Get a QEMU VM's configuration — its hardware and settings: cores, memory, disks, network, boot order (current values plus pending changes). Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_config_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::GuestParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "getting VM config", nodes::qemu_config)
            .await
    }

    #[tool(
        description = "Get a QEMU VM's current runtime status: whether it's running, plus live CPU, memory, and uptime. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_status_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::GuestParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "getting VM status", nodes::qemu_status)
            .await
    }

    // ---- storage ----
    #[tool(
        description = "List storage (datastores) on a node — where VM disks, ISOs, and backups live — with capacity and free space. Use this for \"how much disk space is left\". Filters: content (e.g. images, iso, backup), enabled.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_storage_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::StorageListParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing storage", nodes::storage_list).await
    }

    #[tool(
        description = "List what's stored on one storage on a node: VM disk images, ISOs, backups (vzdump), and container templates. Filter by content type, or by vmid to find a specific guest's disks/backups. Storage IDs come from proxmox_storage_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_storage_content_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::StorageContentParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing storage content", nodes::storage_content)
            .await
    }

    // ---- network ----
    #[tool(
        description = "List the network interfaces, bridges, bonds, and VLANs on a node. Use this to discover which bridges (e.g. vmbr0) are available — for example when choosing a network for a VM. Optional type filter: bridge, bond, eth, vlan, OVSBridge, etc.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_network_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NetworkListParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing node network", nodes::network_list)
            .await
    }

    #[tool(
        description = "Get the configuration of one network interface on a node (addressing, bridge ports, bond members, VLAN tag). Interface names come from proxmox_nodes_network_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_network_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NetworkInterfaceParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "getting network interface", nodes::network_get)
            .await
    }

    // ---- tasks ----
    #[tool(
        description = "Get one task's status: whether it is still running and, once stopped, its exit status (\"OK\" or the error). Takes the task's upid from proxmox_cluster_tasks_list or proxmox_nodes_tasks_list; the node is read from the UPID.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_tasks_status_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::TaskParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "getting task status", nodes::task_status)
            .await
    }

    #[tool(
        description = "Read a task's log to find out what it did or why it failed (backups, migrations, start/stop). By default returns the LAST 50 lines, where errors usually are; pass start (0-based) to page from a given line. Takes the task's upid from proxmox_cluster_tasks_list or proxmox_nodes_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_tasks_log_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::TaskLogParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "reading task log", nodes::task_log).await
    }

    // ---- snapshots ----
    #[tool(
        description = "List a QEMU VM's snapshots: name, description, creation time (snaptime), parent, and whether RAM state was saved (vmstate). The entry named \"current\" marks where the running VM sits in the snapshot tree. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_snapshots_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::GuestParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing VM snapshots", nodes::qemu_snapshots)
            .await
    }

    // ---- backups ----
    #[tool(
        description = "List the scheduled backup (vzdump) jobs: schedule, next run, target storage, mode, which guests are included (all, vmid list, or pool) and excluded, and retention (prune-backups). To see the backups that exist, use proxmox_storage_content_list with content=backup.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_backup_jobs_list(
        &self,
        Parameters(p): Parameters<Scoped<NoParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.get_simple(p, "/cluster/backup", "listing backup jobs")
            .await
    }

    #[tool(
        description = "List every guest (VM or container) that is NOT covered by any scheduled backup job. Use this to answer \"what isn't being backed up?\" in one call.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_guests_without_backup_list(
        &self,
        Parameters(p): Parameters<Scoped<NoParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.get_simple(
            p,
            "/cluster/backup-info/not-backed-up",
            "listing guests without backup",
        )
        .await
    }

    // ---- high availability ----
    #[tool(
        description = "Get the high-availability (HA) manager status: quorum, the active CRM master, each node's LRM state, and the current state of every HA-managed guest (started, stopped, fence, error, migrate...). Use this to check whether HA is healthy.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_ha_status_get(
        &self,
        Parameters(p): Parameters<Scoped<NoParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.get_simple(p, "/cluster/ha/status/current", "getting HA status")
            .await
    }

    #[tool(
        description = "List the guests managed by high availability (HA) with their configured state, restart/relocate limits, and failback setting. A guest missing here is not HA-protected.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_ha_resources_list(
        &self,
        Parameters(p): Parameters<Scoped<NoParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.get_simple(p, "/cluster/ha/resources", "listing HA resources")
            .await
    }

    // ---- disks / ZFS ----
    #[tool(
        description = "List a node's physical disks (HDD/SSD/NVMe): device path, model, serial, size, what uses it (ZFS, LVM, partitions, Ceph OSD), SMART health, and SSD wear-out. Use this for \"is any disk failing?\". For full SMART attributes of one disk, use proxmox_disks_smart_get.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_disks_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::DisksListParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing disks", nodes::disks_list).await
    }

    #[tool(
        description = "Get SMART data for one disk on a node: the overall health verdict plus every SMART attribute (reallocated sectors, temperature, power-on hours, NVMe wear...). Set healthonly=true for just the verdict. The disk path comes from proxmox_disks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_disks_smart_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::DiskSmartParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "getting SMART data", nodes::disk_smart)
            .await
    }

    #[tool(
        description = "List a node's ZFS pools with size, allocated/free space, fragmentation, dedup ratio, and health (ONLINE, DEGRADED, FAULTED...). For one pool's vdev tree, errors, and scrub status, use proxmox_disks_zfs_get.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_disks_zfs_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NodeParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing ZFS pools", nodes::zfs_list).await
    }

    #[tool(
        description = "Get detailed status of one ZFS pool on a node (like `zpool status`): state, last scrub/resilver result and progress, read/write/checksum errors, recommended action, and the vdev tree down to each disk. Pool names come from proxmox_disks_zfs_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_disks_zfs_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::ZfsPoolParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "getting ZFS pool status", nodes::zfs_get)
            .await
    }

    // ---- replication ----
    #[tool(
        description = "List the status of storage replication jobs running from one node: target node, schedule, last and next sync, duration, and failure count plus last error. Filter by vmid for one guest.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_replication_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::ReplicationParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.scoped(p, "listing replication jobs", nodes::replication_list)
            .await
    }
}

// --------------------------------------------------------------------------
// ServerHandler
// --------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for ProxmoxMcpServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "proxmox-mcp",
                env!("CARGO_PKG_VERSION"),
            ));
        let mut instructions = String::from(
            "Read-only access to a Proxmox VE cluster (every tool is a GET; nothing is \
             modified). Start with proxmox_cluster_resources_list for a one-call inventory of \
             all VMs, containers, storage, and nodes. To inspect a guest you know only by name, \
             first call proxmox_guests_find to resolve it to a node + vmid — the per-guest tools \
             (proxmox_qemu_config_get / proxmox_qemu_status_get) require both. To find out why a task (backup, migration, start/stop) \
             failed, pass its upid from a task list to proxmox_tasks_log_get. Epoch timestamp \
             fields are returned alongside an ISO 8601 <field>_iso sibling.",
        );
        if self.is_multi_cluster() {
            use std::fmt::Write as _;
            let _ = write!(
                instructions,
                " Several independent Proxmox clusters are configured: {} (default: {}). \
                 Every tool takes an optional `cluster` argument; omit it for the default. Node \
                 names and vmids are only unique within one cluster, so pass the same cluster to \
                 follow-up calls. proxmox_guests_find and proxmox_cluster_resources_list accept \
                 cluster \"*\" to search every cluster at once, tagging each result with its \
                 cluster.",
                self.cluster_names(),
                self.clusters.default
            );
        }
        info.instructions = Some(instructions);
        info
    }

    /// Dispatch via the stored router, then enrich a bad-arguments error — in-band
    /// or as a JSON-RPC invalid-params error — with the tool's accepted fields so
    /// the caller can self-correct.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let tool_name = request.name.clone();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let tool = || self.tool_router.get(&tool_name);
        match self.tool_router.call(tcc).await {
            Ok(CallToolResponse::Complete(result)) => Ok(CallToolResponse::Complete(
                enrich_argument_error(result, tool()),
            )),
            Err(e) if e.code == ErrorCode::INVALID_PARAMS => Err(enrich_invalid_params(e, tool())),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::mock_client;
    use crate::config::Connection;
    use params::encode_seg;
    use rmcp::handler::server::wrapper::Parameters;
    use serde::de::DeserializeOwned;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Answer GET `p` (when every `query` pair is present) with `{ "data": body }`.
    /// Any other request 404s, so a wrong path or parameter fails the test.
    async fn mount_data(server: &MockServer, p: &str, query: &[(&str, &str)], body: Value) {
        let mut mock = Mock::given(method("GET")).and(path(p));
        for (k, v) in query {
            mock = mock.and(query_param(*k, *v));
        }
        mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": body })))
            .mount(server)
            .await;
    }

    /// Answer every GET with `status` and a plain-text `body`.
    async fn mount_failure(server: &MockServer, status: u16, body: &str) {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(server)
            .await;
    }

    /// Build domain-fn params from JSON, through the same deserialization the
    /// tool arguments go through.
    fn params<T: DeserializeOwned>(v: Value) -> T {
        serde_json::from_value(v).unwrap()
    }

    /// Build tool args the way rmcp does: deserialize the JSON arguments object.
    fn args<T: DeserializeOwned>(v: Value) -> Parameters<T> {
        Parameters(params(v))
    }

    fn text_of(result: &CallToolResult) -> &str {
        &result.content[0].as_text().unwrap().text
    }

    fn mock_server(uri: &str) -> ProxmoxMcpServer {
        mock_multi(&[("default", uri)], "default")
    }

    fn mock_multi(clusters: &[(&str, &str)], default: &str) -> ProxmoxMcpServer {
        let entries = clusters
            .iter()
            .map(|(name, uri)| {
                let conn = Connection {
                    url: (*uri).to_string(),
                    token: format!("root@pam!mcp=secret-{name}"),
                    insecure: false,
                };
                ((*name).to_string(), conn)
            })
            .collect();
        ProxmoxMcpServer::new(Clusters {
            default: default.to_string(),
            entries,
        })
        .unwrap()
    }

    fn two_unreachable_clusters() -> ProxmoxMcpServer {
        mock_multi(
            &[("a", "https://a.invalid"), ("b", "https://b.invalid")],
            "a",
        )
    }

    fn input_schema(tool: &str) -> JsonObject {
        let router = ProxmoxMcpServer::tool_router();
        (*router.get(tool).unwrap().input_schema).clone()
    }

    /// Recursively assert no object field anywhere in `v` is JSON null.
    fn assert_no_nulls(v: &Value, ctx: &str) {
        match v {
            Value::Object(m) => {
                for (k, val) in m {
                    assert!(!val.is_null(), "unexpected null at {ctx}.{k}");
                    assert_no_nulls(val, &format!("{ctx}.{k}"));
                }
            }
            Value::Array(a) => {
                for (i, val) in a.iter().enumerate() {
                    assert_no_nulls(val, &format!("{ctx}[{i}]"));
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Tool registry and argument errors
    // ------------------------------------------------------------------

    #[test]
    fn every_tool_is_annotated_read_only() {
        // This server wraps only Proxmox GET endpoints, so every tool must
        // advertise the read-only behavior hints (readOnlyHint = true,
        // openWorldHint = false) that let MCP clients auto-approve them. Fail
        // closed: a newly added tool whose `#[tool]` omits `annotations(...)`,
        // or that ships a write-capable hint, trips this.
        for tool in ProxmoxMcpServer::tool_router().list_all() {
            let ann = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} is missing tool annotations", tool.name));
            assert_eq!(
                ann.read_only_hint,
                Some(true),
                "{} read_only_hint",
                tool.name
            );
            assert_eq!(
                ann.open_world_hint,
                Some(false),
                "{} open_world_hint",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_except_clusters_list_takes_cluster() {
        for tool in ProxmoxMcpServer::tool_router().list_all() {
            let has = tool
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| p.contains_key("cluster"));
            assert_eq!(has, tool.name != "proxmox_clusters_list", "{}", tool.name);
        }
    }

    #[test]
    fn scoped_schema_flattens_cluster_beside_domain_fields() {
        let schema = input_schema("proxmox_qemu_config_get");
        assert_eq!(schema["type"], "object");
        let props = schema["properties"].as_object().unwrap();
        for field in ["cluster", "node", "vmid"] {
            assert!(props.contains_key(field), "missing {field}: {schema:?}");
        }
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(required.contains(&"node") && required.contains(&"vmid"));
        assert!(!required.contains(&"cluster"), "cluster must be optional");
    }

    #[test]
    fn string_params_render_descriptions_inline() {
        // The string_param! types carry their description in one place; it must
        // reach each tool's schema inline (not behind a `$ref`) for LLM callers.
        let props = input_schema("proxmox_nodes_status_get")["properties"].clone();
        for (field, needle) in [
            ("node", "proxmox_nodes_list"),
            ("cluster", "proxmox_clusters_list"),
        ] {
            let f = &props[field];
            assert!(f.get("$ref").is_none(), "{field} must be inlined: {f}");
            assert!(f["description"].as_str().unwrap().contains(needle), "{f}");
        }
    }

    #[test]
    fn only_fan_out_tools_advertise_star() {
        let star = |tool: &str| {
            input_schema(tool)["properties"]["cluster"]["description"]
                .as_str()
                .unwrap()
                .contains("\"*\"")
        };
        assert!(star("proxmox_guests_find"));
        assert!(star("proxmox_cluster_resources_list"));
        assert!(!star("proxmox_qemu_config_get"));
        assert!(!star("proxmox_version_get"));
    }

    #[test]
    fn expected_fields_summary_lists_required_first() {
        let summary = expected_fields_summary(&input_schema("proxmox_qemu_config_get")).unwrap();
        assert!(
            summary.starts_with("node (required), vmid (required)"),
            "{summary}"
        );
    }

    #[test]
    fn enrich_invalid_params_appends_expected_fields() {
        let router = ProxmoxMcpServer::tool_router();
        let err = McpError::invalid_params("missing field `vmid`", None);
        let msg = enrich_invalid_params(err, router.get("proxmox_qemu_config_get")).message;
        assert!(
            msg.starts_with("missing field `vmid`. Expected fields: node (required)"),
            "{msg}"
        );

        let err = McpError::invalid_params("bad", None);
        assert_eq!(enrich_invalid_params(err, None).message, "bad");
    }

    #[test]
    fn enrich_argument_error_appends_fields_only_to_argument_errors() {
        let router = ProxmoxMcpServer::tool_router();
        let tool = router.get("proxmox_qemu_config_get");
        let enrich = |r: CallToolResult| text_of(&enrich_argument_error(r, tool)).to_string();

        let arg_err = CallToolResult::error(vec![ContentBlock::text(
            "failed to deserialize parameters: missing field `vmid`",
        )]);
        let text = enrich(arg_err);
        assert!(
            text.contains("missing field `vmid`. Expected fields: "),
            "{text}"
        );

        // An ordinary tool error (e.g. a Proxmox API failure) is left alone…
        let api_err = CallToolResult::error(vec![ContentBlock::text("getting VM config: 500")]);
        assert_eq!(enrich(api_err), "getting VM config: 500");
        // …and so is a success whose text merely looks like the prefix.
        let ok = CallToolResult::success(vec![ContentBlock::text(
            "failed to deserialize parameters: but this is data",
        )]);
        assert!(!enrich(ok).contains("Expected fields"));
    }

    #[test]
    fn malformed_upid_is_an_argument_error() {
        let err = serde_json::from_value::<Scoped<nodes::TaskParams>>(json!({ "upid": "nope" }))
            .unwrap_err();
        assert!(err.to_string().contains("invalid task UPID"), "{err}");
    }

    // ------------------------------------------------------------------
    // Domain fns through a mock Proxmox: path/query building, envelope
    // unwrapping, and per-endpoint post-processing.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn node_status_unwraps_and_slims() {
        let server = MockServer::start().await;
        let body = json!({ "uptime": 1000, "cpu": 0.05, "lock": null });
        mount_data(&server, "/nodes/pve1/status", &[], body).await;

        let raw = nodes::node_status(
            &mock_client(&server.uri()),
            params(json!({ "node": "pve1" })),
        )
        .await
        .unwrap();
        let result = slim_value(raw);
        assert_eq!(result["uptime"], json!(1000));
        assert!(result.get("lock").is_none());
        assert_no_nulls(&result, "root");
    }

    #[tokio::test]
    async fn qemu_config_interpolates_node_and_vmid() {
        let server = MockServer::start().await;
        mount_data(
            &server,
            "/nodes/pve1/qemu/100/config",
            &[],
            json!({ "name": "web01" }),
        )
        .await;
        let p = params(json!({ "node": "pve1", "vmid": 100 }));
        let r = nodes::qemu_config(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r["name"], json!("web01"));
    }

    #[tokio::test]
    async fn qemu_list_sends_full_flag_and_strips_blockstat() {
        let server = MockServer::start().await;
        let body =
            json!([{ "vmid": 100, "name": "web01", "blockstat": { "scsi0": { "rd_bytes": 1 } } }]);
        mount_data(&server, "/nodes/pve1/qemu", &[("full", "1")], body).await;
        let p = params(json!({ "node": "pve1", "full": true }));
        let r = nodes::qemu_list(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        // The heavy blockstat blob is gone; the useful fields remain.
        assert!(r[0].get("blockstat").is_none());
        assert_eq!(r[0]["name"], json!("web01"));
    }

    #[tokio::test]
    async fn qemu_snapshots_interpolates_node_and_vmid() {
        let server = MockServer::start().await;
        let body = json!([{ "name": "pre-upgrade" }, { "name": "current" }]);
        mount_data(&server, "/nodes/pve1/qemu/100/snapshot", &[], body).await;
        let p = params(json!({ "node": "pve1", "vmid": 100 }));
        let r = nodes::qemu_snapshots(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r[0]["name"], json!("pre-upgrade"));
    }

    #[tokio::test]
    async fn storage_content_interpolates_two_segments() {
        let server = MockServer::start().await;
        let p = "/nodes/pve1/storage/local-zfs/content";
        mount_data(&server, p, &[("content", "images")], json!([])).await;
        let p = params(json!({ "node": "pve1", "storage": "local-zfs", "content": "images" }));
        assert!(
            nodes::storage_content(&mock_client(&server.uri()), p)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn network_list_passes_type_filter() {
        let server = MockServer::start().await;
        let body = json!([{ "iface": "vmbr0", "type": "bridge", "comments": null }]);
        mount_data(&server, "/nodes/pve1/network", &[("type", "bridge")], body).await;
        let p = params(json!({ "node": "pve1", "type": "bridge" }));
        let r = slim_value(
            nodes::network_list(&mock_client(&server.uri()), p)
                .await
                .unwrap(),
        );
        assert_eq!(r[0]["iface"], json!("vmbr0"));
        assert_no_nulls(&r, "root");
    }

    #[tokio::test]
    async fn network_get_interpolates_iface() {
        let server = MockServer::start().await;
        let body = json!({ "iface": "vmbr0", "bridge_ports": "eth0" });
        mount_data(&server, "/nodes/pve1/network/vmbr0", &[], body).await;
        let p = params(json!({ "node": "pve1", "iface": "vmbr0" }));
        let r = nodes::network_get(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r["bridge_ports"], json!("eth0"));
    }

    #[tokio::test]
    async fn cluster_resources_passes_type_filter() {
        let server = MockServer::start().await;
        let body = json!([{ "vmid": 100, "type": "qemu", "template": null }]);
        mount_data(&server, "/cluster/resources", &[("type", "vm")], body).await;
        let p = params(json!({ "type": "vm" }));
        let r = slim_value(
            cluster::cluster_resources(&mock_client(&server.uri()), p)
                .await
                .unwrap(),
        );
        assert_eq!(r[0]["vmid"], json!(100));
        assert_no_nulls(&r, "root");
    }

    #[tokio::test]
    async fn cluster_tasks_filters_client_side() {
        let server = MockServer::start().await;
        let body = json!([
            { "upid": "a", "node": "pve1", "starttime": 100, "status": "OK" },
            { "upid": "b", "node": "pve2", "starttime": 200, "status": "some error" },
            { "upid": "c", "node": "pve1", "starttime": 300, "status": "OK" },
        ]);
        mount_data(&server, "/cluster/tasks", &[], body).await;
        let client = mock_client(&server.uri());
        let tasks = |filter: Value| cluster::cluster_tasks(&client, params(filter));

        assert_eq!(
            tasks(json!({ "node": "pve1" }))
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let r = tasks(json!({ "errors": true })).await.unwrap();
        assert_eq!(
            r,
            json!([{ "upid": "b", "node": "pve2", "starttime": 200, "status": "some error" }])
        );
        let r = tasks(json!({ "since": 200, "limit": 1 })).await.unwrap();
        assert_eq!(r.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn guest_find_filters_by_name() {
        let server = MockServer::start().await;
        let body = json!([
            { "vmid": 100, "name": "web01", "node": "pve1" },
            { "vmid": 101, "name": "db01", "node": "pve2" },
        ]);
        mount_data(&server, "/cluster/resources", &[("type", "vm")], body).await;
        let p = params(json!({ "name": "WEB" }));
        let r = cluster::guest_find(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r, json!([{ "vmid": 100, "name": "web01", "node": "pve1" }]));
    }

    const UPID: &str = "UPID:pve2:0000ABCD:00112233:66AABBCC:vzdump:100:root@pam!mcp:";

    /// Node taken from the UPID; the UPID's `:`, `@` and `!` percent-encoded
    /// into a single path segment.
    fn task_path(suffix: &str) -> String {
        format!("/nodes/pve2/tasks/{}/{suffix}", encode_seg(UPID))
    }

    #[tokio::test]
    async fn task_status_routes_to_upid_node_with_encoded_upid() {
        let server = MockServer::start().await;
        let body = json!({ "status": "stopped", "exitstatus": "OK" });
        mount_data(&server, &task_path("status"), &[], body).await;
        let r = nodes::task_status(&mock_client(&server.uri()), params(json!({ "upid": UPID })))
            .await
            .unwrap();
        assert_eq!(r["exitstatus"], json!("OK"));
    }

    async fn mount_task_log(server: &MockServer, lines: usize) {
        let entries: Vec<Value> = (1..=lines)
            .map(|n| json!({ "n": n, "t": format!("line {n}") }))
            .collect();
        mount_data(
            server,
            &task_path("log"),
            &[("limit", "100000")],
            json!(entries),
        )
        .await;
    }

    #[tokio::test]
    async fn task_log_defaults_to_tail() {
        let server = MockServer::start().await;
        mount_task_log(&server, 120).await;
        let r = nodes::task_log(&mock_client(&server.uri()), params(json!({ "upid": UPID })))
            .await
            .unwrap();
        assert_eq!(r["total_lines"], json!(120));
        assert_eq!(r["start"], json!(70));
        let lines = r["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 50);
        assert_eq!(lines[0], json!("line 71"));
        assert_eq!(lines[49], json!("line 120"));
    }

    #[tokio::test]
    async fn task_log_pages_from_start_and_clamps() {
        let server = MockServer::start().await;
        mount_task_log(&server, 10).await;
        let client = mock_client(&server.uri());
        let log = |p: Value| nodes::task_log(&client, params(p));

        let r = log(json!({ "upid": UPID, "start": 2, "limit": 3 }))
            .await
            .unwrap();
        assert_eq!(r["lines"], json!(["line 3", "line 4", "line 5"]));
        // Past the end: empty page, not a panic.
        let r = log(json!({ "upid": UPID, "start": 50 })).await.unwrap();
        assert_eq!(r["lines"], json!([]));
        // Short log with the default tail: everything.
        let r = log(json!({ "upid": UPID })).await.unwrap();
        assert_eq!(r["start"], json!(0));
        assert_eq!(r["lines"].as_array().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn disks_list_maps_flags_to_proxmox_names() {
        let server = MockServer::start().await;
        let query = [("include-partitions", "1"), ("skipsmart", "0")];
        let body = json!([{ "devpath": "/dev/sda", "health": "PASSED" }]);
        mount_data(&server, "/nodes/pve1/disks/list", &query, body).await;
        let p = params(json!({ "node": "pve1", "include_partitions": true, "skipsmart": false }));
        let r = nodes::disks_list(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r[0]["health"], json!("PASSED"));
    }

    #[tokio::test]
    async fn disk_smart_passes_disk_as_query() {
        let server = MockServer::start().await;
        let query = [("disk", "/dev/nvme0n1"), ("healthonly", "1")];
        mount_data(
            &server,
            "/nodes/pve1/disks/smart",
            &query,
            json!({ "health": "PASSED" }),
        )
        .await;
        let p = params(json!({ "node": "pve1", "disk": "/dev/nvme0n1", "healthonly": true }));
        assert!(
            nodes::disk_smart(&mock_client(&server.uri()), p)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn zfs_get_interpolates_pool_name() {
        let server = MockServer::start().await;
        let body = json!({ "name": "rpool", "state": "ONLINE" });
        mount_data(&server, "/nodes/pve1/disks/zfs/rpool", &[], body).await;
        let p = params(json!({ "node": "pve1", "name": "rpool" }));
        let r = nodes::zfs_get(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r["state"], json!("ONLINE"));
    }

    #[tokio::test]
    async fn replication_maps_vmid_to_guest() {
        let server = MockServer::start().await;
        let body = json!([{ "id": "100-0", "fail_count": 0 }]);
        mount_data(
            &server,
            "/nodes/pve1/replication",
            &[("guest", "100")],
            body,
        )
        .await;
        let p = params(json!({ "node": "pve1", "vmid": 100 }));
        let r = nodes::replication_list(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r[0]["id"], json!("100-0"));
    }

    // ------------------------------------------------------------------
    // Server-level tool calls
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn tool_returns_success_on_200() {
        let server = MockServer::start().await;
        mount_data(&server, "/version", &[], json!({ "release": "8" })).await;
        let r = mock_server(&server.uri())
            .proxmox_version_get(args(json!({})))
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
    }

    #[tokio::test]
    async fn api_failure_is_an_unprefixed_tool_error_with_one_cluster() {
        let server = MockServer::start().await;
        mount_failure(&server, 500, "no such node").await;
        let r = mock_server(&server.uri())
            .proxmox_nodes_status_get(args(json!({ "node": "ghost" })))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(
            text_of(&r).starts_with("getting node status:"),
            "{}",
            text_of(&r)
        );
    }

    #[tokio::test]
    async fn cluster_level_tools_hit_their_endpoints() {
        let server = MockServer::start().await;
        for (p, body) in [
            ("/cluster/backup", json!([{ "id": "backup-1" }])),
            (
                "/cluster/backup-info/not-backed-up",
                json!([{ "vmid": 105, "name": "scratch" }]),
            ),
            (
                "/cluster/ha/status/current",
                json!([{ "id": "quorum", "quorate": 1 }]),
            ),
            (
                "/cluster/ha/resources",
                json!([{ "sid": "vm:100", "state": "started" }]),
            ),
        ] {
            mount_data(&server, p, &[], body).await;
        }
        let mcp = mock_server(&server.uri());
        let results = [
            mcp.proxmox_backup_jobs_list(args(json!({}))).await.unwrap(),
            mcp.proxmox_guests_without_backup_list(args(json!({})))
                .await
                .unwrap(),
            mcp.proxmox_ha_status_get(args(json!({}))).await.unwrap(),
            mcp.proxmox_ha_resources_list(args(json!({})))
                .await
                .unwrap(),
        ];
        for (r, needle) in results
            .iter()
            .zip(["backup-1", "scratch", "quorum", "vm:100"])
        {
            assert_ne!(r.is_error, Some(true), "{}", text_of(r));
            assert!(text_of(r).contains(needle), "{}", text_of(r));
        }
    }

    // ------------------------------------------------------------------
    // Multi-cluster
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn routes_to_named_cluster_and_defaults_when_omitted() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount_data(&a, "/version", &[], json!({ "release": "from-a" })).await;
        mount_data(&b, "/version", &[], json!({ "release": "from-b" })).await;
        let mcp = mock_multi(&[("a", &a.uri()), ("b", &b.uri())], "a");

        let r = mcp.proxmox_version_get(args(json!({}))).await.unwrap();
        assert!(text_of(&r).contains("from-a"), "{}", text_of(&r));
        let r = mcp
            .proxmox_version_get(args(json!({ "cluster": "b" })))
            .await
            .unwrap();
        assert!(text_of(&r).contains("from-b"), "{}", text_of(&r));
    }

    #[tokio::test]
    async fn routes_domain_tools_with_flattened_params() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount_data(
            &b,
            "/nodes/pve1/qemu/100/config",
            &[],
            json!({ "name": "on-b" }),
        )
        .await;
        let mcp = mock_multi(&[("a", &a.uri()), ("b", &b.uri())], "a");
        let r = mcp
            .proxmox_qemu_config_get(args(json!({ "cluster": "b", "node": "pve1", "vmid": 100 })))
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
        assert!(text_of(&r).contains("on-b"));
    }

    #[tokio::test]
    async fn unknown_cluster_is_tool_error_listing_names() {
        let r = two_unreachable_clusters()
            .proxmox_version_get(args(json!({ "cluster": "nope" })))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(
            text_of(&r).contains("unknown cluster \"nope\"; configured clusters: a, b"),
            "{}",
            text_of(&r)
        );
    }

    #[tokio::test]
    async fn star_is_rejected_on_single_cluster_tools() {
        let r = two_unreachable_clusters()
            .proxmox_nodes_status_get(args(json!({ "cluster": "*", "node": "pve1" })))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(
            text_of(&r).contains("proxmox_guests_find"),
            "{}",
            text_of(&r)
        );
    }

    #[tokio::test]
    async fn multi_cluster_errors_name_the_cluster() {
        let a = MockServer::start().await;
        mount_failure(&a, 500, "no such node").await;
        let mcp = mock_multi(&[("a", &a.uri()), ("b", "https://b.invalid")], "a");
        let r = mcp
            .proxmox_nodes_status_get(args(json!({ "node": "ghost" })))
            .await
            .unwrap();
        assert!(
            text_of(&r).starts_with("[a] getting node status:"),
            "{}",
            text_of(&r)
        );
    }

    #[tokio::test]
    async fn fan_out_tags_results_and_reports_unreachable() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        let c = MockServer::start().await;
        let guests_a = json!([{ "vmid": 100, "name": "web01" }]);
        let guests_b = json!([{ "vmid": 200, "name": "web02" }, { "vmid": 201, "name": "db01" }]);
        mount_data(&a, "/cluster/resources", &[("type", "vm")], guests_a).await;
        mount_data(&b, "/cluster/resources", &[("type", "vm")], guests_b).await;
        mount_failure(&c, 401, "bad token").await;
        let mcp = mock_multi(&[("a", &a.uri()), ("b", &b.uri()), ("c", &c.uri())], "a");

        let r = mcp
            .proxmox_guests_find(args(json!({ "cluster": "*", "name": "web" })))
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
        let out: Value = serde_json::from_str(text_of(&r)).unwrap();
        assert_eq!(
            out["guests"],
            json!([
                { "vmid": 100, "name": "web01", "cluster": "a" },
                { "vmid": 200, "name": "web02", "cluster": "b" }
            ])
        );
        assert!(
            out["unreachable"]["c"].as_str().unwrap().contains("401"),
            "{out}"
        );
        assert_eq!(out["unreachable"].as_object().unwrap().len(), 1, "{out}");
    }

    #[tokio::test]
    async fn fan_out_omits_unreachable_when_all_succeed() {
        let a = MockServer::start().await;
        mount_data(
            &a,
            "/cluster/resources",
            &[("type", "vm")],
            json!([{ "vmid": 100 }]),
        )
        .await;
        let r = mock_server(&a.uri())
            .proxmox_guests_find(args(json!({ "cluster": "*" })))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(text_of(&r)).unwrap();
        assert_eq!(
            out,
            json!({ "guests": [{ "vmid": 100, "cluster": "default" }] })
        );
    }

    #[tokio::test]
    async fn fan_out_fails_when_every_cluster_fails() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount_failure(&a, 500, "down").await;
        mount_failure(&b, 500, "down").await;
        let r = mock_multi(&[("a", &a.uri()), ("b", &b.uri())], "a")
            .proxmox_cluster_resources_list(args(json!({ "cluster": "*" })))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(
            text_of(&r).contains("every cluster failed"),
            "{}",
            text_of(&r)
        );
    }

    #[tokio::test]
    async fn fan_out_tool_without_star_keeps_plain_array_shape() {
        let a = MockServer::start().await;
        mount_data(
            &a,
            "/cluster/resources",
            &[("type", "vm")],
            json!([{ "vmid": 100 }]),
        )
        .await;
        let r = mock_multi(&[("a", &a.uri()), ("b", "https://b.invalid")], "a")
            .proxmox_guests_find(args(json!({})))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(text_of(&r)).unwrap();
        assert_eq!(out, json!([{ "vmid": 100 }]));
    }

    #[tokio::test]
    async fn clusters_list_reports_config_without_tokens() {
        let mcp = mock_multi(
            &[
                ("a", "https://a.example.com/api2/json"),
                ("b", "https://b.example.com/api2/json"),
            ],
            "b",
        );
        let r = mcp.proxmox_clusters_list().await.unwrap();
        let text = text_of(&r);
        assert!(!text.contains("secret"), "token leaked: {text}");
        let out: Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            out,
            json!([
                { "name": "a", "url": "https://a.example.com/api2/json", "insecure": false, "default": false },
                { "name": "b", "url": "https://b.example.com/api2/json", "insecure": false, "default": true }
            ])
        );
    }

    #[test]
    fn instructions_mention_clusters_only_when_several() {
        let text = mock_server("https://a.invalid")
            .get_info()
            .instructions
            .unwrap();
        assert!(!text.contains("`cluster`"), "{text}");
        let text = mock_multi(
            &[("a", "https://a.invalid"), ("b", "https://b.invalid")],
            "b",
        )
        .get_info()
        .instructions
        .unwrap();
        assert!(text.contains("a, b (default: b)"), "{text}");
    }
}
