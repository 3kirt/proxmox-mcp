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
pub struct ClusterTasksParams {}

/// Recent tasks across the whole cluster.
pub async fn cluster_tasks(
    client: &ProxmoxClient,
    _p: ClusterTasksParams,
) -> Result<Value, ProxmoxError> {
    client.get("/cluster/tasks", &[]).await
}
