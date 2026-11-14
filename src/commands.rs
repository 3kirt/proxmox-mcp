//! `login` / `logout` / `unlock` / `status`: managing GPG-encrypted cluster
//! tokens in a `pass`-compatible store. These run in the user's terminal, so
//! (unlike the server) they may prompt. All output goes to stderr, matching
//! the server's rule that stdout is reserved for MCP.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};

use crate::client::ProxmoxClient;
use crate::config::{self, Config, Connection, Credential};
use crate::credentials::{PassEntry, PassError, PassStore};

/// Proxmox's standard API port, left out of default entry names.
const PROXMOX_PORT: u16 = 8006;

#[derive(clap::Args)]
pub struct LoginArgs {
    /// Cluster to log in to; writes <config-dir>/<CLUSTER>.json (default: the
    /// default cluster)
    #[arg(long)]
    cluster: Option<String>,

    /// Proxmox URL, e.g. https://pve.example.com:8006 (default: "url" in the
    /// cluster file, then PROXMOX_URL)
    #[arg(long)]
    url: Option<String>,

    /// Accept a self-signed TLS certificate (records "insecure": true)
    #[arg(long)]
    insecure: bool,

    /// Password-store entry to write (default: the cluster file's
    /// "pass_entry", else proxmox-mcp/<host>)
    #[arg(long)]
    entry: Option<String>,

    /// GPG key to encrypt to when the store has no .gpg-id yet (writes it,
    /// like `pass init`)
    #[arg(long, value_name = "KEY_ID")]
    gpg_id: Option<String>,

    /// Read the token from stdin instead of prompting
    #[arg(long)]
    token_stdin: bool,
}

#[derive(clap::Args)]
pub struct LogoutArgs {
    /// Cluster whose stored token to delete (default: the default cluster)
    #[arg(long)]
    cluster: Option<String>,

    /// Password-store entry to delete (default: the cluster file's "pass_entry")
    #[arg(long)]
    entry: Option<String>,
}

#[derive(clap::Args)]
pub struct ClusterArgs {
    /// Only this cluster (default: every configured cluster)
    #[arg(long)]
    cluster: Option<String>,
}

/// Verify a token, encrypt it into the store, and point the cluster file at it.
pub async fn login(config_dir: Option<&Path>, args: LoginArgs) -> anyhow::Result<()> {
    let (cfg, skipped) = Config::load_or_empty(config_dir)?;
    let explicit_cluster = args.cluster.is_some();
    let cluster = target_cluster(&cfg, args.cluster)?;
    let target_path = cfg.cluster_path(&cluster);
    check_skipped(skipped, &target_path, explicit_cluster)?;
    let (url, url_from_env) = match args.url {
        Some(u) => (u.trim().to_string(), false),
        None => match (cfg.file_url(&cluster), config::env_var("PROXMOX_URL")) {
            (Some(u), _) => (u.to_string(), false),
            (None, Some(u)) => (u, true),
            (None, None) => bail!(
                "no URL for cluster \"{cluster}\": pass --url https://<host>:8006 \
                 (or set \"url\" in {})",
                cfg.cluster_path(&cluster).display()
            ),
        },
    };
    let api_url = config::api_url(&url)?;
    let insecure = args.insecure
        || cfg
            .file_insecure(&cluster)
            .or_else(|| config::env_var("PROXMOX_INSECURE").map(|v| config::parse_bool(&v)))
            .unwrap_or(false);
    let entry_name = choose_entry_name(args.entry, &cfg, &cluster, &url)?;
    let store = cfg.pass_store(&cluster)?;
    let entry = store.entry(&entry_name)?;

    // Settle every local precondition before prompting for the secret or
    // calling Proxmox, so a missing .gpg-id doesn't waste a token entry, and
    // an unwritable cluster file can't leave the store changed (entry and
    // .gpg-id written) with nothing pointing at it.
    config::check_writable(&target_path)?;
    let (recipients, init_id) = recipients_for(&store, &entry, args.gpg_id)?;

    let token = read_token_input(args.token_stdin)?;
    let version = ProxmoxClient::new(Connection {
        url: api_url,
        credential: Credential::File(token.clone()),
        insecure,
    })?
    .get("/version", &[])
    .await
    .map_err(|e| anyhow!("Proxmox rejected the token: {}", e.to_tool_message()))?;
    let release = version["version"].as_str().unwrap_or("<unknown>");

    entry.write_token(&token, &recipients).await?;
    // Only now, with encryption to that key proven, record it as the store's
    // recipient: a mistyped --gpg-id must not leave a .gpg-id behind that
    // blocks every later login.
    if let Some(id) = &init_id {
        store.init(id)?;
        eprintln!(
            "Initialised password store at {} for {id}",
            store.root().display()
        );
    }

    let gpg_path = find_on_path(store.gpg());
    let path = target_path;
    let previous_url = cfg.file_url(&cluster).map(str::to_owned);
    let mut removed_plaintext = false;
    config::update_cluster_file(&path, |obj| {
        obj.insert("url".into(), url.clone().into());
        obj.insert("pass_entry".into(), entry_name.clone().into());
        // Pin the store and keyring login used, so a server launched without
        // this shell's PASSWORD_STORE_DIR / GNUPGHOME reads the same ones.
        obj.insert(
            "password_store_dir".into(),
            store.root().display().to_string().into(),
        );
        if let Some(home) = store.homedir() {
            obj.insert("gnupg_home".into(), home.display().to_string().into());
        }
        // ...and the gpg binary: a client's minimal PATH may not have it.
        if let Some(gpg) = &gpg_path {
            obj.insert("gpg".into(), gpg.display().to_string().into());
        }
        // Whatever TLS setting the verification above used, including one
        // from PROXMOX_INSECURE, which a GUI-launched server won't see.
        if insecure {
            obj.insert("insecure".into(), true.into());
        }
        removed_plaintext = obj.remove("token").is_some();
    })?;

    eprintln!(
        "Logged in to {url} (Proxmox VE {release}). Token encrypted to {} at {}.",
        recipients.join(", "),
        entry.path().display()
    );
    eprintln!(
        "Updated {} (\"pass_entry\": \"{entry_name}\").",
        path.display()
    );
    if let Some(prev) = previous_url.filter(|p| !same_url(p, &url)) {
        let why = if url_from_env {
            " (from PROXMOX_URL)"
        } else {
            ""
        };
        eprintln!("Changed \"url\" from {prev} to {url}{why}.");
    }
    if removed_plaintext {
        eprintln!("Removed the plaintext \"token\" from the cluster file.");
    }
    // A new second cluster file leaves the server without a default.
    if let Err(e) = Config::load(Some(cfg.dir())).and_then(|c| c.default_name()) {
        eprintln!("Note: {e}");
    }
    note_env_token_precedence();
    Ok(())
}

/// The recipients to encrypt to: the nearest `.gpg-id`, or `--gpg-id` when the
/// store has none yet (returned second, to be written once encryption works).
fn recipients_for(
    store: &PassStore,
    entry: &PassEntry,
    gpg_id: Option<String>,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
    Ok(match (entry.recipients()?, gpg_id) {
        (Some(_), Some(_)) => bail!(
            "the password store at {} already has a .gpg-id; drop --gpg-id",
            store.root().display()
        ),
        (Some(ids), None) => (ids, None),
        (None, Some(id)) => {
            let id = id.trim().to_string();
            if id.is_empty() {
                bail!("--gpg-id is empty");
            }
            (vec![id.clone()], Some(id))
        }
        (None, None) => bail!(
            "no .gpg-id in the password store at {}; pass --gpg-id <KEY_ID> to choose \
             the GPG key to encrypt to (see `gpg --list-secret-keys`)",
            store.root().display()
        ),
    })
}

/// Other clusters' broken files don't block a login, but the target's own
/// file must load, and the default can't be chosen without them all.
fn check_skipped(
    skipped: Vec<config::SkippedFile>,
    target_path: &Path,
    explicit_cluster: bool,
) -> anyhow::Result<()> {
    for file in skipped {
        if file.path == target_path {
            return Err(file.error.context("fix this cluster file first"));
        }
        if !explicit_cluster {
            return Err(file
                .error
                .context("fix it, or pass --cluster NAME to log in to another cluster"));
        }
        eprintln!("Warning: skipping {:#}", file.error);
    }
    Ok(())
}

/// The cluster a command acts on: `--cluster`, else the default cluster.
fn target_cluster(cfg: &Config, explicit: Option<String>) -> anyhow::Result<String> {
    if let Some(name) = explicit {
        config::validate_cluster_name(&name)?;
        return Ok(name);
    }
    cfg.default_name()?.with_context(|| {
        format!(
            "no cluster files in {}; pass --cluster NAME to create {}",
            cfg.dir().display(),
            cfg.cluster_path("NAME").display()
        )
    })
}

/// The entry to write: `--entry`, else the cluster file's `pass_entry` *if*
/// the file is for this same URL, else `proxmox-mcp/<host>`. Reusing another
/// host's entry would overwrite that host's token.
fn choose_entry_name(
    explicit: Option<String>,
    cfg: &Config,
    cluster: &str,
    url: &str,
) -> anyhow::Result<String> {
    if let Some(name) = explicit {
        return Ok(name);
    }
    match (cfg.file_pass_entry(cluster), cfg.file_url(cluster)) {
        (Some(name), Some(file_url)) if same_url(file_url, url) => Ok(name.to_owned()),
        _ => default_entry_name(url),
    }
}

/// Whether two configured URLs address the same API (`https://h:8006` and
/// `https://h:8006/api2/json/` do).
fn same_url(a: &str, b: &str) -> bool {
    config::normalize_url(a.trim()) == config::normalize_url(b.trim())
}

/// The absolute path of a bare program name (`gpg`) found on `PATH`, so
/// `login` can record it. `None` for an already-explicit path (it came from
/// the cluster file) or when it isn't found.
fn find_on_path(program: &Path) -> Option<PathBuf> {
    if program.components().count() != 1 {
        return None;
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `proxmox-mcp/<host>`, with `_<port>` for a port other than 8006.
fn default_entry_name(url: &str) -> anyhow::Result<String> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid Proxmox URL: {url}"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("Proxmox URL has no host: {url}"))?
        .trim_matches(['[', ']'])
        .replace(':', "_");
    Ok(match parsed.port() {
        Some(port) if port != PROXMOX_PORT => format!("proxmox-mcp/{host}_{port}"),
        _ => format!("proxmox-mcp/{host}"),
    })
}

fn note_env_token_precedence() {
    if config::env_var("PROXMOX_TOKEN").is_some() {
        eprintln!(
            "Note: PROXMOX_TOKEN is set in this shell and takes precedence over the default \
             cluster's pass entry for servers started from it."
        );
    }
}

/// Delete the stored token. The cluster file's `pass_entry` is left in
/// place, so the server then reports "run `proxmox-mcp login`" rather than
/// "token not set".
pub fn logout(config_dir: Option<&Path>, args: LogoutArgs) -> anyhow::Result<()> {
    let cfg = Config::load(config_dir)?;
    let cluster = target_cluster(&cfg, args.cluster)?;
    let name = args
        .entry
        .as_deref()
        .or_else(|| cfg.file_pass_entry(&cluster))
        .with_context(|| {
            format!(
                "no pass entry for cluster \"{cluster}\": pass --entry or set \"pass_entry\" \
                 in {}",
                cfg.cluster_path(&cluster).display()
            )
        })?;
    let entry = cfg.pass_store(&cluster)?.entry(name)?;
    entry.remove()?;
    eprintln!("Removed {}.", entry.path().display());
    Ok(())
}

/// The clusters `--cluster` selects: that one (which must exist), else all.
fn selected_clusters(cfg: &Config, explicit: Option<String>) -> anyhow::Result<Vec<String>> {
    match explicit {
        Some(name) if cfg.names().contains(&name) => Ok(vec![name]),
        Some(name) => bail!(
            "no cluster \"{name}\": {} does not exist",
            cfg.cluster_path(&name).display()
        ),
        None => Ok(cfg.names()),
    }
}

/// Get each pass-backed cluster's passphrase into gpg-agent's cache,
/// prompting if needed.
pub async fn unlock(config_dir: Option<&Path>, args: ClusterArgs) -> anyhow::Result<()> {
    let cfg = Config::load(config_dir)?;
    // The configured entries, not the resolved credentials: PROXMOX_TOKEN in
    // this shell says nothing about the (GUI-launched) server asking to be
    // unlocked.
    let mut entries: Vec<(String, PassEntry)> = Vec::new();
    for cluster in selected_clusters(&cfg, args.cluster)? {
        if let Some(entry) = cfg.configured_pass_entry(&cluster)? {
            entries.push((cluster, entry));
        }
    }
    if entries.is_empty() {
        bail!(
            "no cluster file in {} sets \"pass_entry\", so there is nothing to unlock",
            cfg.dir().display()
        );
    }
    note_env_token_precedence();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut failed: Vec<String> = Vec::new();
    for (cluster, entry) in entries {
        if !seen.insert(entry.path()) {
            continue;
        }
        // One cluster's problem (e.g. a logged-out entry) must not stop the
        // others from being unlocked.
        if let Err(e) = unlock_entry(&cluster, &entry).await {
            eprintln!("{cluster}: error: {e}");
            failed.push(cluster);
        }
    }
    eprintln!(
        "The server can now read the tokens until gpg-agent's cache expires \
         (default-cache-ttl / max-cache-ttl in gpg-agent.conf); a running server keeps \
         each one in memory after its first successful read."
    );
    if !failed.is_empty() {
        bail!("could not unlock: {}", failed.join(", "));
    }
    Ok(())
}

async fn unlock_entry(cluster: &str, entry: &PassEntry) -> Result<(), PassError> {
    match entry.read_token().await {
        Ok(_) => {
            eprintln!("{cluster}: already unlocked.");
            return Ok(());
        }
        Err(PassError::Locked { .. }) => {}
        Err(e) => return Err(e),
    }
    eprintln!("{cluster}: unlocking {}", entry.path().display());
    entry.unlock_interactive().await?;
    // Confirm the way the server will read it: non-interactively.
    entry.read_token().await?;
    eprintln!("{cluster}: unlocked.");
    Ok(())
}

/// Report where each cluster's token comes from and, for pass, whether it's
/// unlocked — without decrypting it.
pub async fn status(config_dir: Option<&Path>, args: ClusterArgs) -> anyhow::Result<()> {
    let cfg = Config::load(config_dir)?;
    eprintln!("Config dir:   {}", cfg.dir().display());
    let env_only = cfg.names().is_empty();
    let explicit = args.cluster.is_some();
    let mut clusters = selected_clusters(&cfg, args.cluster)?;
    // The cluster the PROXMOX_* overrides apply to, as `resolve()` picks it.
    // `None` when no default can be chosen: then no cluster gets them (the
    // server would refuse to start), rather than guessing one.
    let default = match cfg.default_name() {
        Ok(name) => {
            let name = name.unwrap_or_else(|| config::ENV_CLUSTER_NAME.to_string());
            let note = if env_only { " (environment only)" } else { "" };
            eprintln!("Default:      {name}{note}");
            Some(name)
        }
        Err(e) => {
            eprintln!("Default:      error: {e}");
            None
        }
    };
    if env_only
        && !explicit
        && let Some(name) = &default
    {
        clusters.push(name.clone());
    }
    for cluster in clusters {
        eprintln!();
        eprintln!("[{cluster}]");
        let is_default = default.as_deref() == Some(cluster.as_str());
        cluster_status(&cfg, &cluster, is_default).await;
    }
    Ok(())
}

async fn cluster_status(cfg: &Config, cluster: &str, is_default: bool) {
    let source = cfg.connection(cluster, is_default);
    match &source {
        Ok(conn) => {
            eprintln!("URL:          {}", conn.url);
            eprintln!("Token source: {}", conn.credential.describe());
        }
        Err(e) => eprintln!("Error:        {e:#}"),
    }
    // Report the configured entry even when PROXMOX_TOKEN shadows it here: a
    // server started without that variable still uses it.
    let entry = match cfg.configured_pass_entry(cluster) {
        Ok(Some(entry)) => entry,
        Ok(None) => return,
        Err(e) => {
            eprintln!("Pass entry:   error: {e:#}");
            return;
        }
    };
    let via_pass = matches!(&source, Ok(c) if matches!(c.credential, Credential::Pass(_)));
    if !via_pass {
        eprintln!(
            "Pass entry:   {} ({})",
            entry.name(),
            entry.path().display()
        );
    }
    if matches!(&source, Ok(c) if matches!(c.credential, Credential::Env(_))) {
        eprintln!(
            "Note:         PROXMOX_TOKEN in this shell takes precedence; servers started \
             without it use the pass entry."
        );
    }
    if !entry.path().exists() {
        eprintln!("Entry:        missing (run `proxmox-mcp login --cluster {cluster}`)");
        return;
    }
    let recipients = match entry.recipients() {
        Ok(Some(r)) => r,
        Ok(None) => {
            eprintln!("Recipients:   unknown (no .gpg-id in the store)");
            return;
        }
        Err(e) => {
            eprintln!("Recipients:   error: {e}");
            return;
        }
    };
    eprintln!("Recipients:   {}", recipients.join(", "));
    eprintln!(
        "gpg-agent:    {}",
        agent_state(entry.store(), &recipients).await
    );
}

#[cfg(unix)]
async fn agent_state(store: &PassStore, recipients: &[String]) -> String {
    use crate::credentials::agent;
    let result = async {
        let grips = agent::encryption_keygrips(store, recipients).await?;
        if grips.is_empty() {
            return Ok(agent::AgentState::NoSecretKey);
        }
        let socket = agent::agent_socket(store).await?;
        Ok::<_, String>(
            agent::query_socket(&socket, &grips)
                .await?
                .map_or(agent::AgentState::NotRunning, |infos| {
                    agent::summarize(&infos)
                }),
        )
    }
    .await;
    result.map_or_else(|e| format!("unknown ({e})"), |s| s.to_string())
}

#[cfg(not(unix))]
async fn agent_state(_: &PassStore, _: &[String]) -> String {
    "unknown (gpg-agent query is only supported on Unix)".into()
}

fn read_token_input(from_stdin: bool) -> anyhow::Result<String> {
    let token = if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line
    } else if std::io::stdin().is_terminal() {
        rpassword::prompt_password("Proxmox API token (USER@REALM!TOKENID=SECRET): ")?
    } else {
        bail!("stdin is not a terminal; pass --token-stdin to read the token from it");
    };
    parse_token(&token)
}

/// Accept the token as Proxmox shows it (`USER@REALM!TOKENID=SECRET`), also
/// tolerating a pasted `PVEAPIToken=` header prefix.
fn parse_token(input: &str) -> anyhow::Result<String> {
    let token = input.trim();
    let token = token.strip_prefix("PVEAPIToken=").unwrap_or(token);
    let well_formed = token
        .split_once('=')
        .and_then(|(id, secret)| Some((id.split_once('!')?, secret)))
        .is_some_and(|((user, token_id), secret)| {
            user.contains('@') && !token_id.is_empty() && !secret.is_empty()
        });
    if !well_formed {
        bail!(
            "expected the full API token as USER@REALM!TOKENID=SECRET \
             (e.g. monitoring@pve!mcp=<uuid>)"
        );
    }
    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(files: &[(&str, &str)]) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        for (name, json) in files {
            let path = dir.path().join(name);
            std::fs::write(&path, json).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        let cfg = Config::load(Some(dir.path())).unwrap();
        (dir, cfg)
    }

    #[test]
    fn existing_entry_is_reused_only_for_the_same_url() {
        let (_d, c) = cfg(&[(
            "lab.json",
            r#"{"url":"https://pve.lab:8006/","pass_entry":"mine/lab"}"#,
        )]);
        assert_eq!(
            choose_entry_name(None, &c, "lab", "https://pve.lab:8006/api2/json").unwrap(),
            "mine/lab"
        );
        // A different host must not overwrite the lab token.
        assert_eq!(
            choose_entry_name(None, &c, "lab", "https://pve.work:8006").unwrap(),
            "proxmox-mcp/pve.work"
        );
        assert_eq!(
            choose_entry_name(None, &c, "new", "https://pve.lab:8006").unwrap(),
            "proxmox-mcp/pve.lab"
        );
        assert_eq!(
            choose_entry_name(Some("x/y".into()), &c, "lab", "https://pve.work").unwrap(),
            "x/y"
        );
    }

    #[test]
    fn default_entry_name_uses_host_and_non_standard_port() {
        assert_eq!(
            default_entry_name("https://pve.example.com:8006").unwrap(),
            "proxmox-mcp/pve.example.com"
        );
        assert_eq!(
            default_entry_name("https://pve.example.com").unwrap(),
            "proxmox-mcp/pve.example.com"
        );
        assert_eq!(
            default_entry_name("https://pve.example.com:8443/api2/json").unwrap(),
            "proxmox-mcp/pve.example.com_8443"
        );
        assert_eq!(
            default_entry_name("https://[::1]:9006").unwrap(),
            "proxmox-mcp/__1_9006"
        );
    }

    #[test]
    fn token_input_must_be_a_full_api_token() {
        assert_eq!(
            parse_token("  monitoring@pve!mcp=abc-123\n").unwrap(),
            "monitoring@pve!mcp=abc-123"
        );
        assert_eq!(
            parse_token("PVEAPIToken=root@pam!t=s").unwrap(),
            "root@pam!t=s"
        );
        for bad in [
            "",
            "abc-123",
            "root@pam!mcp",
            "root@pam=abc",
            "root!t=abc",
            "a@b!=x",
            "a@b!t=",
        ] {
            assert!(parse_token(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn target_cluster_defaults_to_the_only_cluster() {
        // default_name() reads PROXMOX_CLUSTER, which config tests set.
        let _guard = crate::config::tests::lock_env();
        let (_d, c) = cfg(&[("lab.json", r#"{"url":"https://pve.lab"}"#)]);
        assert_eq!(target_cluster(&c, None).unwrap(), "lab");
        assert_eq!(target_cluster(&c, Some("new".into())).unwrap(), "new");
        assert!(target_cluster(&c, Some("bad name".into())).is_err());
    }

    #[test]
    fn only_the_target_clusters_broken_file_blocks_login() {
        let skipped = |name: &str| config::SkippedFile {
            path: PathBuf::from(format!("/c/{name}.json")),
            error: anyhow!("cluster file /c/{name}.json: bad JSON"),
        };
        let target = Path::new("/c/lab.json");
        assert!(check_skipped(vec![skipped("other")], target, true).is_ok());
        let err = check_skipped(vec![skipped("lab")], target, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("fix this cluster file"),
            "{err:#}"
        );
        // Without --cluster the default depends on every file.
        let err = check_skipped(vec![skipped("other")], target, false).unwrap_err();
        assert!(format!("{err:#}").contains("--cluster"), "{err:#}");
    }

    #[test]
    fn same_url_ignores_api_path_slashes_and_whitespace() {
        assert!(same_url("https://h:8006", " https://h:8006/api2/json/ "));
        assert!(!same_url("https://h:8006", "https://h:8443"));
    }

    #[test]
    fn explicit_gpg_paths_are_not_looked_up() {
        assert_eq!(find_on_path(Path::new("/opt/homebrew/bin/gpg")), None);
        assert_eq!(find_on_path(Path::new("no-such-program-pmx")), None);
    }

    #[tokio::test]
    async fn unlock_continues_past_a_broken_entry() {
        use crate::credentials::test_support::{fake_gpg, ok_snippet};
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        // "a" was logged out (no entry); "b" decrypts without a prompt.
        std::fs::write(store.join("b.gpg"), ok_snippet("b@pve!t=s")).unwrap();
        let file = |entry: &str| {
            serde_json::json!({
                "url": "https://pve.example.com",
                "pass_entry": entry,
                "password_store_dir": store,
                "gpg": fake_gpg(),
            })
            .to_string()
        };
        let a = file("a");
        let b = file("b");
        let (d, _) = cfg(&[("a.json", &a), ("b.json", &b)]);
        let err = unlock(Some(d.path()), ClusterArgs { cluster: None })
            .await
            .unwrap_err();
        // "b" was still checked (and is fine); only "a" is reported.
        assert_eq!(err.to_string(), "could not unlock: a");
    }
}
