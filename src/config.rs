use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

/// Name given to the cluster described by the legacy single-cluster form
/// (top-level `url`/`token`/`insecure`) or by env vars alone.
pub const LEGACY_CLUSTER_NAME: &str = "default";

/// Raw config as loaded from the JSON file. Either the legacy single-cluster
/// fields or `clusters` (+ optional `default`) may be used, not both.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    url: Option<String>,
    token: Option<String>,
    insecure: Option<bool>,
    default: Option<String>,
    clusters: Option<BTreeMap<String, RawCluster>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawCluster {
    url: Option<String>,
    token: Option<String>,
    insecure: Option<bool>,
}

/// Resolved Proxmox connection settings, ready to build a client.
#[derive(Clone)]
pub struct Connection {
    /// Base API URL, e.g. `https://pve.example.com:8006/api2/json`.
    pub url: String,
    /// Full API token: `USER@REALM!TOKENID=UUID`.
    pub token: String,
    /// Accept invalid/self-signed TLS certificates (homelab default).
    pub insecure: bool,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("insecure", &self.insecure)
            .finish()
    }
}

/// Every configured cluster, keyed by name, plus which one tools use when the
/// caller omits `cluster`.
#[derive(Debug, Clone)]
pub struct Clusters {
    pub default: String,
    pub entries: BTreeMap<String, Connection>,
}

/// Config as loaded from file, before env-var resolution.
#[derive(Debug, Clone, Default)]
pub struct Config {
    default: Option<String>,
    clusters: BTreeMap<String, RawCluster>,
    /// True when the file used the `clusters` map, so errors name the cluster.
    named: bool,
}

impl Config {
    /// Load configuration from path (default: `~/.proxmox_mcp.json`).
    /// A missing file is not an error.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let resolved = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };

        if !resolved.exists() {
            return Ok(Self::default());
        }

        check_file_permissions(&resolved)?;

        let contents = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading config file {}", resolved.display()))?;
        let raw: RawConfig = serde_json::from_str(&contents)
            .with_context(|| format!("parsing config file {}", resolved.display()))?;
        Self::from_raw(raw).with_context(|| format!("invalid config file {}", resolved.display()))
    }

    fn from_raw(raw: RawConfig) -> anyhow::Result<Self> {
        let has_legacy = raw.url.is_some() || raw.token.is_some() || raw.insecure.is_some();
        let Some(clusters) = raw.clusters else {
            if raw.default.is_some() {
                bail!("\"default\" is only valid together with a \"clusters\" map");
            }
            let mut clusters = BTreeMap::new();
            if has_legacy {
                clusters.insert(
                    LEGACY_CLUSTER_NAME.to_string(),
                    RawCluster {
                        url: raw.url,
                        token: raw.token,
                        insecure: raw.insecure,
                    },
                );
            }
            return Ok(Self {
                default: None,
                clusters,
                named: false,
            });
        };

        if has_legacy {
            bail!(
                "use either top-level \"url\"/\"token\"/\"insecure\" or a \"clusters\" map, not both"
            );
        }
        if clusters.is_empty() {
            bail!("\"clusters\" must contain at least one cluster");
        }
        for name in clusters.keys() {
            validate_cluster_name(name)?;
        }
        Ok(Self {
            default: raw.default,
            clusters,
            named: true,
        })
    }

    /// Resolve every cluster. Env vars (`PROXMOX_URL`, `PROXMOX_TOKEN`,
    /// `PROXMOX_INSECURE`, truthy = 1/true/yes/on) override the default
    /// cluster only; with no config file they define a single cluster.
    pub fn resolve(&self) -> anyhow::Result<Clusters> {
        let default = match (&self.default, self.clusters.len()) {
            (Some(d), _) => d.clone(),
            (None, 0) => LEGACY_CLUSTER_NAME.to_string(),
            (None, 1) => self.clusters.keys().next().cloned().unwrap_or_default(),
            (None, _) => bail!(
                "several clusters are configured; set \"default\" to one of: {}",
                self.cluster_names()
            ),
        };

        let mut raw = self.clusters.clone();
        if !raw.contains_key(&default) {
            if raw.is_empty() {
                raw.insert(default.clone(), RawCluster::default());
            } else {
                bail!(
                    "default cluster \"{default}\" is not in \"clusters\" (configured: {})",
                    self.cluster_names()
                );
            }
        }

        let entries = raw
            .into_iter()
            .map(|(name, mut cluster)| {
                if name == default {
                    apply_env(&mut cluster);
                }
                let conn = resolve_cluster(cluster).map_err(|e| {
                    if self.named {
                        e.context(format!("cluster \"{name}\""))
                    } else {
                        e
                    }
                })?;
                Ok((name, conn))
            })
            .collect::<anyhow::Result<_>>()?;

        Ok(Clusters { default, entries })
    }

    fn cluster_names(&self) -> String {
        self.clusters.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

fn apply_env(cluster: &mut RawCluster) {
    if let Ok(url) = std::env::var("PROXMOX_URL") {
        cluster.url = Some(url);
    }
    if let Ok(token) = std::env::var("PROXMOX_TOKEN") {
        cluster.token = Some(token);
    }
    if let Ok(v) = std::env::var("PROXMOX_INSECURE") {
        cluster.insecure = Some(parse_bool(&v));
    }
}

fn resolve_cluster(raw: RawCluster) -> anyhow::Result<Connection> {
    let url = raw.url.ok_or_else(|| {
        anyhow!("Proxmox URL not set: provide PROXMOX_URL or set \"url\" in config file")
    })?;
    enforce_https(&url)?;
    let url = normalize_url(&url);

    let token = raw.token.ok_or_else(|| {
        anyhow!(
            "Proxmox token not set: provide PROXMOX_TOKEN or set \"token\" in config file \
             (format: USER@REALM!TOKENID=UUID)"
        )
    })?;

    Ok(Connection {
        url,
        token,
        insecure: raw.insecure.unwrap_or(false),
    })
}

/// Cluster names are echoed in tool output and passed back as arguments, so
/// keep them to a plain identifier charset. `*` is reserved for "all clusters".
fn validate_cluster_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        bail!("invalid cluster name {name:?}: use only letters, digits, '-', '_' and '.'")
    }
}

/// Proxmox serves its REST API under the `/api2/json` path. Users routinely
/// point the config at a bare `https://host:8006`, which makes every call 500
/// with a misleading `no such file '/version'`. Append the path when it is
/// missing so the bare host form just works.
fn normalize_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/api2/json") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/api2/json")
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn default_config_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("cannot determine home directory")?;
    Ok(PathBuf::from(home).join(".proxmox_mcp.json"))
}

/// Reject world-readable config files on Unix to avoid token exposure.
#[allow(unused_variables)]
fn check_file_permissions(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .with_context(|| format!("checking permissions of {}", path.display()))?;
        if meta.permissions().mode() & 0o004 != 0 {
            bail!(
                "config file {} is world-readable; run: chmod o-r {}",
                path.display(),
                path.display()
            );
        }
    }
    Ok(())
}

/// Proxmox always serves its API over HTTPS (port 8006). Reject plaintext so
/// the token is never sent in the clear; self-signed certs are handled
/// separately via the `insecure` flag, not by downgrading the scheme.
fn enforce_https(url: &str) -> anyhow::Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    bail!(
        "Proxmox URL must use HTTPS, got: {url}  \
         (use https://; for self-signed certs set \"insecure\": true instead)"
    );
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    // Cargo runs unit tests on multiple threads in one process. Tests that touch
    // PROXMOX_* env vars must serialize, or one test's set_var leaks into
    // another's resolve(). Hold this guard for the whole test body.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn clear_env() {
        // SAFETY: callers hold ENV_LOCK, serializing all env access in this module.
        unsafe {
            std::env::remove_var("PROXMOX_URL");
            std::env::remove_var("PROXMOX_TOKEN");
            std::env::remove_var("PROXMOX_INSECURE");
        }
    }

    fn write_config(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("config.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn load_str(content: &str) -> anyhow::Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), content);
        Config::load(Some(&path))
    }

    /// Load + resolve with PROXMOX_* cleared. Callers must hold ENV_LOCK.
    fn resolve_str(content: &str) -> anyhow::Result<Clusters> {
        clear_env();
        load_str(content)?.resolve()
    }

    fn default_conn(c: &Clusters) -> &Connection {
        &c.entries[&c.default]
    }

    #[test]
    fn missing_file_returns_empty() {
        let cfg = Config::load(Some(Path::new("/nonexistent/path.json"))).unwrap();
        assert!(cfg.clusters.is_empty());
        assert!(cfg.default.is_none());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(load_str("not json").is_err());
    }

    #[test]
    fn legacy_form_becomes_default_cluster() {
        let _guard = lock_env();
        let c = resolve_str(
            r#"{"url":"https://file.example.com:8006/api2/json","token":"u@pam!t=x","insecure":true}"#,
        )
        .unwrap();
        assert_eq!(c.default, LEGACY_CLUSTER_NAME);
        assert_eq!(c.entries.len(), 1);
        let conn = default_conn(&c);
        assert_eq!(conn.url, "https://file.example.com:8006/api2/json");
        assert_eq!(conn.token, "u@pam!t=x");
        assert!(conn.insecure);
    }

    #[test]
    fn env_overrides_file() {
        let _guard = lock_env();
        clear_env();
        let cfg = load_str(
            r#"{"url":"https://file.example.com","token":"file@pam!t=x","insecure":false}"#,
        )
        .unwrap();
        // SAFETY: ENV_LOCK serializes all env-touching tests in this module.
        unsafe {
            std::env::set_var("PROXMOX_URL", "https://env.example.com:8006/api2/json");
            std::env::set_var("PROXMOX_TOKEN", "env@pam!t=y");
            std::env::set_var("PROXMOX_INSECURE", "yes");
        }
        let c = cfg.resolve().unwrap();
        clear_env();
        let conn = default_conn(&c);
        assert_eq!(conn.url, "https://env.example.com:8006/api2/json");
        assert_eq!(conn.token, "env@pam!t=y");
        assert!(conn.insecure);
    }

    #[test]
    fn env_alone_defines_single_cluster() {
        let _guard = lock_env();
        clear_env();
        // SAFETY: ENV_LOCK serializes all env-touching tests in this module.
        unsafe {
            std::env::set_var("PROXMOX_URL", "https://env.example.com:8006");
            std::env::set_var("PROXMOX_TOKEN", "env@pam!t=y");
        }
        let c = Config::default().resolve();
        clear_env();
        let c = c.unwrap();
        assert_eq!(c.default, LEGACY_CLUSTER_NAME);
        assert_eq!(
            default_conn(&c).url,
            "https://env.example.com:8006/api2/json"
        );
    }

    #[test]
    fn insecure_defaults_false() {
        let _guard = lock_env();
        let c = resolve_str(r#"{"url":"https://pve.example.com","token":"u@pam!t=x"}"#).unwrap();
        assert!(!default_conn(&c).insecure);
    }

    #[test]
    fn missing_url_is_error() {
        let _guard = lock_env();
        assert!(resolve_str(r#"{"token":"u@pam!t=x"}"#).is_err());
        assert!(resolve_str("{}").is_err());
    }

    #[test]
    fn missing_token_is_error() {
        let _guard = lock_env();
        assert!(resolve_str(r#"{"url":"https://pve.example.com"}"#).is_err());
    }

    const TWO_CLUSTERS: &str = r#"{
        "default": "site-a",
        "clusters": {
            "site-a": {"url":"https://a.example.com:8006","token":"a@pam!t=x"},
            "site-b": {"url":"https://b.example.com:8006/api2/json","token":"b@pam!t=y","insecure":true}
        }
    }"#;

    #[test]
    fn parses_clusters_map() {
        let _guard = lock_env();
        let c = resolve_str(TWO_CLUSTERS).unwrap();
        assert_eq!(c.default, "site-a");
        assert_eq!(c.entries.len(), 2);
        assert_eq!(
            c.entries["site-a"].url,
            "https://a.example.com:8006/api2/json"
        );
        assert!(!c.entries["site-a"].insecure);
        assert_eq!(c.entries["site-b"].token, "b@pam!t=y");
        assert!(c.entries["site-b"].insecure);
    }

    #[test]
    fn env_overrides_only_default_cluster() {
        let _guard = lock_env();
        clear_env();
        let cfg = load_str(TWO_CLUSTERS).unwrap();
        // SAFETY: ENV_LOCK serializes all env-touching tests in this module.
        unsafe {
            std::env::set_var("PROXMOX_TOKEN", "env@pam!t=z");
        }
        let c = cfg.resolve();
        clear_env();
        let c = c.unwrap();
        assert_eq!(c.entries["site-a"].token, "env@pam!t=z");
        assert_eq!(c.entries["site-b"].token, "b@pam!t=y");
    }

    #[test]
    fn default_inferred_for_single_named_cluster() {
        let _guard = lock_env();
        let c = resolve_str(
            r#"{"clusters":{"only":{"url":"https://o.example.com","token":"o@pam!t=x"}}}"#,
        )
        .unwrap();
        assert_eq!(c.default, "only");
    }

    #[test]
    fn several_clusters_without_default_is_error() {
        let _guard = lock_env();
        let err = resolve_str(
            r#"{"clusters":{
                "a":{"url":"https://a.example.com","token":"a@pam!t=x"},
                "b":{"url":"https://b.example.com","token":"b@pam!t=x"}}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("set \"default\""), "{err:#}");
    }

    #[test]
    fn unknown_default_is_error() {
        let _guard = lock_env();
        let err = resolve_str(
            r#"{"default":"nope","clusters":{"a":{"url":"https://a.example.com","token":"a@pam!t=x"}}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("\"nope\""), "{err:#}");
    }

    #[test]
    fn mixing_legacy_and_clusters_is_error() {
        assert!(
            load_str(
                r#"{"url":"https://x.example.com","clusters":{"a":{"url":"https://a.example.com","token":"t"}}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn default_without_clusters_is_error() {
        assert!(load_str(r#"{"default":"a","url":"https://x.example.com","token":"t"}"#).is_err());
    }

    #[test]
    fn empty_clusters_map_is_error() {
        assert!(load_str(r#"{"clusters":{}}"#).is_err());
    }

    #[test]
    fn invalid_cluster_names_are_rejected() {
        for bad in ["*", "", "a b", "a/b"] {
            let json = format!(
                r#"{{"clusters":{{"{bad}":{{"url":"https://a.example.com","token":"t"}}}}}}"#
            );
            assert!(load_str(&json).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn every_cluster_must_use_https_and_errors_name_it() {
        let _guard = lock_env();
        let err = resolve_str(
            r#"{"default":"a","clusters":{
                "a":{"url":"https://a.example.com","token":"a@pam!t=x"},
                "b":{"url":"http://b.example.com","token":"b@pam!t=x"}}}"#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cluster \"b\""), "{msg}");
        assert!(msg.contains("HTTPS"), "{msg}");
    }

    #[test]
    fn connection_debug_redacts_token() {
        let conn = Connection {
            url: "https://pve.example.com/api2/json".into(),
            token: "root@pam!mcp=supersecret".into(),
            insecure: false,
        };
        let dbg = format!("{conn:?}");
        assert!(!dbg.contains("supersecret"), "{dbg}");
    }

    #[test]
    fn enforce_https_accepts_https() {
        assert!(enforce_https("https://pve.example.com:8006/api2/json").is_ok());
    }

    #[test]
    fn enforce_https_rejects_http() {
        assert!(enforce_https("http://pve.example.com:8006/api2/json").is_err());
        assert!(enforce_https("http://localhost:8006").is_err());
        assert!(enforce_https("ftp://pve.example.com").is_err());
        assert!(enforce_https("").is_err());
    }

    #[test]
    fn normalize_url_appends_api_path_when_missing() {
        assert_eq!(
            normalize_url("https://pve.example.com:8006"),
            "https://pve.example.com:8006/api2/json"
        );
        assert_eq!(
            normalize_url("https://pve.example.com:8006/"),
            "https://pve.example.com:8006/api2/json"
        );
    }

    #[test]
    fn normalize_url_leaves_existing_api_path_intact() {
        assert_eq!(
            normalize_url("https://pve.example.com:8006/api2/json"),
            "https://pve.example.com:8006/api2/json"
        );
        assert_eq!(
            normalize_url("https://pve.example.com:8006/api2/json/"),
            "https://pve.example.com:8006/api2/json"
        );
    }

    #[test]
    fn resolve_normalizes_bare_host_url() {
        let _guard = lock_env();
        let c =
            resolve_str(r#"{"url":"https://pve.example.com:8006","token":"u@pam!t=x"}"#).unwrap();
        assert_eq!(
            default_conn(&c).url,
            "https://pve.example.com:8006/api2/json"
        );
    }

    #[test]
    fn parse_bool_truthy_and_falsy() {
        for v in ["1", "true", "TRUE", "yes", "On"] {
            assert!(parse_bool(v), "{v} should be truthy");
        }
        for v in ["0", "false", "no", "", "maybe"] {
            assert!(!parse_bool(v), "{v} should be falsy");
        }
    }
}
