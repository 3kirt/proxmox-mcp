use crate::client::ProxmoxClient;
use crate::config::Connection;
use rmcp::{
    ErrorData as McpError, ServerHandler, handler::server::wrapper::Parameters, model::*, tool,
    tool_handler, tool_router,
};
use serde_json::Value;

mod slim;
use slim::{humanize_value, slim_value};

pub mod cluster;
pub mod nodes;

// --------------------------------------------------------------------------
// Shared helpers
// --------------------------------------------------------------------------

pub fn json_result(v: Value) -> Result<CallToolResult, McpError> {
    let v = slim_value(humanize_value(v));
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
        description = "List recent tasks across the whole cluster. Filters: limit (default 50), errors (only failures), since (UNIX epoch), node.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_tasks(
        &self,
        Parameters(p): Parameters<cluster::ClusterTasksParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, cluster::cluster_tasks, p, "listing cluster tasks")
    }

    #[tool(
        description = "Find VMs/containers anywhere in the cluster by name (case-insensitive substring), resolving each to its node and vmid. Omit name to list every guest cluster-wide. Use this to turn a hostname into the node+vmid that the per-VM tools require.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_guest_find(
        &self,
        Parameters(p): Parameters<cluster::GuestFindParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, cluster::guest_find, p, "finding guests")
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
        description = "Read the finished-task list for one node. Filters: limit, errors (only failures), since (UNIX epoch), type (task type, e.g. vzdump).",
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
        description = "List QEMU/KVM virtual machines on a node. Set full=true for full status of running VMs (per-VM blockstat is omitted; use proxmox_qemu_status for it). To find a VM by name across the cluster, use proxmox_guest_find.",
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

    // ------------------------------------------------------------------
    // Pipeline tests — exercise the full path through a wiremock server:
    // domain fn → ProxmoxClient (HTTP + data-envelope unwrap) → slim_value.
    // ------------------------------------------------------------------

    use crate::config::Connection;
    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mock_client(uri: &str) -> ProxmoxClient {
        ProxmoxClient::new(Connection {
            url: uri.to_string(),
            token: "root@pam!mcp=secret".to_string(),
            insecure: false,
        })
        .unwrap()
    }

    fn mock_server(uri: &str) -> ProxmoxMcpServer {
        ProxmoxMcpServer::new(Connection {
            url: uri.to_string(),
            token: "root@pam!mcp=secret".to_string(),
            insecure: false,
        })
        .unwrap()
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

    #[tokio::test]
    async fn pipeline_node_status_unwraps_and_slims() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "uptime": 1000, "cpu": 0.05, "lock": null }
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = nodes::NodeParams {
            node: "pve1".to_string(),
        };
        let raw = nodes::node_status(&client, p).await.unwrap();
        let result = slim_value(raw);

        // Envelope unwrapped to the inner object, and the null `lock` is gone.
        assert_eq!(result["uptime"], json!(1000));
        assert!(result.get("lock").is_none());
        assert_no_nulls(&result, "root");
    }

    #[tokio::test]
    async fn pipeline_qemu_config_interpolates_node_and_vmid() {
        let server = MockServer::start().await;
        // Mounted on the exact interpolated path; a wrong path 404s and unwrap fails.
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/qemu/100/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "name": "web01", "cores": 2 }
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = nodes::GuestParams {
            node: "pve1".to_string(),
            vmid: 100,
        };
        let result = nodes::qemu_config(&client, p).await.unwrap();
        assert_eq!(result["name"], json!("web01"));
    }

    #[tokio::test]
    async fn pipeline_qemu_list_sends_full_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/qemu"))
            .and(query_param("full", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = nodes::QemuListParams {
            node: "pve1".to_string(),
            full: Some(true),
        };
        // The bool is serialized to Proxmox's `1`; mismatch would 404 and fail.
        assert!(nodes::qemu_list(&client, p).await.is_ok());
    }

    #[tokio::test]
    async fn pipeline_storage_content_interpolates_two_segments() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/storage/local-zfs/content"))
            .and(query_param("content", "images"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = nodes::StorageContentParams {
            node: "pve1".to_string(),
            storage: "local-zfs".to_string(),
            content: Some("images".to_string()),
            vmid: None,
        };
        assert!(nodes::storage_content(&client, p).await.is_ok());
    }

    #[tokio::test]
    async fn pipeline_cluster_resources_passes_type_filter() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cluster/resources"))
            .and(query_param("type", "vm"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "vmid": 100, "type": "qemu", "template": null }]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = cluster::ClusterResourcesParams {
            r#type: Some("vm".to_string()),
        };
        let result = slim_value(cluster::cluster_resources(&client, p).await.unwrap());
        assert_eq!(result[0]["vmid"], json!(100));
        assert_no_nulls(&result, "root");
    }

    #[tokio::test]
    async fn pipeline_qemu_list_strips_blockstat() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/qemu"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "vmid": 100, "name": "web01", "blockstat": { "scsi0": { "rd_bytes": 1 } } }
                ]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = nodes::QemuListParams {
            node: "pve1".to_string(),
            full: Some(true),
        };
        let result = nodes::qemu_list(&client, p).await.unwrap();
        assert_eq!(result[0]["vmid"], json!(100));
        // The heavy blockstat blob is gone; the useful fields remain.
        assert!(result[0].get("blockstat").is_none());
        assert_eq!(result[0]["name"], json!("web01"));
    }

    #[tokio::test]
    async fn pipeline_cluster_tasks_filters_client_side() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cluster/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                { "upid": "a", "node": "pve1", "starttime": 100, "status": "OK" },
                { "upid": "b", "node": "pve2", "starttime": 200, "status": "some error" },
                { "upid": "c", "node": "pve1", "starttime": 300, "status": "OK" },
            ]})))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());

        // node filter
        let p = cluster::ClusterTasksParams {
            limit: None,
            errors: None,
            since: None,
            node: Some("pve1".to_string()),
        };
        let r = cluster::cluster_tasks(&client, p).await.unwrap();
        assert_eq!(r.as_array().unwrap().len(), 2);

        // errors filter keeps only the non-OK task
        let p = cluster::ClusterTasksParams {
            limit: None,
            errors: Some(true),
            since: None,
            node: None,
        };
        let r = cluster::cluster_tasks(&client, p).await.unwrap();
        assert_eq!(r.as_array().unwrap().len(), 1);
        assert_eq!(r[0]["upid"], json!("b"));

        // since + limit
        let p = cluster::ClusterTasksParams {
            limit: Some(1),
            errors: None,
            since: Some(200),
            node: None,
        };
        let r = cluster::cluster_tasks(&client, p).await.unwrap();
        assert_eq!(r.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pipeline_guest_find_filters_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cluster/resources"))
            .and(query_param("type", "vm"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                { "vmid": 100, "name": "web01", "node": "pve1" },
                { "vmid": 101, "name": "db01", "node": "pve2" },
            ]})))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = cluster::GuestFindParams {
            name: Some("WEB".to_string()),
        };
        let r = cluster::guest_find(&client, p).await.unwrap();
        assert_eq!(r.as_array().unwrap().len(), 1);
        assert_eq!(r[0]["vmid"], json!(100));
        assert_eq!(r[0]["node"], json!("pve1"));
    }

    #[tokio::test]
    async fn server_tool_returns_success_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/version"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"release": "8"}})),
            )
            .mount(&server)
            .await;

        let mcp = mock_server(&server.uri());
        let result = mcp.proxmox_version().await.unwrap();
        assert_ne!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn server_tool_returns_tool_error_on_api_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/ghost/status"))
            .respond_with(ResponseTemplate::new(500).set_body_string("no such node"))
            .mount(&server)
            .await;

        let mcp = mock_server(&server.uri());
        let p = nodes::NodeParams {
            node: "ghost".to_string(),
        };
        // A failed API call surfaces as a tool error, not a transport-level Err.
        let result = mcp.proxmox_node_status(Parameters(p)).await.unwrap();
        assert_eq!(result.is_error, Some(true));
    }
}
