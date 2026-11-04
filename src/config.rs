//! Cluster configuration: one JSON file per cluster in a config directory
//! (default `~/.config/proxmox-mcp/clusters/`), much like a set of kubeconfig
//! files. `prod.json` defines the cluster `prod`; with several files, the one
//! marked `"default": true` serves tool calls that omit `cluster`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

/// Name of the cluster defined by env vars alone (no cluster files), unless
/// `PROXMOX_CLUSTER` names it.
pub const ENV_CLUSTER_NAME: &str = "default";

/// File extension that marks a cluster file; anything else in the directory
/// (READMEs, editor backups, hidden files) is ignored.
const CLUSTER_FILE_EXTENSION: &str = "json";

/// One cluster file, as parsed. Unknown keys are rejected so a typo
/// (`"insecue"`, `"defualt"`) fails loudly instead of silently resolving to a
/// default.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterFile {
    url: Option<String>,
    token: Option<String>,
    insecure: Option<bool>,
    /// Marks the cluster used when a tool call omits `cluster`. Only needed
    /// when the directory holds more than one cluster file.
    #[serde(default)]
    default: bool,
    /// Where this cluster was loaded from, so errors can name the file.
    /// `None` for the env-only cluster.
    #[serde(skip)]
    source: Option<PathBuf>,
}

/// Redacts the token, so `Debug` on a loaded [`Config`] never prints it.
impl std::fmt::Debug for ClusterFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterFile")
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("insecure", &self.insecure)
            .field("default", &self.default)
            .field("source", &self.source)
            .finish()
    }
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

/// The cluster files of a config directory, before env-var resolution.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The directory the files came from, for error messages.
    dir: PathBuf,
    clusters: BTreeMap<String, ClusterFile>,
}

impl Config {
    /// Load every cluster file from `dir`, or from the default directory
    /// (`$XDG_CONFIG_HOME/proxmox-mcp/clusters`, else
    /// `~/.config/proxmox-mcp/clusters`) when `None`. Only the default
    /// directory may be absent — env vars can then define a single cluster —
    /// so a typo'd `--config-dir` fails loudly.
    pub fn load(dir: Option<&Path>) -> anyhow::Result<Self> {
        match dir {
            Some(d) => Self::load_dir(d, true),
            None => Self::load_dir(&default_config_dir()?, false),
        }
    }

    /// The directory the cluster files were (or would have been) read from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn load_dir(dir: &Path, required: bool) -> anyhow::Result<Self> {
        let mut config = Self {
            dir: dir.to_path_buf(),
            clusters: BTreeMap::new(),
        };
        // Read rather than probing with `exists()`, which reports `false` for
        // *any* error (e.g. permission denied) and would mask an unreadable
        // directory as a missing one.
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(config);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading config directory {}", dir.display()));
            }
        };

        for entry in entries {
            let path = entry
                .with_context(|| format!("reading config directory {}", dir.display()))?
                .path();
            let Some(name) = cluster_name(&path)? else {
                continue;
            };
            let cluster = load_cluster_file(&path)
                .with_context(|| format!("cluster file {}", path.display()))?;
            config.clusters.insert(name, cluster);
        }
        Ok(config)
    }

    /// Resolve every cluster and pick the default.
    ///
    /// The default is, in order: the cluster named by `PROXMOX_CLUSTER`; the
    /// only cluster; the one file marked `"default": true`. The env vars
    /// `PROXMOX_URL`, `PROXMOX_TOKEN` and `PROXMOX_INSECURE` (truthy =
    /// 1/true/yes/on) then override the default cluster only. With no cluster
    /// files at all, they define a single cluster on their own.
    pub fn resolve(&self) -> anyhow::Result<Clusters> {
        let mut raw = self.clusters.clone();
        let selected = std::env::var("PROXMOX_CLUSTER")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let default = if raw.is_empty() {
            let name = selected.unwrap_or_else(|| ENV_CLUSTER_NAME.to_string());
            validate_cluster_name(&name).context("PROXMOX_CLUSTER")?;
            raw.insert(name.clone(), ClusterFile::default());
            name
        } else if let Some(name) = selected {
            if !raw.contains_key(&name) {
                bail!(
                    "PROXMOX_CLUSTER is \"{name}\", but {} has no {name}.json (clusters: {})",
                    self.dir.display(),
                    self.cluster_names()
                );
            }
            name
        } else {
            self.marked_default()?
        };

        let entries = raw
            .into_iter()
            .map(|(name, mut cluster)| {
                if name == default {
                    apply_env(&mut cluster);
                }
                let source = cluster.source.clone();
                let conn = resolve_cluster(cluster, &self.dir).map_err(|e| match source {
                    Some(path) => e.context(format!("cluster file {}", path.display())),
                    None => e,
                })?;
                Ok((name, conn))
            })
            .collect::<anyhow::Result<_>>()?;

        Ok(Clusters { default, entries })
    }

    /// The default when `PROXMOX_CLUSTER` is unset: the only cluster, or the
    /// single file marked `"default": true`.
    fn marked_default(&self) -> anyhow::Result<String> {
        if self.clusters.len() == 1
            && let Some(name) = self.clusters.keys().next()
        {
            return Ok(name.clone());
        }
        let marked: Vec<&str> = self
            .clusters
            .iter()
            .filter(|(_, c)| c.default)
            .map(|(name, _)| name.as_str())
            .collect();
        match marked.as_slice() {
            [name] => Ok((*name).to_string()),
            [] => bail!(
                "several clusters are configured in {} ({}); add \"default\": true to one \
                 cluster file, or set PROXMOX_CLUSTER",
                self.dir.display(),
                self.cluster_names()
            ),
            several => bail!(
                "only one cluster file may set \"default\": true, but {} do: {}",
                several.len(),
                several.join(", ")
            ),
        }
    }

    fn cluster_names(&self) -> String {
        self.clusters.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// The cluster a directory entry defines — its file stem — or `None` for
/// entries that aren't cluster files (other extensions, hidden files).
fn cluster_name(path: &Path) -> anyhow::Result<Option<String>> {
    if path.extension().and_then(|e| e.to_str()) != Some(CLUSTER_FILE_EXTENSION) {
        return Ok(None);
    }
    let Some(stem) = path.file_stem() else {
        return Ok(None);
    };
    let name = stem
        .to_str()
        .with_context(|| format!("cluster file name {} is not UTF-8", path.display()))?;
    if name.starts_with('.') {
        return Ok(None);
    }
    validate_cluster_name(name).with_context(|| format!("cluster file {}", path.display()))?;
    Ok(Some(name.to_string()))
}

fn load_cluster_file(path: &Path) -> anyhow::Result<ClusterFile> {
    check_file_permissions(path)?;
    let contents = std::fs::read_to_string(path).context("reading file")?;
    let mut cluster: ClusterFile = serde_json::from_str(&contents).context("parsing JSON")?;
    cluster.source = Some(path.to_path_buf());
    Ok(cluster)
}

fn apply_env(cluster: &mut ClusterFile) {
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

fn resolve_cluster(raw: ClusterFile, dir: &Path) -> anyhow::Result<Connection> {
    let url = raw.url.ok_or_else(|| {
        if raw.source.is_some() {
            anyhow!("Proxmox URL not set: set \"url\" in the cluster file, or provide PROXMOX_URL")
        } else {
            anyhow!(
                "no Proxmox cluster configured: add a cluster file such as {} \
                 ({{\"url\": ..., \"token\": ...}}), or set PROXMOX_URL and PROXMOX_TOKEN",
                dir.join("pve.json").display()
            )
        }
    })?;
    enforce_https(&url)?;
    let url = normalize_url(&url);

    let token = raw.token.ok_or_else(|| {
        anyhow!(
            "Proxmox token not set: set \"token\" in the cluster file, or provide \
             PROXMOX_TOKEN (format: USER@REALM!TOKENID=UUID)"
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

/// `$XDG_CONFIG_HOME/proxmox-mcp/clusters`, falling back to
/// `~/.config/proxmox-mcp/clusters` (XDG says to ignore a relative value).
fn default_config_dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(xdg) if xdg.is_absolute() => xdg,
        _ => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .context("cannot determine home directory")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("proxmox-mcp").join("clusters"))
}

/// Reject world-readable cluster files on Unix to avoid token exposure.
#[cfg_attr(not(unix), allow(unused_variables))]
fn check_file_permissions(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).context("checking permissions")?;
        if meta.permissions().mode() & 0o004 != 0 {
            bail!("file is world-readable; run: chmod o-r {}", path.display());
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

    const ENV_VARS: [&str; 4] = [
        "PROXMOX_URL",
        "PROXMOX_TOKEN",
        "PROXMOX_INSECURE",
        "PROXMOX_CLUSTER",
    ];

    fn clear_env() {
        // SAFETY: callers hold ENV_LOCK, serializing all env access in this module.
        unsafe {
            for var in ENV_VARS {
                std::env::remove_var(var);
            }
        }
    }

    /// Set env vars for the rest of the test. Callers hold ENV_LOCK.
    fn set_env(vars: &[(&str, &str)]) {
        // SAFETY: callers hold ENV_LOCK, serializing all env access in this module.
        unsafe {
            for (k, v) in vars {
                std::env::set_var(k, v);
            }
        }
    }

    /// A temp config dir holding the given `(file name, contents)` pairs, each
    /// written `0600`. Keep the returned guard alive for the test.
    fn config_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::write(&path, content).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        dir
    }

    fn load(files: &[(&str, &str)]) -> anyhow::Result<Config> {
        let dir = config_dir(files);
        Config::load(Some(dir.path()))
    }

    /// Load + resolve with PROXMOX_* cleared. Callers must hold ENV_LOCK.
    fn resolve(files: &[(&str, &str)]) -> anyhow::Result<Clusters> {
        clear_env();
        load(files)?.resolve()
    }

    fn default_conn(c: &Clusters) -> &Connection {
        &c.entries[&c.default]
    }

    const LAB: &str = r#"{"url":"https://lab.example.com:8006","token":"lab@pam!t=x"}"#;
    const PROD: &str = r#"{"url":"https://prod.example.com:8006/api2/json","token":"prod@pam!t=y","insecure":true}"#;
    const PROD_DEFAULT: &str =
        r#"{"url":"https://prod.example.com:8006","token":"prod@pam!t=y","default":true}"#;

    #[test]
    fn missing_default_dir_returns_empty() {
        let cfg = Config::load_dir(Path::new("/nonexistent/clusters"), false).unwrap();
        assert!(cfg.clusters.is_empty());
    }

    #[test]
    fn missing_explicit_dir_is_error() {
        let err = Config::load(Some(Path::new("/nonexistent/clusters"))).unwrap_err();
        assert!(
            format!("{err:#}").contains("/nonexistent/clusters"),
            "{err:#}"
        );
    }

    #[test]
    fn one_file_per_cluster_named_by_its_stem() {
        let _guard = lock_env();
        let c = resolve(&[("lab.json", LAB), ("prod.json", PROD_DEFAULT)]).unwrap();
        assert_eq!(c.entries.len(), 2);
        assert_eq!(c.default, "prod");
        assert_eq!(
            c.entries["lab"].url,
            "https://lab.example.com:8006/api2/json"
        );
        assert_eq!(c.entries["lab"].token, "lab@pam!t=x");
        assert!(!c.entries["lab"].insecure, "insecure defaults to false");
        assert_eq!(c.entries["prod"].token, "prod@pam!t=y");
    }

    #[test]
    fn single_file_is_the_default_without_a_marker() {
        let _guard = lock_env();
        let c = resolve(&[("prod.json", PROD)]).unwrap();
        assert_eq!(c.default, "prod");
        let conn = default_conn(&c);
        assert_eq!(conn.url, "https://prod.example.com:8006/api2/json");
        assert!(conn.insecure);
    }

    #[test]
    fn several_files_without_a_marker_is_error() {
        let _guard = lock_env();
        let err = resolve(&[("lab.json", LAB), ("prod.json", PROD)]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("\"default\": true") && msg.contains("lab, prod"),
            "{msg}"
        );
    }

    #[test]
    fn several_marked_defaults_is_error() {
        let _guard = lock_env();
        let lab_default =
            r#"{"url":"https://lab.example.com","token":"lab@pam!t=x","default":true}"#;
        let err = resolve(&[("lab.json", lab_default), ("prod.json", PROD_DEFAULT)]).unwrap_err();
        assert!(format!("{err:#}").contains("lab, prod"), "{err:#}");
    }

    #[test]
    fn proxmox_cluster_overrides_the_marker() {
        let _guard = lock_env();
        clear_env();
        let cfg = load(&[("lab.json", LAB), ("prod.json", PROD_DEFAULT)]).unwrap();
        set_env(&[("PROXMOX_CLUSTER", "lab")]);
        let c = cfg.resolve();
        clear_env();
        assert_eq!(c.unwrap().default, "lab");
    }

    #[test]
    fn proxmox_cluster_naming_an_unknown_cluster_is_error() {
        let _guard = lock_env();
        clear_env();
        let cfg = load(&[("lab.json", LAB)]).unwrap();
        set_env(&[("PROXMOX_CLUSTER", "nope")]);
        let err = cfg.resolve().unwrap_err();
        clear_env();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nope.json") && msg.contains("clusters: lab"),
            "{msg}"
        );
    }

    #[test]
    fn env_overrides_only_the_default_cluster() {
        let _guard = lock_env();
        clear_env();
        let cfg = load(&[("lab.json", LAB), ("prod.json", PROD_DEFAULT)]).unwrap();
        set_env(&[
            ("PROXMOX_URL", "https://env.example.com:8006"),
            ("PROXMOX_TOKEN", "env@pam!t=z"),
            ("PROXMOX_INSECURE", "yes"),
        ]);
        let c = cfg.resolve();
        clear_env();
        let c = c.unwrap();
        assert_eq!(
            c.entries["prod"].url,
            "https://env.example.com:8006/api2/json"
        );
        assert_eq!(c.entries["prod"].token, "env@pam!t=z");
        assert!(c.entries["prod"].insecure);
        assert_eq!(c.entries["lab"].token, "lab@pam!t=x");
    }

    #[test]
    fn env_alone_defines_a_single_cluster() {
        let _guard = lock_env();
        clear_env();
        set_env(&[
            ("PROXMOX_URL", "https://env.example.com:8006"),
            ("PROXMOX_TOKEN", "env@pam!t=y"),
        ]);
        let c = Config::default().resolve();
        set_env(&[("PROXMOX_CLUSTER", "homelab")]);
        let named = Config::default().resolve();
        clear_env();
        let c = c.unwrap();
        assert_eq!(c.default, ENV_CLUSTER_NAME);
        assert_eq!(
            default_conn(&c).url,
            "https://env.example.com:8006/api2/json"
        );
        assert_eq!(named.unwrap().default, "homelab");
    }

    #[test]
    fn no_files_and_no_env_is_error_naming_the_dir() {
        let _guard = lock_env();
        let dir = config_dir(&[]);
        clear_env();
        let err = Config::load(Some(dir.path()))
            .unwrap()
            .resolve()
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no Proxmox cluster configured"), "{msg}");
        assert!(msg.contains("PROXMOX_URL"), "{msg}");
        assert!(msg.contains(&dir.path().display().to_string()), "{msg}");
    }

    #[test]
    fn ignores_non_json_and_hidden_files() {
        let cfg = load(&[
            ("lab.json", LAB),
            ("README.md", "notes"),
            ("lab.json.bak", "not json"),
            (".prod.json", "not json"),
        ])
        .unwrap();
        assert_eq!(cfg.cluster_names(), "lab");
    }

    #[test]
    fn invalid_file_names_are_rejected() {
        for bad in ["a b.json", "*.json", "a@b.json"] {
            let err = load(&[(bad, LAB)]).unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid cluster name"),
                "{bad}: {err:#}"
            );
        }
    }

    #[test]
    fn malformed_and_unknown_keys_are_rejected_naming_the_file() {
        let err = load(&[("lab.json", "not json")]).unwrap_err();
        assert!(format!("{err:#}").contains("lab.json"), "{err:#}");

        let err = load(&[(
            "lab.json",
            r#"{"url":"https://x.example.com","token":"t","insecue":true}"#,
        )])
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("lab.json") && msg.contains("insecue"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn world_readable_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = config_dir(&[("lab.json", LAB)]);
        let path = dir.path().join("lab.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = Config::load(Some(dir.path())).unwrap_err();
        assert!(format!("{err:#}").contains("world-readable"), "{err:#}");
    }

    #[test]
    fn missing_url_or_token_is_error_naming_the_file() {
        let _guard = lock_env();
        let err = resolve(&[("lab.json", r#"{"token":"u@pam!t=x"}"#)]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("lab.json") && msg.contains("URL not set"),
            "{msg}"
        );

        let err = resolve(&[("lab.json", r#"{"url":"https://pve.example.com"}"#)]).unwrap_err();
        assert!(format!("{err:#}").contains("token not set"), "{err:#}");
    }

    #[test]
    fn every_cluster_must_use_https_and_errors_name_the_file() {
        let _guard = lock_env();
        let err = resolve(&[
            (
                "lab.json",
                r#"{"url":"http://lab.example.com","token":"t"}"#,
            ),
            ("prod.json", PROD_DEFAULT),
        ])
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("lab.json") && msg.contains("HTTPS"), "{msg}");
    }

    #[test]
    fn default_config_dir_honours_xdg_config_home() {
        let _guard = lock_env();
        let saved = std::env::var_os("XDG_CONFIG_HOME");
        set_env(&[("XDG_CONFIG_HOME", "/xdg")]);
        let xdg = default_config_dir().unwrap();
        // A relative XDG_CONFIG_HOME is ignored per the XDG spec.
        set_env(&[("XDG_CONFIG_HOME", "relative")]);
        let relative = default_config_dir().unwrap();
        // SAFETY: ENV_LOCK serializes all env-touching tests in this module.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        assert_eq!(xdg, Path::new("/xdg/proxmox-mcp/clusters"));
        assert!(
            relative.ends_with(".config/proxmox-mcp/clusters"),
            "{relative:?}"
        );
    }

    #[test]
    fn config_debug_redacts_tokens() {
        let cfg = load(&[(
            "lab.json",
            r#"{"url":"https://a.example.com","token":"a@pam!t=supersecret"}"#,
        )])
        .unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("supersecret"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
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
    fn parse_bool_truthy_and_falsy() {
        for v in ["1", "true", "TRUE", "yes", "On"] {
            assert!(parse_bool(v), "{v} should be truthy");
        }
        for v in ["0", "false", "no", "", "maybe"] {
            assert!(!parse_bool(v), "{v} should be falsy");
        }
    }
}
