//! Tool-argument plumbing shared by every tool: string parameter types whose
//! descriptions live in one place, the `cluster` selection wrappers, and the
//! helpers that turn arguments into Proxmox paths and query strings.

use serde::Deserialize;

/// A string tool argument whose JSON-schema description is defined once and
/// inlined at every use site (not hidden behind a `$ref`). Extra attributes,
/// e.g. `#[serde(transparent)]` or `#[serde(try_from = "String")]`, pick how
/// it deserializes. Derefs to `str`.
macro_rules! string_param {
    ($(#[$attr:meta])* $name:ident, $description:literal) => {
        #[derive(Debug, Clone, Deserialize)]
        $(#[$attr])*
        pub struct $name(String);

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
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
                schemars::json_schema!({ "type": "string", "description": $description })
            }
        }
    };
}

string_param!(
    #[serde(transparent)]
    NodeId,
    "Cluster node name (see proxmox_nodes_list)"
);

string_param!(
    #[serde(transparent)]
    ClusterId,
    "Proxmox cluster to query, by configured name (see proxmox_clusters_list). Omit to use the default cluster."
);

string_param!(
    #[serde(transparent)]
    ClusterSelector,
    "Proxmox cluster to query, by configured name (see proxmox_clusters_list), or \"*\" to query every configured cluster and tag each result with its cluster. Omit to use the default cluster."
);

string_param!(
    /// A Proxmox task ID (`UPID:{node}:{pid}:{pstart}:{starttime}:{type}:{id}:{user}:`),
    /// validated on deserialization. It carries its own node, so task tools need
    /// no separate `node` argument.
    #[serde(try_from = "String")]
    Upid,
    "Task ID (UPID), e.g. UPID:pve1:0000ABCD:...:vzdump:100:root@pam: — the `upid` field from proxmox_cluster_tasks_list or proxmox_nodes_tasks_list"
);

impl TryFrom<String> for Upid {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let mut parts = s.split(':');
        if parts.next() == Some("UPID") && parts.next().is_some_and(|n| !n.is_empty()) {
            Ok(Self(s))
        } else {
            Err(format!(
                "invalid task UPID {s:?}: expected UPID:<node>:... as returned by \
                 proxmox_cluster_tasks_list / proxmox_nodes_tasks_list"
            ))
        }
    }
}

impl Upid {
    /// The node that ran the task (the UPID's second field).
    pub fn node(&self) -> &str {
        self.0.split(':').nth(1).unwrap_or_default()
    }
}

/// The `cluster` value that selects every configured cluster.
pub const ALL_CLUSTERS: &str = "*";

/// Args for a tool that runs against exactly one cluster: an optional
/// `cluster` flattened beside the domain params `P`, so domain fns never see it.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Scoped<P> {
    pub cluster: Option<ClusterId>,
    #[serde(flatten)]
    pub inner: P,
}

/// Like [`Scoped`], but the tool also accepts `cluster: "*"` (all clusters).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnyScoped<P> {
    pub cluster: Option<ClusterSelector>,
    #[serde(flatten)]
    pub inner: P,
}

/// Domain params for tools that take nothing but `cluster`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

/// Percent-encode a single URL path segment. Encodes everything except the
/// RFC 3986 unreserved set, so user-supplied node/storage names cannot inject
/// extra path components (`/`, `..`) or break the request URL.
pub fn encode_seg(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Fluent builder for optional query parameters.
#[derive(Default)]
pub struct QueryBuilder {
    params: Vec<(&'static str, String)>,
}

impl QueryBuilder {
    pub fn new() -> Self {
        Self::default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(encode_seg("é"), "%C3%A9");
    }

    #[test]
    fn query_builder_skips_none_and_encodes_flags() {
        let params = QueryBuilder::new()
            .opt("a", Some(1))
            .opt::<i32>("b", None)
            .opt("c", Some("x".to_string()))
            .flag("d", Some(true))
            .flag("e", Some(false))
            .flag("f", None)
            .into_params();
        assert_eq!(
            params,
            vec![
                ("a", "1".to_string()),
                ("c", "x".to_string()),
                ("d", "1".to_string()),
                ("e", "0".to_string())
            ]
        );
    }

    #[test]
    fn transparent_params_deserialize_from_plain_strings() {
        let node: NodeId = serde_json::from_value(json!("pve1")).unwrap();
        assert_eq!(&*node, "pve1");
    }

    #[test]
    fn upid_reads_node_and_rejects_malformed_ids() {
        let upid: Upid =
            serde_json::from_value(json!("UPID:pve2:0000ABCD:0:0:vzdump:100:root@pam:")).unwrap();
        assert_eq!(upid.node(), "pve2");
        for bad in ["", "UPID", "UPID::x", "TASK:pve1:1", "pve1"] {
            let err = serde_json::from_value::<Upid>(json!(bad)).unwrap_err();
            assert!(
                err.to_string().contains("invalid task UPID"),
                "{bad:?}: {err}"
            );
        }
    }
}
