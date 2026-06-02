use crate::client::{ProxmoxClient, ProxmoxError};
use crate::tools::QueryBuilder;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClusterResourcesParams {
    #[schemars(description = "Restrict to one resource type: vm, storage, node, or sdn")]
    pub r#type: Option<String>,
}

/// Cluster-wide resource index — the single best inventory call. Lists every
/// VM, container, storage, and node across the cluster.
pub async fn cluster_resources(
    client: &ProxmoxClient,
    p: ClusterResourcesParams,
) -> Result<Value, ProxmoxError> {
    let params = QueryBuilder::new().opt("type", p.r#type).into_params();
    client.get("/cluster/resources", &params).await
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClusterTasksParams {
    #[schemars(description = "Cap the number of tasks returned (default 50, most recent first)")]
    pub limit: Option<i32>,
    #[schemars(description = "Only return tasks that finished with an error (status != OK)")]
    pub errors: Option<bool>,
    #[schemars(description = "Only return tasks started at or after this UNIX epoch")]
    pub since: Option<i64>,
    #[schemars(description = "Only return tasks that ran on this node")]
    pub node: Option<String>,
}

/// Recent tasks across the whole cluster.
///
/// The Proxmox `/cluster/tasks` endpoint takes no query parameters, so the
/// `limit`/`errors`/`since`/`node` filters are applied client-side. The list
/// arrives most-recent-first; `limit` keeps the newest N.
pub async fn cluster_tasks(
    client: &ProxmoxClient,
    p: ClusterTasksParams,
) -> Result<Value, ProxmoxError> {
    let data = client.get("/cluster/tasks", &[]).await?;
    let Value::Array(mut tasks) = data else {
        return Ok(data);
    };

    if let Some(node) = &p.node {
        tasks.retain(|t| t.get("node").and_then(Value::as_str) == Some(node.as_str()));
    }
    if let Some(since) = p.since {
        tasks.retain(|t| {
            t.get("starttime")
                .and_then(Value::as_i64)
                .is_some_and(|s| s >= since)
        });
    }
    if p.errors.unwrap_or(false) {
        // A finished, successful task reports status "OK"; failures carry the
        // error text, and still-running tasks have no status yet.
        tasks.retain(|t| {
            t.get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "OK")
        });
    }

    let limit = p.limit.unwrap_or(50).max(0) as usize;
    tasks.truncate(limit);
    Ok(Value::Array(tasks))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GuestFindParams {
    #[schemars(
        description = "Case-insensitive substring to match against guest names. Omit to list every guest cluster-wide."
    )]
    pub name: Option<String>,
}

/// Find guests anywhere in the cluster by name, resolving each match to its
/// node + vmid. Wraps `/cluster/resources?type=vm` (which covers both QEMU VMs
/// and LXC containers) and filters by name client-side, so a single call
/// replaces a full cluster dump scanned by hand.
pub async fn guest_find(client: &ProxmoxClient, p: GuestFindParams) -> Result<Value, ProxmoxError> {
    let data = client
        .get("/cluster/resources", &[("type", "vm".to_string())])
        .await?;
    let Value::Array(mut guests) = data else {
        return Ok(data);
    };

    if let Some(name) = &p.name {
        let needle = name.to_lowercase();
        guests.retain(|g| {
            g.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.to_lowercase().contains(&needle))
        });
    }
    Ok(Value::Array(guests))
}
