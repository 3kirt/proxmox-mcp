use crate::client::{ProxmoxClient, ProxmoxError};
use crate::config::Connection;
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler,
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::*,
    service::{NotificationContext, RequestContext},
    tool, tool_handler, tool_router,
};
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};

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

/// A Proxmox **node** name. This newtype exists so the parameter description —
/// otherwise duplicated across six `*Params` structs — lives in exactly one
/// place: its [`schemars::JsonSchema`] impl below. It is `#[serde(transparent)]`
/// over a plain string (wire shape unchanged) and `Deref`s to `str`, so the
/// existing `encode_seg(&p.node)` call sites keep working via deref coercion.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl std::ops::Deref for NodeId {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl schemars::JsonSchema for NodeId {
    // Inline the schema at each use site so the description renders on the field
    // itself rather than behind a `$ref`.
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NodeId".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Cluster node name (see proxmox_nodes_list)"
        })
    }
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

    /// Append a boolean flag as Proxmox's `1`/`0` if `v` is Some.
    pub fn flag(self, key: &'static str, v: Option<bool>) -> Self {
        self.opt(key, v.map(|b| b as i32))
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

/// Numeric severity ordering for MCP logging levels, used to decide whether a
/// message clears the client-requested threshold.
fn level_severity(level: LoggingLevel) -> u8 {
    match level {
        LoggingLevel::Debug => 0,
        LoggingLevel::Info => 1,
        LoggingLevel::Notice => 2,
        LoggingLevel::Warning => 3,
        LoggingLevel::Error => 4,
        LoggingLevel::Critical => 5,
        LoggingLevel::Alert => 6,
        LoggingLevel::Emergency => 7,
    }
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
    let Some(summary) = tool.and_then(|t| expected_fields_summary(&t.input_schema)) else {
        return error;
    };
    McpError::invalid_params(
        format!("{}. Expected fields: {}", error.message, summary),
        error.data,
    )
}

/// Run a domain function and convert the Result into an MCP response.
macro_rules! respond {
    ($self:expr, $domain_fn:path, $p:expr, $noun:literal) => {{
        let client = $self.get_client();
        $self.respond($domain_fn(client, $p).await, $noun).await
    }};
}

// --------------------------------------------------------------------------
// Server struct
// --------------------------------------------------------------------------

/// The MCP server — holds a Proxmox client plus the connected peer and the
/// client-requested log level, so tool failures can be surfaced as MCP log
/// notifications.
#[derive(Clone)]
pub struct ProxmoxMcpServer {
    client: ProxmoxClient,
    /// Held so the `call_tool` override can look up a tool's schema to enrich
    /// invalid-params errors; reused per call rather than rebuilt each time.
    tool_router: ToolRouter<ProxmoxMcpServer>,
    /// Set once the client connects (`on_initialized`/`set_level`); `None`
    /// before then, which makes `send_log` a no-op (e.g. in unit tests).
    peer: Arc<OnceLock<Peer<RoleServer>>>,
    /// Minimum level the client wants delivered (default Warning until it sends
    /// `logging/setLevel`).
    log_level: Arc<Mutex<LoggingLevel>>,
}

impl ProxmoxMcpServer {
    pub fn new(conn: Connection) -> anyhow::Result<Self> {
        Ok(Self {
            client: ProxmoxClient::new(conn)?,
            tool_router: Self::tool_router(),
            peer: Arc::new(OnceLock::new()),
            log_level: Arc::new(Mutex::new(LoggingLevel::Warning)),
        })
    }

    fn get_client(&self) -> &ProxmoxClient {
        &self.client
    }

    /// Send an MCP `notifications/message` to the client if a peer is connected
    /// and `level` meets the client-requested threshold. Best-effort: a delivery
    /// failure is swallowed rather than masking the tool result it accompanies.
    async fn send_log(&self, level: LoggingLevel, message: &str) {
        let current = *self.log_level.lock().unwrap();
        if level_severity(level) >= level_severity(current)
            && let Some(peer) = self.peer.get()
        {
            let _ = peer
                .notify_logging_message(LoggingMessageNotificationParam {
                    level,
                    logger: Some("proxmox-mcp".to_string()),
                    data: serde_json::json!({ "message": message }),
                })
                .await;
        }
    }

    /// Convert a domain-call result into an MCP response: the payload on
    /// success, or a `"{noun}: {error}"` tool error on failure. The error path
    /// also emits an `Error`-level MCP log so the client sees why a call failed.
    async fn respond(
        &self,
        result: Result<Value, ProxmoxError>,
        noun: &str,
    ) -> Result<CallToolResult, McpError> {
        match result {
            Ok(v) => json_result(v),
            Err(e) => {
                let msg = format!("{noun}: {}", e.to_tool_message());
                self.send_log(LoggingLevel::Error, &msg).await;
                tool_error(&msg)
            }
        }
    }

    /// Shared body for the zero-parameter "GET this fixed path" tools.
    async fn get_simple(&self, path: &str, noun: &str) -> Result<CallToolResult, McpError> {
        self.respond(self.client.get(path, &[]).await, noun).await
    }
}

// --------------------------------------------------------------------------
// Tool shims — one per endpoint
// --------------------------------------------------------------------------

#[tool_router]
impl ProxmoxMcpServer {
    // ---- cluster / global ----
    #[tool(
        description = "Get the Proxmox VE API version and basic datacenter info. Doubles as a quick connectivity/health check that the server is reachable and the token works.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_version_get(&self) -> Result<CallToolResult, McpError> {
        self.get_simple("/version", "getting version").await
    }

    #[tool(
        description = "Get cluster health: quorum state, member nodes, and cluster name. Use this to check whether the cluster is quorate and every node is online.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_status_get(&self) -> Result<CallToolResult, McpError> {
        self.get_simple("/cluster/status", "getting cluster status")
            .await
    }

    #[tool(
        description = "List the entire cluster inventory in one call — every VM, container, storage, and node, with live CPU/memory/disk usage. The best starting point for \"what's running\" or for locating where a guest lives. Optional type filter: vm, storage, node, sdn. To search by guest name, use proxmox_guests_find.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_resources_list(
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
        description = "List recent tasks (jobs/operations — backups, migrations, snapshots, start/stop) across the whole cluster, most recent first. Use this for \"what happened recently\" or to hunt failures. Filters: limit (default 50), errors (only failures), since (UNIX epoch), node. For a single node, use proxmox_nodes_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_tasks_list(
        &self,
        Parameters(p): Parameters<cluster::ClusterTasksParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, cluster::cluster_tasks, p, "listing cluster tasks")
    }

    #[tool(
        description = "Find VMs/containers anywhere in the cluster by name (case-insensitive substring), resolving each to its node and vmid. Omit name to list every guest cluster-wide. Use this to turn a hostname into the node+vmid that the per-VM tools require.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_guests_find(
        &self,
        Parameters(p): Parameters<cluster::GuestFindParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, cluster::guest_find, p, "finding guests")
    }

    // ---- nodes ----
    #[tool(
        description = "List the cluster's nodes (the physical hosts/servers running Proxmox) with status, CPU, and memory.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_list(&self) -> Result<CallToolResult, McpError> {
        self.get_simple("/nodes", "listing nodes").await
    }

    #[tool(
        description = "Get detailed status for one node (a physical host): CPU, memory, load average, uptime, and kernel. Node names come from proxmox_nodes_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_status_get(
        &self,
        Parameters(p): Parameters<nodes::NodeParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::node_status, p, "getting node status")
    }

    #[tool(
        description = "List recent tasks (jobs/operations: backups/vzdump, migrations, start/stop) that ran on one node, most recent first. Filters: limit, errors (only failures), since (UNIX epoch), type (e.g. vzdump for backups). For the whole cluster, use proxmox_cluster_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_tasks_list(
        &self,
        Parameters(p): Parameters<nodes::NodeTasksParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::node_tasks, p, "listing node tasks")
    }

    // ---- QEMU VMs ----
    #[tool(
        description = "List QEMU/KVM virtual machines (VMs) on a node. Set full=true for live status of running VMs (per-VM blockstat is omitted; use proxmox_qemu_status_get for it). To find a VM by name across the cluster, use proxmox_guests_find.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_list(
        &self,
        Parameters(p): Parameters<nodes::QemuListParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_list, p, "listing VMs")
    }

    #[tool(
        description = "Get a QEMU VM's configuration — its hardware and settings: cores, memory, disks, network, boot order (current values plus pending changes). Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_config_get(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_config, p, "getting VM config")
    }

    #[tool(
        description = "Get a QEMU VM's current runtime status: whether it's running, plus live CPU, memory, and uptime. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_status_get(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_status, p, "getting VM status")
    }

    // ---- LXC containers ----
    #[tool(
        description = "List LXC containers (CTs) on a node. To find a container by name across the cluster, use proxmox_guests_find.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_list(
        &self,
        Parameters(p): Parameters<nodes::NodeParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_list, p, "listing containers")
    }

    #[tool(
        description = "Get an LXC container's configuration — its resources and settings: cores, memory, rootfs/disks, network. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_config_get(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_config, p, "getting container config")
    }

    #[tool(
        description = "Get an LXC container's current runtime status: whether it's running, plus live CPU, memory, and uptime. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_status_get(
        &self,
        Parameters(p): Parameters<nodes::GuestParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_status, p, "getting container status")
    }

    // ---- storage ----
    #[tool(
        description = "List storage (datastores) on a node — where VM disks, ISOs, and backups live — with capacity and free space. Use this for \"how much disk space is left\". Filters: content (e.g. images, iso, backup), enabled.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_storage_list(
        &self,
        Parameters(p): Parameters<nodes::StorageListParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::storage_list, p, "listing storage")
    }

    #[tool(
        description = "List what's stored on one storage on a node: VM disk images, ISOs, backups (vzdump), and container templates. Filter by content type, or by vmid to find a specific guest's disks/backups. Storage IDs come from proxmox_storage_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_storage_content_list(
        &self,
        Parameters(p): Parameters<nodes::StorageContentParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::storage_content, p, "listing storage content")
    }

    // ---- network ----
    #[tool(
        description = "List the network interfaces, bridges, bonds, and VLANs on a node. Use this to discover which bridges (e.g. vmbr0) are available — for example when choosing a network for a VM. Optional type filter: bridge, bond, eth, vlan, OVSBridge, etc.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_network_list(
        &self,
        Parameters(p): Parameters<nodes::NetworkListParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::network_list, p, "listing node network")
    }

    #[tool(
        description = "Get the configuration of one network interface on a node (addressing, bridge ports, bond members, VLAN tag). Interface names come from proxmox_nodes_network_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_network_get(
        &self,
        Parameters(p): Parameters<nodes::NetworkInterfaceParams>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::network_get, p, "getting network interface")
    }
}

// --------------------------------------------------------------------------
// ServerHandler
// --------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for ProxmoxMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_logging()
                .build(),
        )
        .with_server_info(Implementation::new(
            "proxmox-mcp",
            env!("CARGO_PKG_VERSION"),
        ));
        info.instructions = Some(
            "Read-only access to a Proxmox VE cluster (every tool is a GET; nothing is \
             modified). Start with proxmox_cluster_resources_list for a one-call inventory of \
             all VMs, containers, storage, and nodes. To inspect a guest you know only by name, \
             first call proxmox_guests_find to resolve it to a node + vmid — the per-guest tools \
             (proxmox_qemu_config_get / proxmox_qemu_status_get and their proxmox_lxc_* \
             equivalents) require both. Epoch timestamp fields are returned alongside an ISO \
             8601 <field>_iso sibling."
                .to_string(),
        );
        info
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let _ = self.peer.set(context.peer);
    }

    /// Dispatch via the stored router, then enrich any invalid-params error with
    /// the tool's accepted fields so the caller can self-correct.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool_name = request.name.clone();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        match self.tool_router.call(tcc).await {
            Err(e) if e.code == ErrorCode::INVALID_PARAMS => {
                Err(enrich_invalid_params(e, self.tool_router.get(&tool_name)))
            }
            other => other,
        }
    }

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        *self.log_level.lock().unwrap() = request.level;
        let _ = self.peer.set(context.peer);
        Ok(())
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

    #[test]
    fn level_severity_orders_low_to_high() {
        // send_log compares these, so the ordering must be monotonic: a message
        // is delivered only when its level is at least the client's threshold.
        assert!(level_severity(LoggingLevel::Debug) < level_severity(LoggingLevel::Info));
        assert!(level_severity(LoggingLevel::Info) < level_severity(LoggingLevel::Warning));
        assert!(level_severity(LoggingLevel::Warning) < level_severity(LoggingLevel::Error));
        assert!(level_severity(LoggingLevel::Error) < level_severity(LoggingLevel::Critical));
        assert!(level_severity(LoggingLevel::Critical) < level_severity(LoggingLevel::Emergency));
    }

    #[test]
    fn expected_fields_summary_lists_required_first() {
        let router = ProxmoxMcpServer::tool_router();
        let tool = router.get("proxmox_qemu_config_get").unwrap();
        let summary = expected_fields_summary(&tool.input_schema).unwrap();
        assert!(summary.contains("node (required)"), "{summary}");
        assert!(summary.contains("vmid (required)"), "{summary}");
    }

    #[test]
    fn enrich_invalid_params_appends_expected_fields() {
        let router = ProxmoxMcpServer::tool_router();
        let err = McpError::invalid_params(
            "failed to deserialize parameters: missing field `vmid`",
            None,
        );
        let enriched = enrich_invalid_params(err, router.get("proxmox_qemu_config_get"));
        assert!(
            enriched.message.contains("missing field `vmid`"),
            "{}",
            enriched.message
        );
        assert!(
            enriched.message.contains("Expected fields: ")
                && enriched.message.contains("node (required)"),
            "{}",
            enriched.message
        );
    }

    #[test]
    fn enrich_invalid_params_without_tool_keeps_error_unchanged() {
        let err = McpError::invalid_params("failed to deserialize parameters", None);
        let enriched = enrich_invalid_params(err, None);
        assert_eq!(enriched.message, "failed to deserialize parameters");
    }

    #[test]
    fn node_id_renders_description_inline() {
        // The NodeId newtype carries the parameter description in one place;
        // verify it reaches the per-tool input schema inline (not behind a
        // `$ref`, which inline_schema() prevents) so LLM callers still see it.
        let router = ProxmoxMcpServer::tool_router();
        let tool = router.get("proxmox_nodes_status_get").unwrap();
        let node = tool
            .input_schema
            .get("properties")
            .unwrap()
            .get("node")
            .unwrap();
        assert!(node.get("$ref").is_none(), "node must be inlined: {node}");
        assert_eq!(node["type"], "string");
        assert!(
            node["description"]
                .as_str()
                .unwrap()
                .contains("proxmox_nodes_list"),
            "{node}"
        );
    }

    #[test]
    fn node_id_deserializes_transparently_from_string() {
        // The wire contract is unchanged: a plain JSON string still deserializes
        // into the newtype, and it derefs back to that string.
        let p: nodes::NodeParams = serde_json::from_value(json!({ "node": "pve1" })).unwrap();
        assert_eq!(&*p.node, "pve1");
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
            node: "pve1".into(),
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
            node: "pve1".into(),
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
            node: "pve1".into(),
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
            node: "pve1".into(),
            storage: "local-zfs".to_string(),
            content: Some("images".to_string()),
            vmid: None,
        };
        assert!(nodes::storage_content(&client, p).await.is_ok());
    }

    #[tokio::test]
    async fn pipeline_network_list_passes_type_filter() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/network"))
            .and(query_param("type", "bridge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "iface": "vmbr0", "type": "bridge", "comments": null }]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = nodes::NetworkListParams {
            node: "pve1".into(),
            r#type: Some("bridge".to_string()),
        };
        let result = slim_value(nodes::network_list(&client, p).await.unwrap());
        assert_eq!(result[0]["iface"], json!("vmbr0"));
        assert_no_nulls(&result, "root");
    }

    #[tokio::test]
    async fn pipeline_network_get_interpolates_iface() {
        let server = MockServer::start().await;
        // Mounted on the exact interpolated path; a wrong path 404s and unwrap fails.
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/network/vmbr0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "iface": "vmbr0", "type": "bridge", "bridge_ports": "eth0" }
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let p = nodes::NetworkInterfaceParams {
            node: "pve1".into(),
            iface: "vmbr0".to_string(),
        };
        let result = nodes::network_get(&client, p).await.unwrap();
        assert_eq!(result["bridge_ports"], json!("eth0"));
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
            node: "pve1".into(),
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
        let result = mcp.proxmox_version_get().await.unwrap();
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
            node: "ghost".into(),
        };
        // A failed API call surfaces as a tool error, not a transport-level Err.
        let result = mcp.proxmox_nodes_status_get(Parameters(p)).await.unwrap();
        assert_eq!(result.is_error, Some(true));
    }
}
