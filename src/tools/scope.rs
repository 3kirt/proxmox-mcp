//! Per-call cluster selection. Tool shims take `Scoped<P>` (one cluster) or
//! `AnyScoped<P>` (one cluster or `"*"`), which flatten an optional `cluster`
//! argument next to the domain params `P`, so domain fns stay cluster-agnostic.

use serde::Deserialize;

/// The `cluster` value that selects every configured cluster.
pub const ALL_CLUSTERS: &str = "*";

macro_rules! cluster_name_type {
    ($name:ident, $description:literal) => {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl schemars::JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "description": $description
                })
            }
        }
    };
}

cluster_name_type!(
    ClusterId,
    "Proxmox cluster to query, by configured name (see proxmox_clusters_list). Omit to use the default cluster."
);

cluster_name_type!(
    ClusterSelector,
    "Proxmox cluster to query, by configured name (see proxmox_clusters_list), or \"*\" to query every configured cluster and tag each result with its cluster. Omit to use the default cluster."
);

/// Params for a tool that runs against exactly one cluster.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Scoped<P> {
    pub cluster: Option<ClusterId>,
    #[serde(flatten)]
    pub inner: P,
}

/// Params for a tool that also accepts `cluster: "*"` (fan-out to all clusters).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnyScoped<P> {
    pub cluster: Option<ClusterSelector>,
    #[serde(flatten)]
    pub inner: P,
}

/// Domain params for tools that take nothing but `cluster`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}
