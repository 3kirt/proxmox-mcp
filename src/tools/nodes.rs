use crate::client::{ProxmoxClient, ProxmoxError};
use crate::tools::{NodeId, QueryBuilder, Upid, encode_seg};
use serde::Deserialize;
use serde_json::{Value, json};

// --------------------------------------------------------------------------
// Node
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NodeParams {
    pub node: NodeId,
}

/// Read overall status (CPU, memory, uptime, kernel) of one node.
pub async fn node_status(client: &ProxmoxClient, p: NodeParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/status", encode_seg(&p.node));
    client.get(&path, &[]).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NodeTasksParams {
    pub node: NodeId,
    #[schemars(description = "Only list this number of tasks (default 50)")]
    pub limit: Option<i32>,
    #[schemars(description = "Only list tasks with an ERROR status")]
    pub errors: Option<bool>,
    #[schemars(description = "Only list tasks since this UNIX epoch")]
    pub since: Option<i64>,
    #[schemars(description = "Only list tasks of this type (e.g. vzdump, qmstart, qmshutdown)")]
    pub r#type: Option<String>,
}

/// Read the finished-task list for one node.
pub async fn node_tasks(client: &ProxmoxClient, p: NodeTasksParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/tasks", encode_seg(&p.node));
    let params = QueryBuilder::new()
        .opt("limit", p.limit)
        .flag("errors", p.errors)
        .opt("since", p.since)
        .opt("typefilter", p.r#type)
        .into_params();
    client.get(&path, &params).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskParams {
    pub upid: Upid,
}

/// Read one task's status (running/stopped and its exit status).
pub async fn task_status(client: &ProxmoxClient, p: TaskParams) -> Result<Value, ProxmoxError> {
    let path = format!(
        "/nodes/{}/tasks/{}/status",
        encode_seg(p.upid.node()),
        encode_seg(&p.upid)
    );
    client.get(&path, &[]).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskLogParams {
    pub upid: Upid,
    #[schemars(description = "Number of lines to return (default 50)")]
    pub limit: Option<i64>,
    #[schemars(
        description = "0-based line to start from. Omit to get the LAST `limit` lines, where a failure's error message usually is."
    )]
    pub start: Option<i64>,
}

/// Upper bound on lines fetched to slice the log client-side; far above any
/// realistic task log, but keeps a runaway log from being read unbounded.
const TASK_LOG_FETCH_LIMIT: i64 = 100_000;

/// Read a task's log. Proxmox pages from the start and only reports the total
/// line count outside the `data` envelope, so the whole log is fetched and
/// sliced here: by default the tail, or a page from `start`.
pub async fn task_log(client: &ProxmoxClient, p: TaskLogParams) -> Result<Value, ProxmoxError> {
    let path = format!(
        "/nodes/{}/tasks/{}/log",
        encode_seg(p.upid.node()),
        encode_seg(&p.upid)
    );
    let data = client
        .get(&path, &[("limit", TASK_LOG_FETCH_LIMIT.to_string())])
        .await?;
    let Value::Array(entries) = data else {
        return Ok(data);
    };
    let lines: Vec<Value> = entries
        .into_iter()
        .filter_map(|e| e.get("t").cloned())
        .collect();

    let total = lines.len();
    let limit = usize::try_from(p.limit.unwrap_or(50)).unwrap_or(0);
    let start = p.start.map_or_else(
        || total.saturating_sub(limit),
        |s| usize::try_from(s).unwrap_or(0).min(total),
    );
    let end = start.saturating_add(limit).min(total);
    Ok(json!({
        "total_lines": total,
        "start": start,
        "lines": lines[start..end],
    }))
}

// --------------------------------------------------------------------------
// QEMU virtual machines
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QemuListParams {
    pub node: NodeId,
    #[schemars(description = "Include full status of active VMs (slower)")]
    pub full: Option<bool>,
}

/// List QEMU/KVM virtual machines on one node.
pub async fn qemu_list(client: &ProxmoxClient, p: QemuListParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/qemu", encode_seg(&p.node));
    let params = QueryBuilder::new().flag("full", p.full).into_params();
    let mut data = client.get(&path, &params).await?;

    // `full=true` attaches per-VM `blockstat` (raw QEMU block-I/O counters) to
    // every entry. On a busy cluster this runs to ~100k chars and can blow the
    // MCP context limit. The same data is available per-VM via
    // proxmox_qemu_status_get, so drop it from the list view.
    if let Value::Array(vms) = &mut data {
        for vm in vms {
            if let Value::Object(map) = vm {
                map.remove("blockstat");
            }
        }
    }
    Ok(data)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GuestParams {
    pub node: NodeId,
    #[schemars(description = "Numeric guest ID (VMID)")]
    pub vmid: i64,
}

/// Get the configuration of a QEMU VM (current values plus pending changes).
pub async fn qemu_config(client: &ProxmoxClient, p: GuestParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/qemu/{}/config", encode_seg(&p.node), p.vmid);
    client.get(&path, &[]).await
}

/// Get the current runtime status of a QEMU VM.
pub async fn qemu_status(client: &ProxmoxClient, p: GuestParams) -> Result<Value, ProxmoxError> {
    let path = format!(
        "/nodes/{}/qemu/{}/status/current",
        encode_seg(&p.node),
        p.vmid
    );
    client.get(&path, &[]).await
}

/// List a QEMU VM's snapshots (includes a synthetic `current` entry marking the
/// running state's position in the snapshot tree).
pub async fn qemu_snapshots(client: &ProxmoxClient, p: GuestParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/qemu/{}/snapshot", encode_seg(&p.node), p.vmid);
    client.get(&path, &[]).await
}

// --------------------------------------------------------------------------
// LXC containers
// --------------------------------------------------------------------------

/// List LXC containers on one node. Reuses NodeParams (node only).
pub async fn lxc_list(client: &ProxmoxClient, p: NodeParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/lxc", encode_seg(&p.node));
    client.get(&path, &[]).await
}

/// Get the configuration of an LXC container.
pub async fn lxc_config(client: &ProxmoxClient, p: GuestParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/lxc/{}/config", encode_seg(&p.node), p.vmid);
    client.get(&path, &[]).await
}

/// Get the current runtime status of an LXC container.
pub async fn lxc_status(client: &ProxmoxClient, p: GuestParams) -> Result<Value, ProxmoxError> {
    let path = format!(
        "/nodes/{}/lxc/{}/status/current",
        encode_seg(&p.node),
        p.vmid
    );
    client.get(&path, &[]).await
}

// --------------------------------------------------------------------------
// Storage
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StorageListParams {
    pub node: NodeId,
    #[schemars(
        description = "Only list stores supporting this content type (e.g. images, iso, backup)"
    )]
    pub content: Option<String>,
    #[schemars(description = "Only list enabled stores")]
    pub enabled: Option<bool>,
}

/// Get status for all datastores available on one node.
pub async fn storage_list(
    client: &ProxmoxClient,
    p: StorageListParams,
) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/storage", encode_seg(&p.node));
    let params = QueryBuilder::new()
        .opt("content", p.content)
        .flag("enabled", p.enabled)
        .into_params();
    client.get(&path, &params).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StorageContentParams {
    pub node: NodeId,
    #[schemars(description = "Storage identifier (see proxmox_storage_list)")]
    pub storage: String,
    #[schemars(description = "Only list content of this type (e.g. images, iso, backup, vztmpl)")]
    pub content: Option<String>,
    #[schemars(description = "Only list images belonging to this VMID")]
    pub vmid: Option<i64>,
}

/// List the content (disk images, ISOs, backups, templates) of one storage.
pub async fn storage_content(
    client: &ProxmoxClient,
    p: StorageContentParams,
) -> Result<Value, ProxmoxError> {
    let path = format!(
        "/nodes/{}/storage/{}/content",
        encode_seg(&p.node),
        encode_seg(&p.storage)
    );
    let params = QueryBuilder::new()
        .opt("content", p.content)
        .opt("vmid", p.vmid)
        .into_params();
    client.get(&path, &params).await
}

// --------------------------------------------------------------------------
// Physical disks and ZFS
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DisksListParams {
    pub node: NodeId,
    #[schemars(description = "Also list partitions, not just whole disks")]
    pub include_partitions: Option<bool>,
    #[schemars(description = "Skip the SMART health check (faster; health is omitted)")]
    pub skipsmart: Option<bool>,
    #[schemars(description = "Only list disks of this kind: unused or journal_disks")]
    pub r#type: Option<String>,
}

/// List a node's physical disks with model, size, usage, SMART health and SSD wear.
pub async fn disks_list(client: &ProxmoxClient, p: DisksListParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/disks/list", encode_seg(&p.node));
    let params = QueryBuilder::new()
        .flag("include-partitions", p.include_partitions)
        .flag("skipsmart", p.skipsmart)
        .opt("type", p.r#type)
        .into_params();
    client.get(&path, &params).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DiskSmartParams {
    pub node: NodeId,
    #[schemars(
        description = "Block device path, e.g. /dev/sda (the devpath from proxmox_disks_list)"
    )]
    pub disk: String,
    #[schemars(description = "Return only the overall health verdict, not every SMART attribute")]
    pub healthonly: Option<bool>,
}

/// Read one disk's SMART data.
pub async fn disk_smart(client: &ProxmoxClient, p: DiskSmartParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/disks/smart", encode_seg(&p.node));
    let params = QueryBuilder::new()
        .opt("disk", Some(p.disk))
        .flag("healthonly", p.healthonly)
        .into_params();
    client.get(&path, &params).await
}

/// List a node's ZFS pools with size, allocation, fragmentation and health.
pub async fn zfs_list(client: &ProxmoxClient, p: NodeParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/disks/zfs", encode_seg(&p.node));
    client.get(&path, &[]).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ZfsPoolParams {
    pub node: NodeId,
    #[schemars(description = "ZFS pool name (see proxmox_disks_zfs_list)")]
    pub name: String,
}

/// Detailed status of one ZFS pool: state, scrub/resilver progress, errors,
/// and the vdev tree.
pub async fn zfs_get(client: &ProxmoxClient, p: ZfsPoolParams) -> Result<Value, ProxmoxError> {
    let path = format!(
        "/nodes/{}/disks/zfs/{}",
        encode_seg(&p.node),
        encode_seg(&p.name)
    );
    client.get(&path, &[]).await
}

// --------------------------------------------------------------------------
// Replication
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplicationParams {
    pub node: NodeId,
    #[schemars(description = "Only list replication jobs for this guest ID (VMID)")]
    pub vmid: Option<i64>,
}

/// Status of the replication jobs running on one node: last/next sync,
/// duration, and failure count/error.
pub async fn replication_list(
    client: &ProxmoxClient,
    p: ReplicationParams,
) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/replication", encode_seg(&p.node));
    let params = QueryBuilder::new().opt("guest", p.vmid).into_params();
    client.get(&path, &params).await
}

// --------------------------------------------------------------------------
// Network
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NetworkListParams {
    pub node: NodeId,
    #[schemars(
        description = "Only list interfaces of this type: bridge, bond, eth, alias, vlan, \
                       OVSBridge, OVSBond, OVSPort, OVSIntPort, vnet, or any_bridge"
    )]
    pub r#type: Option<String>,
}

/// List the network interfaces, bridges, bonds, and VLANs configured on a node.
pub async fn network_list(
    client: &ProxmoxClient,
    p: NetworkListParams,
) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/network", encode_seg(&p.node));
    let params = QueryBuilder::new().opt("type", p.r#type).into_params();
    client.get(&path, &params).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NetworkInterfaceParams {
    pub node: NodeId,
    #[schemars(
        description = "Network interface name (e.g. vmbr0, eth0; see proxmox_nodes_network_list)"
    )]
    pub iface: String,
}

/// Get the configuration of one network interface on a node.
pub async fn network_get(
    client: &ProxmoxClient,
    p: NetworkInterfaceParams,
) -> Result<Value, ProxmoxError> {
    let path = format!(
        "/nodes/{}/network/{}",
        encode_seg(&p.node),
        encode_seg(&p.iface)
    );
    client.get(&path, &[]).await
}
