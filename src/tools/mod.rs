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
use tokio::task::JoinHandle;

mod slim;
use slim::{humanize_value, slim_value};

pub mod cluster;
pub mod nodes;
pub mod scope;
use scope::{ALL_CLUSTERS, AnyScoped, NoParams, Scoped};

// --------------------------------------------------------------------------
// Shared helpers
// --------------------------------------------------------------------------

pub fn json_result(v: Value) -> Result<CallToolResult, McpError> {
    let v = slim_value(humanize_value(v));
    let text = serde_json::to_string_pretty(&v)
        .map_err(|e| McpError::internal_error(format!("marshalling response: {e}"), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

// Mirrors `json_result`'s fallible signature so both arms of `respond` share one
// `Result<CallToolResult, McpError>` shape; the error variant is infallible here.
#[allow(clippy::unnecessary_wraps)]
pub fn tool_error(msg: &str) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(msg)]))
}

/// Convert a domain-call result into an MCP response: the payload on success, or
/// a `"{noun}: {error}"` tool error on failure. The error is also logged via
/// `tracing` (operator-facing, through the `--debug` / `--log-file` pipeline); it
/// is still returned in-band as the tool result.
pub fn respond(
    result: Result<Value, ProxmoxError>,
    noun: &str,
) -> Result<CallToolResult, McpError> {
    match result {
        Ok(v) => json_result(v),
        Err(e) => {
            let msg = format!("{noun}: {}", e.to_tool_message());
            tracing::error!("{msg}");
            tool_error(&msg)
        }
    }
}

/// Percent-encode a single URL path segment. Encodes everything except the
/// RFC 3986 unreserved set, so user-supplied node/storage names cannot inject
/// extra path components (`/`, `..`) or break the request URL.
pub fn encode_seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
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

/// A Proxmox task ID (`UPID:{node}:{pid}:{pstart}:{starttime}:{type}:{id}:{user}:`).
/// Validated on deserialization so a malformed ID is an invalid-params error,
/// and it carries its own node, so task tools need no separate `node` argument.
#[derive(Debug, Clone)]
pub struct Upid(String);

impl Upid {
    fn parse(s: String) -> Result<Self, String> {
        let mut parts = s.split(':');
        let is_upid = parts.next() == Some("UPID");
        let has_node = parts.next().is_some_and(|n| !n.is_empty());
        if is_upid && has_node {
            Ok(Self(s))
        } else {
            Err(format!(
                "invalid task UPID {s:?}: expected UPID:<node>:... as returned by \
                 proxmox_cluster_tasks_list / proxmox_nodes_tasks_list"
            ))
        }
    }

    /// The node that ran the task (the UPID's second field).
    pub fn node(&self) -> &str {
        self.0.split(':').nth(1).unwrap_or_default()
    }
}

impl std::ops::Deref for Upid {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for Upid {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for Upid {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Upid".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Task ID (UPID), e.g. UPID:pve1:0000ABCD:...:vzdump:100:root@pam: — the `upid` field from proxmox_cluster_tasks_list or proxmox_nodes_tasks_list"
        })
    }
}

/// Fluent builder for optional query parameters.
pub struct QueryBuilder {
    params: Vec<(&'static str, String)>,
}

impl QueryBuilder {
    pub const fn new() -> Self {
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
        self.opt(key, v.map(i32::from))
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

/// Resolve the requested cluster, run a domain function against it, and convert
/// the Result into an MCP response. An unknown cluster is an in-band tool error.
macro_rules! respond {
    ($self:expr, $domain_fn:path, $cluster:expr, $p:expr, $noun:literal) => {{
        match $self.client_for($cluster) {
            Ok((name, client)) => $self.respond_in(name, $domain_fn(client, $p).await, $noun),
            Err(msg) => $crate::tools::tool_error(&msg),
        }
    }};
    ($self:expr, $domain_fn:path, $args:expr, $noun:literal) => {{
        let Scoped { cluster, inner } = $args;
        respond!($self, $domain_fn, cluster.as_deref(), inner, $noun)
    }};
}

/// Like `respond!`, but `cluster: "*"` runs the domain fn against every
/// cluster concurrently and merges the (array) results under `$key`.
macro_rules! respond_any {
    ($self:expr, $domain_fn:path, $args:expr, $noun:literal, $key:literal) => {{
        let AnyScoped { cluster, inner } = $args;
        if cluster.as_deref() == Some(ALL_CLUSTERS) {
            let handles = $self
                .clusters
                .entries
                .iter()
                .map(|(name, entry)| {
                    let client = entry.client.clone();
                    let p = inner.clone();
                    let handle = tokio::spawn(async move { $domain_fn(&client, p).await });
                    (name.clone(), handle)
                })
                .collect();
            $self.merge_all(handles, $noun, $key).await
        } else {
            respond!($self, $domain_fn, cluster.as_deref(), inner, $noun)
        }
    }};
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

    /// `respond`, with the cluster named in any error once there is more than
    /// one to tell apart.
    fn respond_in(
        &self,
        cluster: &str,
        result: Result<Value, ProxmoxError>,
        noun: &str,
    ) -> Result<CallToolResult, McpError> {
        if self.is_multi_cluster() {
            respond(result, &format!("[{cluster}] {noun}"))
        } else {
            respond(result, noun)
        }
    }

    /// Shared body for the "GET this fixed path" tools that take only `cluster`.
    async fn get_simple(
        &self,
        args: Scoped<NoParams>,
        path: &str,
        noun: &str,
    ) -> Result<CallToolResult, McpError> {
        match self.client_for(args.cluster.as_deref()) {
            Ok((name, client)) => self.respond_in(name, client.get(path, &[]).await, noun),
            Err(msg) => tool_error(&msg),
        }
    }

    /// Merge per-cluster array results from a `cluster: "*"` call into
    /// `{ <key>: [...each item tagged with "cluster"], "unreachable": {name: error} }`.
    /// Fails only if every cluster failed.
    async fn merge_all(
        &self,
        handles: Vec<(String, JoinHandle<Result<Value, ProxmoxError>>)>,
        noun: &str,
        key: &str,
    ) -> Result<CallToolResult, McpError> {
        let total = handles.len();
        let mut items = Vec::new();
        let mut unreachable = serde_json::Map::new();
        for (name, handle) in handles {
            let data = match handle.await {
                Ok(Ok(data)) => data,
                Ok(Err(e)) => {
                    let msg = e.to_tool_message();
                    tracing::error!("[{name}] {noun}: {msg}");
                    unreachable.insert(name, Value::String(msg));
                    continue;
                }
                Err(e) => {
                    tracing::error!("[{name}] {noun}: task failed: {e}");
                    unreachable.insert(name, Value::String(format!("internal error: {e}")));
                    continue;
                }
            };
            match data {
                Value::Array(list) => items.extend(list.into_iter().map(|mut item| {
                    if let Value::Object(map) = &mut item {
                        map.insert("cluster".to_string(), Value::String(name.clone()));
                    }
                    item
                })),
                other => items.push(json!({ "cluster": name, "data": other })),
            }
        }
        if total > 0 && unreachable.len() == total {
            return tool_error(&format!(
                "{noun}: every cluster failed: {}",
                Value::Object(unreachable)
            ));
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
        respond_any!(
            self,
            cluster::cluster_resources,
            p,
            "listing cluster resources",
            "resources"
        )
    }

    #[tool(
        description = "List recent tasks (jobs/operations — backups, migrations, snapshots, start/stop) across the whole cluster, most recent first. Use this for \"what happened recently\" or to hunt failures. Filters: limit (default 50), errors (only failures), since (UNIX epoch), node. For a single node, use proxmox_nodes_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_cluster_tasks_list(
        &self,
        Parameters(p): Parameters<Scoped<cluster::ClusterTasksParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, cluster::cluster_tasks, p, "listing cluster tasks")
    }

    #[tool(
        description = "Find VMs/containers anywhere in the cluster by name (case-insensitive substring), resolving each to its node and vmid. Omit name to list every guest cluster-wide. Use this to turn a hostname into the node+vmid that the per-VM tools require. With cluster \"*\", searches every configured cluster and returns {guests: [...each tagged with cluster], unreachable: {cluster: error}}; pass a match's cluster to the follow-up per-guest tools.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_guests_find(
        &self,
        Parameters(p): Parameters<AnyScoped<cluster::GuestFindParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond_any!(self, cluster::guest_find, p, "finding guests", "guests")
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
        respond!(self, nodes::node_status, p, "getting node status")
    }

    #[tool(
        description = "List recent tasks (jobs/operations: backups/vzdump, migrations, start/stop) that ran on one node, most recent first. Filters: limit, errors (only failures), since (UNIX epoch), type (e.g. vzdump for backups). For the whole cluster, use proxmox_cluster_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_tasks_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NodeTasksParams>>,
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
        Parameters(p): Parameters<Scoped<nodes::QemuListParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_list, p, "listing VMs")
    }

    #[tool(
        description = "Get a QEMU VM's configuration — its hardware and settings: cores, memory, disks, network, boot order (current values plus pending changes). Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_config_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::GuestParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::qemu_config, p, "getting VM config")
    }

    #[tool(
        description = "Get a QEMU VM's current runtime status: whether it's running, plus live CPU, memory, and uptime. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_qemu_status_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::GuestParams>>,
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
        Parameters(p): Parameters<Scoped<nodes::NodeParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_list, p, "listing containers")
    }

    #[tool(
        description = "Get an LXC container's configuration — its resources and settings: cores, memory, rootfs/disks, network. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_config_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::GuestParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::lxc_config, p, "getting container config")
    }

    #[tool(
        description = "Get an LXC container's current runtime status: whether it's running, plus live CPU, memory, and uptime. Needs node + vmid; if you only have a name, call proxmox_guests_find first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_lxc_status_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::GuestParams>>,
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
        Parameters(p): Parameters<Scoped<nodes::StorageListParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::storage_list, p, "listing storage")
    }

    #[tool(
        description = "List what's stored on one storage on a node: VM disk images, ISOs, backups (vzdump), and container templates. Filter by content type, or by vmid to find a specific guest's disks/backups. Storage IDs come from proxmox_storage_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_storage_content_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::StorageContentParams>>,
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
        Parameters(p): Parameters<Scoped<nodes::NetworkListParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::network_list, p, "listing node network")
    }

    #[tool(
        description = "Get the configuration of one network interface on a node (addressing, bridge ports, bond members, VLAN tag). Interface names come from proxmox_nodes_network_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_nodes_network_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NetworkInterfaceParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::network_get, p, "getting network interface")
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
        respond!(self, nodes::task_status, p, "getting task status")
    }

    #[tool(
        description = "Read a task's log to find out what it did or why it failed (backups, migrations, start/stop). By default returns the LAST 50 lines, where errors usually are; pass start (0-based) to page from a given line. Takes the task's upid from proxmox_cluster_tasks_list or proxmox_nodes_tasks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_tasks_log_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::TaskLogParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::task_log, p, "reading task log")
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
        respond!(self, nodes::qemu_snapshots, p, "listing VM snapshots")
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
        respond!(self, nodes::disks_list, p, "listing disks")
    }

    #[tool(
        description = "Get SMART data for one disk on a node: the overall health verdict plus every SMART attribute (reallocated sectors, temperature, power-on hours, NVMe wear...). Set healthonly=true for just the verdict. The disk path comes from proxmox_disks_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_disks_smart_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::DiskSmartParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::disk_smart, p, "getting SMART data")
    }

    #[tool(
        description = "List a node's ZFS pools with size, allocated/free space, fragmentation, dedup ratio, and health (ONLINE, DEGRADED, FAULTED...). For one pool's vdev tree, errors, and scrub status, use proxmox_disks_zfs_get.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_disks_zfs_list(
        &self,
        Parameters(p): Parameters<Scoped<nodes::NodeParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::zfs_list, p, "listing ZFS pools")
    }

    #[tool(
        description = "Get detailed status of one ZFS pool on a node (like `zpool status`): state, last scrub/resilver result and progress, read/write/checksum errors, recommended action, and the vdev tree down to each disk. Pool names come from proxmox_disks_zfs_list.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn proxmox_disks_zfs_get(
        &self,
        Parameters(p): Parameters<Scoped<nodes::ZfsPoolParams>>,
    ) -> Result<CallToolResult, McpError> {
        respond!(self, nodes::zfs_get, p, "getting ZFS pool status")
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
        respond!(self, nodes::replication_list, p, "listing replication jobs")
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
             (proxmox_qemu_config_get / proxmox_qemu_status_get and their proxmox_lxc_* \
             equivalents) require both. To find out why a task (backup, migration, start/stop) \
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
                "{} must declare read_only_hint = true",
                tool.name
            );
            assert_eq!(
                ann.open_world_hint,
                Some(false),
                "{} must declare open_world_hint = false",
                tool.name
            );
        }
    }

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
    fn enrich_argument_error_appends_fields_to_in_band_argument_errors() {
        let router = ProxmoxMcpServer::tool_router();
        let result = CallToolResult::error(vec![ContentBlock::text(
            "failed to deserialize parameters: missing field `vmid`",
        )]);
        let enriched = enrich_argument_error(result, router.get("proxmox_qemu_config_get"));
        let text = &enriched.content[0].as_text().unwrap().text;
        assert!(text.contains("missing field `vmid`"), "{text}");
        assert!(
            text.contains("Expected fields: ") && text.contains("vmid (required)"),
            "{text}"
        );
    }

    #[test]
    fn enrich_argument_error_leaves_other_results_alone() {
        let router = ProxmoxMcpServer::tool_router();
        let tool = router.get("proxmox_qemu_config_get");
        // An ordinary tool error (e.g. a Proxmox API failure) is not an argument error.
        let api_err = CallToolResult::error(vec![ContentBlock::text("getting VM config: 500")]);
        let text = enrich_argument_error(api_err, tool).content[0]
            .as_text()
            .unwrap()
            .text
            .clone();
        assert_eq!(text, "getting VM config: 500");
        // Success with prefix-like text stays untouched too.
        let ok = CallToolResult::success(vec![ContentBlock::text(
            "failed to deserialize parameters: but this is data",
        )]);
        let text = enrich_argument_error(ok, tool).content[0]
            .as_text()
            .unwrap()
            .text
            .clone();
        assert!(!text.contains("Expected fields"), "{text}");
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
        mock_multi(&[("default", uri)], "default")
    }

    fn mock_multi(clusters: &[(&str, &str)], default: &str) -> ProxmoxMcpServer {
        let entries = clusters
            .iter()
            .map(|(name, uri)| {
                (
                    (*name).to_string(),
                    Connection {
                        url: (*uri).to_string(),
                        token: format!("root@pam!mcp=secret-{name}"),
                        insecure: false,
                    },
                )
            })
            .collect();
        ProxmoxMcpServer::new(crate::config::Clusters {
            default: default.to_string(),
            entries,
        })
        .unwrap()
    }

    /// Build tool args the way rmcp does: deserialize the JSON arguments object.
    fn args<T: serde::de::DeserializeOwned>(v: Value) -> Parameters<T> {
        Parameters(serde_json::from_value(v).unwrap())
    }

    fn text_of(result: &CallToolResult) -> &str {
        &result.content[0].as_text().unwrap().text
    }

    async fn mount_version(server: &MockServer, release: &str) {
        Mock::given(method("GET"))
            .and(path("/version"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"release": release}})),
            )
            .mount(server)
            .await;
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
        let result = mcp.proxmox_version_get(args(json!({}))).await.unwrap();
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
        // A failed API call surfaces as a tool error, not a transport-level Err.
        let result = mcp
            .proxmox_nodes_status_get(args(json!({ "node": "ghost" })))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        // Single-cluster error messages carry no cluster prefix.
        assert!(
            text_of(&result).starts_with("getting node status:"),
            "{}",
            text_of(&result)
        );
    }

    // ------------------------------------------------------------------
    // Multi-cluster
    // ------------------------------------------------------------------

    fn props(tool: &str) -> serde_json::Map<String, Value> {
        let router = ProxmoxMcpServer::tool_router();
        let tool = router.get(tool).unwrap();
        tool.input_schema["properties"].as_object().unwrap().clone()
    }

    #[test]
    fn scoped_schema_flattens_cluster_beside_domain_fields() {
        let router = ProxmoxMcpServer::tool_router();
        let schema = &router.get("proxmox_qemu_config_get").unwrap().input_schema;
        assert_eq!(schema["type"], "object");
        let p = schema["properties"].as_object().unwrap();
        for field in ["cluster", "node", "vmid"] {
            assert!(p.contains_key(field), "missing {field}: {schema:?}");
        }
        assert!(
            p["cluster"].get("$ref").is_none(),
            "cluster must be inlined"
        );
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
    fn every_tool_except_clusters_list_takes_cluster() {
        for tool in ProxmoxMcpServer::tool_router().list_all() {
            let has = tool
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| p.contains_key("cluster"));
            assert_eq!(
                has,
                tool.name != "proxmox_clusters_list",
                "{} cluster argument",
                tool.name
            );
        }
    }

    #[test]
    fn only_fan_out_tools_advertise_star() {
        let star = |tool: &str| {
            props(tool)["cluster"]["description"]
                .as_str()
                .unwrap()
                .contains("\"*\"")
        };
        assert!(star("proxmox_guests_find"));
        assert!(star("proxmox_cluster_resources_list"));
        assert!(!star("proxmox_qemu_config_get"));
        assert!(!star("proxmox_version_get"));
    }

    #[tokio::test]
    async fn routes_to_named_cluster_and_defaults_when_omitted() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        mount_version(&a, "from-a").await;
        mount_version(&b, "from-b").await;
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
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/qemu/100/config"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"name": "on-b"}})),
            )
            .mount(&b)
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
        let mcp = mock_multi(
            &[("a", "https://a.invalid"), ("b", "https://b.invalid")],
            "a",
        );
        let r = mcp
            .proxmox_version_get(args(json!({ "cluster": "nope" })))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text_of(&r).contains("\"nope\""), "{}", text_of(&r));
        assert!(text_of(&r).contains("a, b"), "{}", text_of(&r));
    }

    #[tokio::test]
    async fn star_is_rejected_on_single_cluster_tools() {
        let mcp = mock_multi(
            &[("a", "https://a.invalid"), ("b", "https://b.invalid")],
            "a",
        );
        let r = mcp
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
        Mock::given(method("GET"))
            .and(path("/nodes/ghost/status"))
            .respond_with(ResponseTemplate::new(500).set_body_string("no such node"))
            .mount(&a)
            .await;
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

    async fn mount_guests(server: &MockServer, guests: Value) {
        Mock::given(method("GET"))
            .and(path("/cluster/resources"))
            .and(query_param("type", "vm"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": guests })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fan_out_tags_results_and_reports_unreachable() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        let c = MockServer::start().await;
        mount_guests(
            &a,
            json!([{ "vmid": 100, "name": "web01", "node": "pve1" }]),
        )
        .await;
        mount_guests(
            &b,
            json!([
                { "vmid": 200, "name": "web02", "node": "pve1" },
                { "vmid": 201, "name": "db01", "node": "pve1" }
            ]),
        )
        .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .mount(&c)
            .await;
        let mcp = mock_multi(&[("a", &a.uri()), ("b", &b.uri()), ("c", &c.uri())], "a");

        let r = mcp
            .proxmox_guests_find(args(json!({ "cluster": "*", "name": "web" })))
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
        let out: Value = serde_json::from_str(text_of(&r)).unwrap();
        let guests = out["guests"].as_array().unwrap();
        assert_eq!(guests.len(), 2, "{out}");
        assert!(
            guests
                .contains(&json!({ "vmid": 100, "name": "web01", "node": "pve1", "cluster": "a" }))
        );
        assert!(
            guests
                .contains(&json!({ "vmid": 200, "name": "web02", "node": "pve1", "cluster": "b" }))
        );
        assert!(
            out["unreachable"]["c"].as_str().unwrap().contains("401"),
            "{out}"
        );
        assert!(out["unreachable"].get("a").is_none());
    }

    #[tokio::test]
    async fn fan_out_omits_unreachable_when_all_succeed() {
        let a = MockServer::start().await;
        mount_guests(&a, json!([{ "vmid": 100, "name": "web01" }])).await;
        let mcp = mock_server(&a.uri());
        let r = mcp
            .proxmox_guests_find(args(json!({ "cluster": "*" })))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(text_of(&r)).unwrap();
        assert_eq!(out["guests"][0]["cluster"], json!("default"));
        assert!(out.get("unreachable").is_none(), "{out}");
    }

    #[tokio::test]
    async fn fan_out_fails_when_every_cluster_fails() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        for s in [&a, &b] {
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(500).set_body_string("down"))
                .mount(s)
                .await;
        }
        let mcp = mock_multi(&[("a", &a.uri()), ("b", &b.uri())], "a");
        let r = mcp
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
        mount_guests(&a, json!([{ "vmid": 100, "name": "web01" }])).await;
        let mcp = mock_multi(&[("a", &a.uri()), ("b", "https://b.invalid")], "a");
        let r = mcp.proxmox_guests_find(args(json!({}))).await.unwrap();
        let out: Value = serde_json::from_str(text_of(&r)).unwrap();
        assert_eq!(out, json!([{ "vmid": 100, "name": "web01" }]));
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
        let single = mock_server("https://a.invalid");
        let text = single.get_info().instructions.unwrap();
        assert!(!text.contains("`cluster`"), "{text}");

        let multi = mock_multi(
            &[("a", "https://a.invalid"), ("b", "https://b.invalid")],
            "b",
        );
        let text = multi.get_info().instructions.unwrap();
        assert!(text.contains("a, b (default: b)"), "{text}");
    }

    // ------------------------------------------------------------------
    // Tasks, snapshots, backups, HA, disks, replication
    // ------------------------------------------------------------------

    const UPID: &str = "UPID:pve2:0000ABCD:00112233:66AABBCC:vzdump:100:root@pam!mcp:";

    fn task_path(suffix: &str) -> String {
        format!("/nodes/pve2/tasks/{}/{suffix}", encode_seg(UPID))
    }

    #[test]
    fn upid_reads_node_and_rejects_malformed_ids() {
        let p: nodes::TaskParams = serde_json::from_value(json!({ "upid": UPID })).unwrap();
        assert_eq!(p.upid.node(), "pve2");
        for bad in ["", "UPID", "UPID::x", "TASK:pve1:1", "pve1"] {
            let err = serde_json::from_value::<Scoped<nodes::TaskParams>>(json!({ "upid": bad }))
                .unwrap_err();
            assert!(
                err.to_string().contains("invalid task UPID"),
                "{bad:?}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn pipeline_task_status_routes_to_upid_node_with_encoded_upid() {
        let server = MockServer::start().await;
        // Mounted on the exact path: node taken from the UPID, and the UPID's
        // `:`, `@` and `!` percent-encoded into a single segment.
        Mock::given(method("GET"))
            .and(path(task_path("status")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "status": "stopped", "exitstatus": "OK" }
            })))
            .mount(&server)
            .await;

        let p = serde_json::from_value(json!({ "upid": UPID })).unwrap();
        let r = nodes::task_status(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r["exitstatus"], json!("OK"));
    }

    async fn mount_task_log(server: &MockServer, lines: usize) {
        let entries: Vec<Value> = (1..=lines)
            .map(|n| json!({ "n": n, "t": format!("line {n}") }))
            .collect();
        Mock::given(method("GET"))
            .and(path(task_path("log")))
            .and(query_param("limit", "100000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": entries })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn pipeline_task_log_defaults_to_tail() {
        let server = MockServer::start().await;
        mount_task_log(&server, 120).await;
        let p = serde_json::from_value(json!({ "upid": UPID })).unwrap();
        let r = nodes::task_log(&mock_client(&server.uri()), p)
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
    async fn pipeline_task_log_pages_from_start_and_clamps() {
        let server = MockServer::start().await;
        mount_task_log(&server, 10).await;
        let client = mock_client(&server.uri());

        let p = serde_json::from_value(json!({ "upid": UPID, "start": 2, "limit": 3 })).unwrap();
        let r = nodes::task_log(&client, p).await.unwrap();
        assert_eq!(r["lines"], json!(["line 3", "line 4", "line 5"]));

        // Past the end: empty page, not a panic.
        let p = serde_json::from_value(json!({ "upid": UPID, "start": 50 })).unwrap();
        let r = nodes::task_log(&client, p).await.unwrap();
        assert_eq!(r["lines"], json!([]));

        // Short log with the default tail: everything.
        let p = serde_json::from_value(json!({ "upid": UPID })).unwrap();
        let r = nodes::task_log(&client, p).await.unwrap();
        assert_eq!(r["start"], json!(0));
        assert_eq!(r["lines"].as_array().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn pipeline_qemu_snapshots_interpolates_node_and_vmid() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/qemu/100/snapshot"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "name": "pre-upgrade", "snaptime": 1_700_000_000 }, { "name": "current" }]
            })))
            .mount(&server)
            .await;
        let p = nodes::GuestParams {
            node: "pve1".into(),
            vmid: 100,
        };
        let r = nodes::qemu_snapshots(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r[0]["name"], json!("pre-upgrade"));
    }

    #[tokio::test]
    async fn pipeline_disks_list_maps_flags_to_proxmox_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/disks/list"))
            .and(query_param("include-partitions", "1"))
            .and(query_param("skipsmart", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "devpath": "/dev/sda", "health": "PASSED", "wearout": 97 }]
            })))
            .mount(&server)
            .await;
        let p = serde_json::from_value(json!({
            "node": "pve1", "include_partitions": true, "skipsmart": false
        }))
        .unwrap();
        let r = nodes::disks_list(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r[0]["health"], json!("PASSED"));
    }

    #[tokio::test]
    async fn pipeline_disk_smart_passes_disk_as_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/disks/smart"))
            .and(query_param("disk", "/dev/nvme0n1"))
            .and(query_param("healthonly", "1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "data": { "health": "PASSED" } })),
            )
            .mount(&server)
            .await;
        let p = nodes::DiskSmartParams {
            node: "pve1".into(),
            disk: "/dev/nvme0n1".to_string(),
            healthonly: Some(true),
        };
        assert!(
            nodes::disk_smart(&mock_client(&server.uri()), p)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn pipeline_zfs_get_interpolates_pool_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/disks/zfs/rpool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "name": "rpool", "state": "ONLINE", "errors": "No known data errors" }
            })))
            .mount(&server)
            .await;
        let p = nodes::ZfsPoolParams {
            node: "pve1".into(),
            name: "rpool".to_string(),
        };
        let r = nodes::zfs_get(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r["state"], json!("ONLINE"));
    }

    #[tokio::test]
    async fn pipeline_replication_maps_vmid_to_guest() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/replication"))
            .and(query_param("guest", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "id": "100-0", "fail_count": 0 }]
            })))
            .mount(&server)
            .await;
        let p = nodes::ReplicationParams {
            node: "pve1".into(),
            vmid: Some(100),
        };
        let r = nodes::replication_list(&mock_client(&server.uri()), p)
            .await
            .unwrap();
        assert_eq!(r[0]["id"], json!("100-0"));
    }

    #[tokio::test]
    async fn cluster_level_tools_hit_their_endpoints() {
        let server = MockServer::start().await;
        for (p, body) in [
            (
                "/cluster/backup",
                json!([{ "id": "backup-1", "schedule": "21:00" }]),
            ),
            (
                "/cluster/backup-info/not-backed-up",
                json!([{ "vmid": 105, "name": "scratch", "type": "qemu" }]),
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
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": body })))
                .mount(&server)
                .await;
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
}
