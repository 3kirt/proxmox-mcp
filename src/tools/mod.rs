use crate::client::ProxmoxClient;
use crate::config::Connection;
use rmcp::{
    ErrorData as McpError, ServerHandler, handler::server::wrapper::Parameters, model::*, tool,
    tool_handler, tool_router,
};
use serde_json::Value;

mod slim;
use slim::slim_value;

pub mod cluster;
pub mod nodes;

// --------------------------------------------------------------------------
// Shared helpers
// --------------------------------------------------------------------------

pub fn json_result(v: Value) -> Result<CallToolResult, McpError> {
    let v = slim_value(v);
    let text = serde_json::to_string_pretty(&v)
        .map_err(|e| McpError::internal_error(format!("marshalling response: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

pub fn tool_error(msg: &str) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(msg)]))
}

/// Percent-encode a single URL path segment. Encodes everything except the
/// RFC 3986 unreserved set, so user-supplied node/storage names cannot inject
/// extra path components (`/`, `..`) or break the request URL.
pub fn encode_seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Fluent builder for optional query parameters.
pub struct QueryBuilder {
    params: Vec<(&'static str, String)>,
}

impl QueryBuilder {
    pub fn new() -> Self {
        Self { params: vec![] }
    }

    /// Append `(key, v.to_string())` if `v` is Some.
    pub fn opt<T: ToString>(mut self, key: &'static str, v: Option<T>) -> Self {
        if let Some(v) = v {
            self.params.push((key, v.to_string()));
        }
        self
    }

    pub fn into_params(self) -> Vec<(&'static str, String)> {
        self.params
    }
}

impl Default for QueryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a domain function and convert the Result into an MCP response.
macro_rules! respond {
    ($self:expr, $domain_fn:path, $p:expr, $noun:literal) => {{
        let client = $self.get_client();
        match $domain_fn(client, $p).await {
            Ok(v) => json_result(v),
            Err(e) => tool_error(&format!("{}: {}", $noun, e.to_tool_message())),
        }
    }};
}

// --------------------------------------------------------------------------
// Server struct
// --------------------------------------------------------------------------

/// The MCP server — holds a Proxmox client.
#[derive(Clone)]
pub struct ProxmoxMcpServer {
    client: ProxmoxClient,
}

impl ProxmoxMcpServer {
    pub fn new(conn: Connection) -> anyhow::Result<Self> {
        Ok(Self {
            client: ProxmoxClient::new(conn)?,
        })
    }

    fn get_client(&self) -> &ProxmoxClient {
        &self.client
    }

    /// Shared body for the zero-parameter "GET this fixed path" tools.
    async fn get_simple(&self, path: &str, noun: &str) -> Result<CallToolResult, McpError> {
        match self.client.get(path, &[]).await {
            Ok(v) => json_result(v),
            Err(e) => tool_error(&format!("{}: {}", noun, e.to_tool_message())),
        }
    }
}

// --------------------------------------------------------------------------
// Tool shims — one per endpoint
// --------------------------------------------------------------------------

#[tool_router]
impl ProxmoxMcpServer {
    // ---- cluster / global ----
    #[tool(
        description = "Get the Proxmox API version and basic datacenter info.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_version(&self) -> Result<CallToolResult, McpError> {
        self.get_simple("/version", "getting version").await
    }

    #[tool(
        description = "Get cluster status: quorum, nodes, and cluster name. Returns node-level membership info.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_status(&self) -> Result<CallToolResult, McpError> {
        self.get_simple("/cluster/status", "getting cluster status")
            .await
    }

    #[tool(
        description = "Cluster-wide resource index — the best single inventory call. Lists every VM, container, storage, and node. Optional type filter: vm, storage, node, sdn.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_resources(
        &self,
        Parameters(p): Parameters<cluster::ClusterResourcesParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(
            self,
            cluster::cluster_resources,
            p,
            "listing cluster resources"
        )
    }

    #[tool(
        description = "List recent tasks across the whole cluster.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_tasks(
        &self,
        Parameters(p): Parameters<cluster::ClusterTasksParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, cluster::cluster_tasks, p, "listing cluster tasks")
    }

    // ---- nodes ----
    #[tool(
        description = "List all nodes in the cluster with status, CPU, and memory.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_list(&self) -> Result<CallToolResult, McpError> {
        self.get_simple("/nodes", "listing nodes").await
    }

    #[tool(
        description = "Read overall status of one node (CPU, memory, uptime, kernel, load).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_node_status(
        &self,
        Parameters(p): Parameters<nodes::NodeParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::node_status, p, "getting node status")
    }

    #[tool(
        description = "Read the finished-task list for one node. Filters: limit, errors (only failures), since (UNIX epoch).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_node_tasks(
        &self,
        Parameters(p): Parameters<nodes::NodeTasksParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::node_tasks, p, "listing node tasks")
    }

    // ---- QEMU VMs ----
    #[tool(
        description = "List QEMU/KVM virtual machines on a node. Set full=true for full status of running VMs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_list(
        &self,
        Parameters(p): Parameters<nodes::QemuListParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_list, p, "listing VMs")
    }

    #[tool(
        description = "Get the configuration of a QEMU VM (current values plus pending changes).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_config(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_config, p, "getting VM config")
    }

    #[tool(
        description = "Get the current runtime status of a QEMU VM (running state, CPU, memory, uptime).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_status(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_status, p, "getting VM status")
    }

    // ---- LXC containers ----
    #[tool(
        description = "List LXC containers on a node.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_list(
        &self,
        Parameters(p): Parameters<nodes::NodeParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_list, p, "listing containers")
    }

    #[tool(
        description = "Get the configuration of an LXC container.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_config(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_config, p, "getting container config")
    }

    #[tool(
        description = "Get the current runtime status of an LXC container.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_status(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_status, p, "getting container status")
    }

    // ---- storage ----
    #[tool(
        description = "Get status for all datastores on a node. Filters: content (e.g. images, iso, backup), enabled.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_node_storage_list(
        &self,
        Parameters(p): Parameters<nodes::StorageListParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::storage_list, p, "listing storage")
    }

    #[tool(
        description = "List the content (disk images, ISOs, backups, templates) of one storage on a node. Filters: content type, vmid.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_storage_content(
        &self,
        Parameters(p): Parameters<nodes::StorageContentParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::storage_content, p, "listing storage content")
    }
}

// --------------------------------------------------------------------------
// ServerHandler
// --------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for ProxmoxMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("proxmox-mcp", env!("CARGO_PKG_VERSION")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_seg_passes_unreserved() {
        assert_eq!(encode_seg("pve-node1"), "pve-node1");
        assert_eq!(encode_seg("local-zfs"), "local-zfs");
        assert_eq!(encode_seg("a.b_c~d"), "a.b_c~d");
    }

    #[test]
    fn encode_seg_escapes_path_traversal_and_specials() {
        assert_eq!(encode_seg("../etc"), "..%2Fetc");
        assert_eq!(encode_seg("a/b"), "a%2Fb");
        assert_eq!(encode_seg("a b"), "a%20b");
        assert_eq!(encode_seg("a?b#c"), "a%3Fb%23c");
    }

    #[test]
    fn query_builder_skips_none() {
        let params = QueryBuilder::new()
            .opt("a", Some(1))
            .opt::<i32>("b", None)
            .opt("c", Some("x".to_string()))
            .into_params();
        assert_eq!(params, vec![("a", "1".to_string()), ("c", "x".to_string())]);
    }
}
