//! Cluster configuration: one JSON file per cluster in a config directory
//! (default `~/.config/proxmox-mcp/clusters/`), much like a set of kubeconfig
//! files. `prod.json` defines the cluster `prod`; with several files, the one
//! marked `"default": true` serves tool calls that omit `cluster`.
//!
//! A cluster's token is either plaintext (`"token"`) or GPG-encrypted in a
//! `pass`-compatible store (`"pass_entry"`, see [`crate::credentials`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

use crate::credentials::{PassEntry, PassStore, StoreLocation};

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
    /// Name of a `pass`-compatible store entry holding the GPG-encrypted
    /// token, as an alternative to the plaintext `token`.
    pass_entry: Option<String>,
    /// Path to the gpg binary (default: `gpg` from PATH).
    gpg: Option<String>,
    /// Password-store root (default: `$PASSWORD_STORE_DIR`, else `~/.password-store`).
    password_store_dir: Option<String>,
    /// GnuPG home (default: `$GNUPGHOME`, else gpg's own default).
    gnupg_home: Option<String>,
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
            .field("pass_entry", &self.pass_entry)
            .field("gpg", &self.gpg)
            .field("password_store_dir", &self.password_store_dir)
            .field("gnupg_home", &self.gnupg_home)
            .field("source", &self.source)
            .finish()
    }
}

impl ClusterFile {
    /// Treat blank values and unexpanded `${VAR}` placeholders as unset, the
    /// same rule [`env_var`] applies to the environment.
    fn normalized(self) -> Self {
        Self {
            url: non_blank(self.url),
            token: non_blank(self.token),
            pass_entry: non_blank(self.pass_entry),
            gpg: non_blank(self.gpg),
            password_store_dir: non_blank(self.password_store_dir),
            gnupg_home: non_blank(self.gnupg_home),
            ..self
        }
    }

    /// The password store this cluster's `gpg` / `password_store_dir` /
    /// `gnupg_home` point at (each falling back to the environment).
    fn pass_store(&self) -> anyhow::Result<PassStore> {
        PassStore::resolve(&StoreLocation {
            gpg: self.gpg.as_deref(),
            store_dir: self.password_store_dir.as_deref(),
            gnupg_home: self.gnupg_home.as_deref(),
        })
    }
}

/// Where a cluster's API token comes from.
#[derive(Clone)]
pub enum Credential {
    /// `PROXMOX_TOKEN` environment variable (default cluster only).
    Env(String),
    /// Plaintext `token` in the cluster file.
    File(String),
    /// GPG-encrypted `pass` entry, decrypted lazily on first use.
    Pass(PassEntry),
}

impl Credential {
    /// Human description, never including the secret.
    pub fn describe(&self) -> String {
        match self {
            Self::Env(_) => "PROXMOX_TOKEN environment variable".into(),
            Self::File(_) => "plaintext \"token\" in the cluster file".into(),
            Self::Pass(e) => format!("pass entry {} ({})", e.name(), e.path().display()),
        }
    }
}

/// Redacts plaintext tokens.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(_) => f.write_str("Env(<redacted>)"),
            Self::File(_) => f.write_str("File(<redacted>)"),
            Self::Pass(e) => f.debug_tuple("Pass").field(&e.name()).finish(),
        }
    }
}

/// Resolved Proxmox connection settings, ready to build a client.
#[derive(Debug, Clone)]
pub struct Connection {
    /// Base API URL, e.g. `https://pve.example.com:8006/api2/json`.
    pub url: String,
    /// The API token (`USER@REALM!TOKENID=UUID`), or where to decrypt it from.
    pub credential: Credential,
    /// Accept invalid/self-signed TLS certificates (homelab default).
    pub insecure: bool,
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

    /// Like [`Self::load`], but a missing directory — even an explicit
    /// `--config-dir` — is empty: for `proxmox-mcp login`, which creates it.
    pub fn load_or_empty(dir: Option<&Path>) -> anyhow::Result<Self> {
        match dir {
            Some(d) => Self::load_dir(d, false),
            None => Self::load_dir(&default_config_dir()?, false),
        }
    }

    /// The directory the cluster files were (or would have been) read from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file that defines (or would define) cluster `name`.
    pub fn cluster_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.{CLUSTER_FILE_EXTENSION}"))
    }

    /// Names of every cluster file, sorted.
    pub fn names(&self) -> Vec<String> {
        self.clusters.keys().cloned().collect()
    }

    /// The cluster file's own `url` (ignoring `PROXMOX_URL`).
    pub fn file_url(&self, name: &str) -> Option<&str> {
        self.clusters.get(name)?.url.as_deref()
    }

    /// The cluster file's own `insecure` (ignoring `PROXMOX_INSECURE`).
    pub fn file_insecure(&self, name: &str) -> Option<bool> {
        self.clusters.get(name)?.insecure
    }

    /// The cluster file's `pass_entry` name.
    pub fn file_pass_entry(&self, name: &str) -> Option<&str> {
        self.clusters.get(name)?.pass_entry.as_deref()
    }

    /// The password store for cluster `name`, honouring its `gpg`,
    /// `password_store_dir` and `gnupg_home` over the process environment
    /// (or the environment alone when there is no such cluster file yet).
    pub fn pass_store(&self, name: &str) -> anyhow::Result<PassStore> {
        self.clusters.get(name).map_or_else(
            || ClusterFile::default().pass_store(),
            ClusterFile::pass_store,
        )
    }

    /// The cluster file's `pass_entry` as an entry, regardless of
    /// `PROXMOX_TOKEN`. `unlock`/`status` use this: a shell exporting
    /// PROXMOX_TOKEN must not hide the entry a GUI-launched server, started
    /// without that variable, is trying to decrypt.
    pub fn configured_pass_entry(&self, name: &str) -> anyhow::Result<Option<PassEntry>> {
        self.file_pass_entry(name)
            .map(|entry| Ok(self.pass_store(name)?.entry(entry)?))
            .transpose()
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
        let default = self
            .default_name()?
            .unwrap_or_else(|| ENV_CLUSTER_NAME.to_string());
        if self.clusters.is_empty() {
            let conn = self.connection(&default, true)?;
            return Ok(Clusters {
                entries: BTreeMap::from([(default.clone(), conn)]),
                default,
            });
        }
        let entries = self
            .clusters
            .keys()
            .map(|name| Ok((name.clone(), self.connection(name, *name == default)?)))
            .collect::<anyhow::Result<_>>()?;
        Ok(Clusters { default, entries })
    }

    /// The default cluster's name: `PROXMOX_CLUSTER`, else the only cluster
    /// file, else the one marked `"default": true`. `None` when there are no
    /// cluster files and `PROXMOX_CLUSTER` is unset.
    pub fn default_name(&self) -> anyhow::Result<Option<String>> {
        let selected = env_var("PROXMOX_CLUSTER").map(|s| s.trim().to_string());
        if self.clusters.is_empty() {
            if let Some(name) = &selected {
                validate_cluster_name(name).context("PROXMOX_CLUSTER")?;
            }
            return Ok(selected);
        }
        if let Some(name) = selected {
            if !self.clusters.contains_key(&name) {
                bail!(
                    "PROXMOX_CLUSTER is \"{name}\", but {} has no {name}.json (clusters: {})",
                    self.dir.display(),
                    self.cluster_names()
                );
            }
            return Ok(Some(name));
        }
        self.marked_default().map(Some)
    }

    /// Resolve one cluster. The default cluster also takes the `PROXMOX_*`
    /// overrides; a name with no cluster file is defined by them alone.
    pub fn connection(&self, name: &str, is_default: bool) -> anyhow::Result<Connection> {
        let mut cluster = self.clusters.get(name).cloned().unwrap_or_default();
        let env_token = if is_default {
            apply_env(&mut cluster);
            env_var("PROXMOX_TOKEN")
        } else {
            None
        };
        let source = cluster.source.clone();
        resolve_cluster(&cluster, env_token, &self.dir).map_err(|e| match source {
            Some(path) => e.context(format!("cluster file {}", path.display())),
            None => e,
        })
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
        self.names().join(", ")
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
    Ok(cluster.normalized())
}

/// `PROXMOX_URL` / `PROXMOX_INSECURE` overrides (the token is handled by
/// [`resolve_cluster`], since it also displaces `pass_entry`).
fn apply_env(cluster: &mut ClusterFile) {
    if let Some(url) = env_var("PROXMOX_URL") {
        cluster.url = Some(url);
    }
    if let Some(v) = env_var("PROXMOX_INSECURE") {
        cluster.insecure = Some(parse_bool(&v));
    }
}

/// The one "is this value set?" rule for every env var and config value:
/// blank counts as unset, and so does an unexpanded `${VAR}` placeholder.
/// MCP clients substitute `${PROXMOX_TOKEN}` in a shared config from the
/// user's environment, and when the variable isn't exported Claude Code
/// passes the placeholder through *literally*. Taking that as a real token
/// would shadow a `pass_entry` (or the cluster file's token) and fail every
/// call with a 401; treating it as unset falls through to the cluster file,
/// as an empty value does.
fn non_blank(v: Option<String>) -> Option<String> {
    v.filter(|s| !is_unset_value(s))
}

fn is_unset_value(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || is_unexpanded_placeholder(t)
}

/// `${NAME}` or `${NAME:-…}` exactly, as left behind by a client that didn't
/// substitute it. No real token or URL has this shape.
fn is_unexpanded_placeholder(t: &str) -> bool {
    t.strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|inner| {
            let name = inner.split_once(":-").map_or(inner, |(n, _)| n);
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

/// An environment variable, treating blank values and unexpanded `${…}`
/// placeholders as unset (see [`non_blank`]).
pub fn env_var(name: &str) -> Option<String> {
    let raw = std::env::var(name).ok()?;
    if is_unexpanded_placeholder(raw.trim()) {
        tracing::warn!(
            variable = name,
            "ignoring unexpanded placeholder (the MCP client did not substitute it; \
             the variable is not set in its environment)"
        );
        return None;
    }
    non_blank(Some(raw))
}

fn resolve_cluster(
    raw: &ClusterFile,
    env_token: Option<String>,
    dir: &Path,
) -> anyhow::Result<Connection> {
    let url = raw.url.clone().ok_or_else(|| {
        if raw.source.is_some() {
            anyhow!("Proxmox URL not set: set \"url\" in the cluster file, or provide PROXMOX_URL")
        } else {
            anyhow!(
                "no Proxmox cluster configured: add a cluster file such as {} \
                 ({{\"url\": ..., \"token\": ...}}), set PROXMOX_URL and PROXMOX_TOKEN, \
                 or run `proxmox-mcp login --cluster pve --url ...`",
                dir.join("pve.json").display()
            )
        }
    })?;
    let url = api_url(&url)?;

    // PROXMOX_TOKEN wins over both file sources. Setting both of those is an
    // error: either one silently shadowing the other would be surprising.
    let credential = match (env_token, &raw.token, &raw.pass_entry) {
        (Some(t), _, _) => Credential::Env(t),
        (None, Some(_), Some(_)) => bail!(
            "the cluster file sets both \"token\" and \"pass_entry\"; remove the plaintext \
             \"token\" (`proxmox-mcp login` does this for you)"
        ),
        (None, Some(t), None) => Credential::File(t.clone()),
        (None, None, Some(entry)) => Credential::Pass(raw.pass_store()?.entry(entry)?),
        (None, None, None) => bail!(
            "Proxmox token not set: set \"token\" in the cluster file (format: \
             USER@REALM!TOKENID=UUID), provide PROXMOX_TOKEN, or run `proxmox-mcp login` \
             to store it GPG-encrypted"
        ),
    };

    Ok(Connection {
        url,
        credential,
        insecure: raw.insecure.unwrap_or(false),
    })
}

/// Validate a configured URL (HTTPS only) and normalize it to the API base.
pub fn api_url(url: &str) -> anyhow::Result<String> {
    enforce_https(url)?;
    Ok(normalize_url(url))
}

/// Cluster names are echoed in tool output and passed back as arguments, so
/// keep them to a plain identifier charset. `*` is reserved for "all clusters".
pub fn validate_cluster_name(name: &str) -> anyhow::Result<()> {
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

pub fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot determine home directory")
}

/// `$XDG_CONFIG_HOME/proxmox-mcp/clusters`, falling back to
/// `~/.config/proxmox-mcp/clusters` (XDG says to ignore a relative value).
fn default_config_dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(xdg) if xdg.is_absolute() => xdg,
        _ => home_dir()?.join(".config"),
    };
    Ok(base.join("proxmox-mcp").join("clusters"))
}

/// Apply `edit` to the JSON object in the cluster file at `path` (created,
/// along with its directory, if absent) and write it back atomically with
/// owner-only permissions. A symlinked file is updated at its target rather
/// than replaced.
pub fn update_cluster_file(
    path: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> anyhow::Result<()> {
    let present = path
        .try_exists()
        .with_context(|| format!("checking {}", path.display()))?;
    let target = if present {
        check_file_permissions(path).with_context(|| format!("cluster file {}", path.display()))?;
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?
    } else {
        if let Some(dir) = path.parent() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(dir)
                .with_context(|| format!("creating config directory {}", dir.display()))?;
        }
        path.to_path_buf()
    };
    let mut obj = if present {
        let contents = std::fs::read_to_string(&target)
            .with_context(|| format!("reading cluster file {}", target.display()))?;
        match serde_json::from_str(&contents)
            .with_context(|| format!("parsing cluster file {}", target.display()))?
        {
            serde_json::Value::Object(m) => m,
            _ => bail!("cluster file {} is not a JSON object", target.display()),
        }
    } else {
        serde_json::Map::new()
    };
    edit(&mut obj);

    let mut tmp_name = target.clone().into_os_string();
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write = || -> anyhow::Result<()> {
        let mut file = opts.open(&tmp)?;
        let mut body = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
        body.push('\n');
        std::io::Write::write_all(&mut file, body.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        Ok(())
    };
    write()
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("writing cluster file {}", target.display()))
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
pub mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    // Cargo runs unit tests on multiple threads in one process. Tests that touch
    // PROXMOX_* env vars must serialize, or one test's set_var leaks into
    // another's resolve(). Hold this guard for the whole test body.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub fn lock_env() -> MutexGuard<'static, ()> {
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

    /// A plaintext (env or file) token; panics for a pass entry.
    fn token(conn: &Connection) -> &str {
        match &conn.credential {
            Credential::Env(t) | Credential::File(t) => t,
            Credential::Pass(e) => {
                panic!("expected a plaintext token, got pass entry {}", e.name())
            }
        }
    }

    fn pass_entry(conn: &Connection) -> &PassEntry {
        match &conn.credential {
            Credential::Pass(e) => e,
            other => panic!("expected a pass entry, got {other:?}"),
        }
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
        assert_eq!(token(&c.entries["lab"]), "lab@pam!t=x");
        assert!(!c.entries["lab"].insecure, "insecure defaults to false");
        assert_eq!(token(&c.entries["prod"]), "prod@pam!t=y");
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
        assert_eq!(token(&c.entries["prod"]), "env@pam!t=z");
        assert!(c.entries["prod"].insecure);
        assert_eq!(token(&c.entries["lab"]), "lab@pam!t=x");
    }

    #[test]
    fn blank_env_vars_do_not_override_the_cluster_file() {
        let _guard = lock_env();
        clear_env();
        let cfg = load(&[("lab.json", LAB)]).unwrap();
        set_env(&[
            ("PROXMOX_URL", ""),
            ("PROXMOX_TOKEN", "  "),
            ("PROXMOX_INSECURE", ""),
            ("PROXMOX_CLUSTER", ""),
        ]);
        let c = cfg.resolve();
        clear_env();
        let c = c.unwrap();
        assert_eq!(c.default, "lab");
        let conn = default_conn(&c);
        assert_eq!(conn.url, "https://lab.example.com:8006/api2/json");
        assert_eq!(token(conn), "lab@pam!t=x");
        assert!(!conn.insecure);
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
        for credential in [
            Credential::File("root@pam!mcp=supersecret".into()),
            Credential::Env("root@pam!mcp=supersecret".into()),
        ] {
            let conn = Connection {
                url: "https://pve.example.com/api2/json".into(),
                credential,
                insecure: false,
            };
            let dbg = format!("{conn:?}");
            assert!(!dbg.contains("supersecret"), "{dbg}");
        }
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

    const PASS: &str = r#"{"url":"https://lab.example.com","pass_entry":"proxmox-mcp/lab","password_store_dir":"/stores/pass","gnupg_home":"/keys/gnupg"}"#;

    #[test]
    fn pass_entry_resolves_to_the_configured_store() {
        let _guard = lock_env();
        let c = resolve(&[("lab.json", PASS)]).unwrap();
        let entry = pass_entry(default_conn(&c));
        assert_eq!(entry.name(), "proxmox-mcp/lab");
        assert_eq!(entry.path(), Path::new("/stores/pass/proxmox-mcp/lab.gpg"));
        assert_eq!(entry.store().homedir(), Some(Path::new("/keys/gnupg")));
    }

    #[test]
    fn token_and_pass_entry_together_is_error() {
        let _guard = lock_env();
        let both = r#"{"url":"https://lab.example.com","token":"a@pam!t=x","pass_entry":"p"}"#;
        let err = resolve(&[("lab.json", both)]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("both") && msg.contains("lab.json"), "{msg}");
    }

    #[test]
    fn invalid_pass_entry_name_is_rejected() {
        let _guard = lock_env();
        let bad = r#"{"url":"https://lab.example.com","pass_entry":"../escape"}"#;
        let err = resolve(&[("lab.json", bad)]).unwrap_err();
        assert!(format!("{err:#}").contains("invalid pass entry"), "{err:#}");
    }

    #[test]
    fn env_token_beats_pass_entry_on_the_default_cluster_only() {
        let _guard = lock_env();
        clear_env();
        let other =
            r#"{"url":"https://o.example.com","pass_entry":"proxmox-mcp/o","default":true}"#;
        let cfg = load(&[("lab.json", PASS), ("other.json", other)]).unwrap();
        set_env(&[("PROXMOX_TOKEN", "env@pam!t=z")]);
        let c = cfg.resolve();
        clear_env();
        let c = c.unwrap();
        assert!(matches!(c.entries["other"].credential, Credential::Env(_)));
        assert_eq!(pass_entry(&c.entries["lab"]).name(), "proxmox-mcp/lab");
    }

    #[test]
    // `${X}` etc. are shell-style placeholders under test, not format args.
    #[allow(clippy::literal_string_with_formatting_args)]
    fn placeholder_env_vars_fall_back_to_the_cluster_file() {
        let _guard = lock_env();
        clear_env();
        let cfg = load(&[("lab.json", PASS)]).unwrap();
        set_env(&[
            ("PROXMOX_TOKEN", "${PROXMOX_TOKEN}"),
            ("PROXMOX_URL", "${PROXMOX_URL:-}"),
            ("PROXMOX_CLUSTER", "${PROXMOX_CLUSTER}"),
        ]);
        let c = cfg.resolve();
        clear_env();
        let c = c.unwrap();
        let conn = default_conn(&c);
        assert_eq!(conn.url, "https://lab.example.com/api2/json");
        assert_eq!(pass_entry(conn).name(), "proxmox-mcp/lab");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn placeholder_in_a_cluster_file_counts_as_unset() {
        let _guard = lock_env();
        let file =
            r#"{"url":"https://lab.example.com","token":"${PROXMOX_TOKEN}","pass_entry":"p"}"#;
        let c = resolve(&[("lab.json", file)]).unwrap();
        assert_eq!(pass_entry(default_conn(&c)).name(), "p");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn unexpanded_placeholders_are_recognised() {
        for p in [
            "${PROXMOX_TOKEN}",
            "${PROXMOX_URL:-https://pve}",
            "${A_1}",
            "${X:-}",
        ] {
            assert!(is_unexpanded_placeholder(p), "{p}");
        }
        for v in [
            "root@pam!mcp=abc",
            "https://pve.example.com",
            "${}",
            "$PROXMOX_TOKEN",
            "${PROXMOX_TOKEN",
            "x${PROXMOX_TOKEN}",
            "${not valid}",
        ] {
            assert!(!is_unexpanded_placeholder(v), "{v}");
        }
    }

    /// Every MCP client config documented in the README must give each
    /// `${VAR}` in `env` a `:-` default. Claude Code passes a bare `${VAR}`
    /// through *literally* when the variable is unset (gitlab-mcp v0.39.0
    /// shipped exactly that bug). The server ignores such values too, but the
    /// documented configs shouldn't depend on that.
    #[test]
    fn documented_mcp_configs_default_every_placeholder() {
        let readme =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
        let mut checked = 0;
        for (i, block) in readme.split("```json\n").skip(1).enumerate() {
            let body = block.split("```").next().unwrap();
            if !body.contains("mcpServers") {
                continue;
            }
            let config: serde_json::Value =
                serde_json::from_str(body).unwrap_or_else(|e| panic!("README json block {i}: {e}"));
            for (server, def) in config["mcpServers"].as_object().unwrap() {
                let Some(env) = def["env"].as_object() else {
                    continue;
                };
                for (key, value) in env {
                    let value = value.as_str().unwrap_or_default();
                    for placeholder in value.split("${").skip(1) {
                        let inner = placeholder.split('}').next().unwrap_or_default();
                        assert!(
                            inner.contains(":-"),
                            "README json block {i}: {server}.env.{key} uses ${{{inner}}} \
                             without a `:-` default; write ${{{inner}:-}}"
                        );
                    }
                }
            }
            checked += 1;
        }
        assert!(checked >= 1, "expected an mcpServers example in the README");
    }

    #[cfg(unix)]
    #[test]
    fn update_cluster_file_creates_private_files_and_keeps_other_keys() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clusters/lab.json");
        update_cluster_file(&path, |obj| {
            obj.insert("url".into(), "https://a.example.com".into());
            obj.insert("token".into(), "a@pam!t=x".into());
            obj.insert("default".into(), true.into());
        })
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o077, 0, "{dir_mode:o}");

        update_cluster_file(&path, |obj| {
            obj.remove("token");
            obj.insert("pass_entry".into(), "proxmox-mcp/a".into());
        })
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "url": "https://a.example.com",
                "default": true,
                "pass_entry": "proxmox-mcp/a"
            })
        );
    }
}
