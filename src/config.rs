use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

/// Raw config as loaded from the JSON file.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    url: Option<String>,
    token: Option<String>,
    insecure: Option<bool>,
}

/// Resolved Proxmox connection settings, ready to build a client.
#[derive(Debug, Clone)]
pub struct Connection {
    /// Base API URL, e.g. `https://pve.example.com:8006/api2/json`.
    pub url: String,
    /// Full API token: `USER@REALM!TOKENID=UUID`.
    pub token: String,
    /// Accept invalid/self-signed TLS certificates (homelab default).
    pub insecure: bool,
}

/// Config as loaded from file, before env-var resolution.
#[derive(Debug, Clone)]
pub struct Config {
    file_url: Option<String>,
    file_token: Option<String>,
    file_insecure: Option<bool>,
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
            return Ok(Config {
                file_url: None,
                file_token: None,
                file_insecure: None,
            });
        }

        check_file_permissions(&resolved)?;

        let contents = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading config file {}", resolved.display()))?;
        let raw: RawConfig = serde_json::from_str(&contents)
            .with_context(|| format!("parsing config file {}", resolved.display()))?;

        Ok(Config {
            file_url: raw.url,
            file_token: raw.token,
            file_insecure: raw.insecure,
        })
    }

    /// Resolve the full connection: env vars override the config file.
    /// `PROXMOX_URL`, `PROXMOX_TOKEN`, `PROXMOX_INSECURE` (truthy = 1/true/yes).
    pub fn resolve(&self) -> anyhow::Result<Connection> {
        let url = std::env::var("PROXMOX_URL")
            .ok()
            .or_else(|| self.file_url.clone())
            .ok_or_else(|| {
                anyhow!("Proxmox URL not set: provide PROXMOX_URL or set \"url\" in config file")
            })?;
        enforce_https(&url)?;
        let url = normalize_url(&url);

        let token = std::env::var("PROXMOX_TOKEN")
            .ok()
            .or_else(|| self.file_token.clone())
            .ok_or_else(|| {
                anyhow!(
                    "Proxmox token not set: provide PROXMOX_TOKEN or set \"token\" in config file \
                     (format: USER@REALM!TOKENID=UUID)"
                )
            })?;

        let insecure = match std::env::var("PROXMOX_INSECURE") {
            Ok(v) => parse_bool(&v),
            Err(_) => self.file_insecure.unwrap_or(false),
        };

        Ok(Connection {
            url,
            token,
            insecure,
        })
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
        "Proxmox URL must use HTTPS, got: {}  \
         (use https://; for self-signed certs set \"insecure\": true instead)",
        url
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
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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

    #[test]
    fn missing_file_returns_empty() {
        let cfg = Config::load(Some(Path::new("/nonexistent/path.json"))).unwrap();
        assert!(cfg.file_url.is_none());
        assert!(cfg.file_token.is_none());
    }

    #[test]
    fn parses_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"url":"https://pve.example.com:8006/api2/json","token":"root@pam!mcp=uuid","insecure":true}"#,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(
            cfg.file_url.as_deref(),
            Some("https://pve.example.com:8006/api2/json")
        );
        assert_eq!(cfg.file_token.as_deref(), Some("root@pam!mcp=uuid"));
        assert_eq!(cfg.file_insecure, Some(true));
    }

    #[test]
    fn rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "not json");
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn resolve_uses_file_values() {
        let _guard = lock_env();
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"url":"https://file.example.com:8006/api2/json","token":"u@pam!t=x","insecure":true}"#,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        let conn = cfg.resolve().unwrap();
        assert_eq!(conn.url, "https://file.example.com:8006/api2/json");
        assert_eq!(conn.token, "u@pam!t=x");
        assert!(conn.insecure);
    }

    #[test]
    fn env_overrides_file() {
        let _guard = lock_env();
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"url":"https://file.example.com","token":"file@pam!t=x","insecure":false}"#,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        // SAFETY: ENV_LOCK serializes all env-touching tests in this module.
        unsafe {
            std::env::set_var("PROXMOX_URL", "https://env.example.com:8006/api2/json");
            std::env::set_var("PROXMOX_TOKEN", "env@pam!t=y");
            std::env::set_var("PROXMOX_INSECURE", "yes");
        }
        let conn = cfg.resolve().unwrap();
        clear_env();
        assert_eq!(conn.url, "https://env.example.com:8006/api2/json");
        assert_eq!(conn.token, "env@pam!t=y");
        assert!(conn.insecure);
    }

    #[test]
    fn insecure_defaults_false() {
        let _guard = lock_env();
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"url":"https://pve.example.com","token":"u@pam!t=x"}"#,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        let conn = cfg.resolve().unwrap();
        assert!(!conn.insecure);
    }

    #[test]
    fn missing_url_is_error() {
        let _guard = lock_env();
        clear_env();
        let cfg = Config {
            file_url: None,
            file_token: Some("u@pam!t=x".into()),
            file_insecure: None,
        };
        assert!(cfg.resolve().is_err());
    }

    #[test]
    fn missing_token_is_error() {
        let _guard = lock_env();
        clear_env();
        let cfg = Config {
            file_url: Some("https://pve.example.com".into()),
            file_token: None,
            file_insecure: None,
        };
        assert!(cfg.resolve().is_err());
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
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"url":"https://pve.example.com:8006","token":"u@pam!t=x"}"#,
        );
        let cfg = Config::load(Some(&path)).unwrap();
        let conn = cfg.resolve().unwrap();
        assert_eq!(conn.url, "https://pve.example.com:8006/api2/json");
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
