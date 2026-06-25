use crate::client::{ProxmoxClient, ProxmoxError};
use crate::tools::{NodeId, QueryBuilder, encode_seg};
use serde::Deserialize;
use serde_json::Value;

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
