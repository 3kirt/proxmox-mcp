use crate::client::{ProxmoxClient, ProxmoxError};
use crate::tools::{QueryBuilder, encode_seg};
use serde::Deserialize;
use serde_json::Value;

// --------------------------------------------------------------------------
// Node
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NodeParams {
    #[schemars(description = "Cluster node name (see proxmox_nodes_list)")]
    pub node: String,
}

/// Read overall status (CPU, memory, uptime, kernel) of one node.
pub async fn node_status(client: &ProxmoxClient, p: NodeParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/status", encode_seg(&p.node));
    client.get(&path, &[]).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NodeTasksParams {
    #[schemars(description = "Cluster node name")]
    pub node: String,
    #[schemars(description = "Only list this number of tasks (default 50)")]
    pub limit: Option<i32>,
    #[schemars(description = "Only list tasks with an ERROR status")]
    pub errors: Option<bool>,
    #[schemars(description = "Only list tasks since this UNIX epoch")]
    pub since: Option<i64>,
}

/// Read the finished-task list for one node.
pub async fn node_tasks(client: &ProxmoxClient, p: NodeTasksParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/tasks", encode_seg(&p.node));
    let params = QueryBuilder::new()
        .opt("limit", p.limit)
        .opt("errors", p.errors.map(|b| b as i32))
        .opt("since", p.since)
        .into_params();
    client.get(&path, &params).await
}

// --------------------------------------------------------------------------
// QEMU virtual machines
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QemuListParams {
    #[schemars(description = "Cluster node name")]
    pub node: String,
    #[schemars(description = "Include full status of active VMs (slower)")]
    pub full: Option<bool>,
}

/// List QEMU/KVM virtual machines on one node.
pub async fn qemu_list(client: &ProxmoxClient, p: QemuListParams) -> Result<Value, ProxmoxError> {
    let path = format!("/nodes/{}/qemu", encode_seg(&p.node));
    let params = QueryBuilder::new()
        .opt("full", p.full.map(|b| b as i32))
        .into_params();
    client.get(&path, &params).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GuestParams {
    #[schemars(description = "Cluster node name")]
    pub node: String,
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
    #[schemars(description = "Cluster node name")]
    pub node: String,
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
        .opt("enabled", p.enabled.map(|b| b as i32))
        .into_params();
    client.get(&path, &params).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StorageContentParams {
    #[schemars(description = "Cluster node name")]
    pub node: String,
    #[schemars(description = "Storage identifier (see proxmox_node_storage_list)")]
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
