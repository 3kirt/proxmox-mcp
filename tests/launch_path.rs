//! End-to-end tests of the *built binary*, launched the way an MCP client
//! launches it: a spawned process speaking MCP over stdio, configured only
//! through its environment and a cluster file.
//!
//! Unit tests call domain functions directly and so can't see what a client
//! puts in the process environment. gitlab-mcp v0.39.0 shipped a bug that
//! lived exactly there: for `"GITLAB_TOKEN": "${GITLAB_TOKEN}"` with the
//! variable unset, Claude Code passes the placeholder text through literally,
//! the server took it as a real token, and a configured `pass_entry` was never
//! used. These tests drive each way a client can hand over `PROXMOX_TOKEN`
//! and assert the request reaching Proxmox carries the token decrypted from
//! the pass entry.
//!
//! The server refuses plain HTTP, so "Proxmox" here is a minimal HTTPS
//! responder with a self-signed certificate (the cluster file sets
//! `"insecure": true`, as a homelab would).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Deliberately not shaped like a real token secret (no UUID).
const PASS_TOKEN: &str = "launch@pve!test=not-a-real-secret";
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// A stand-in for `gpg --decrypt` that prints the token with the status lines
/// real GnuPG emits, so the server's status-code classification is exercised.
fn write_fake_gpg(dir: &Path) -> PathBuf {
    let gpg = dir.join("gpg");
    std::fs::write(
        &gpg,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' '{PASS_TOKEN}'\n\
             echo '[GNUPG:] BEGIN_DECRYPTION' >&2\n\
             echo '[GNUPG:] DECRYPTION_OKAY' >&2\n"
        ),
    )
    .unwrap();
    set_mode(&gpg, 0o755);
    gpg
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// A tiny HTTPS "Proxmox": answers every request with `/version` data when
/// the `Authorization` header carries [`PASS_TOKEN`], else 401, and records
/// each header it saw.
struct FakeProxmox {
    url: String,
    seen_auth: Arc<Mutex<Vec<String>>>,
}

async fn fake_proxmox() -> FakeProxmox {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let seen_auth = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&seen_auth);
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf);
                let auth = head
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_string())
                    })
                    .unwrap_or_default();
                let ok = auth == format!("PVEAPIToken={PASS_TOKEN}");
                seen.lock().unwrap().push(auth);
                let (status, body) = if ok {
                    (
                        "200 OK",
                        json!({"data": {"version": "9.0.3", "release": "9.0"}}),
                    )
                } else {
                    ("401 authentication failure", json!({"data": null}))
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    FakeProxmox { url, seen_auth }
}

/// How the client hands `PROXMOX_TOKEN` to the server.
#[derive(Clone, Copy, Debug)]
enum TokenEnv {
    /// Not in the environment at all.
    Unset,
    /// `"${PROXMOX_TOKEN:-}"` with the variable unset.
    Empty,
    /// `"${PROXMOX_TOKEN}"` with the variable unset: Claude Code's literal
    /// passthrough.
    LiteralPlaceholder,
}

/// Spawn the server against `proxmox` with a pass-entry cluster file, call
/// one tool over MCP stdio, and return the `tools/call` result.
async fn call_through_binary(proxmox: &FakeProxmox, token_env: TokenEnv) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let gpg = write_fake_gpg(dir.path());
    let store = dir.path().join("store");
    std::fs::create_dir_all(store.join("proxmox-mcp")).unwrap();
    // Content is irrelevant: the fake gpg ignores its input.
    std::fs::write(store.join("proxmox-mcp/test.gpg"), "ciphertext").unwrap();
    let clusters = dir.path().join("clusters");
    std::fs::create_dir_all(&clusters).unwrap();
    let cluster_file = clusters.join("lab.json");
    std::fs::write(
        &cluster_file,
        json!({
            "url": proxmox.url,
            "insecure": true,
            "pass_entry": "proxmox-mcp/test",
            "password_store_dir": store,
            "gpg": gpg,
        })
        .to_string(),
    )
    .unwrap();
    set_mode(&cluster_file, 0o600);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_proxmox-mcp"));
    cmd.arg("--config-dir")
        .arg(&clusters)
        // Isolate from the developer's own setup.
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("PROXMOX_URL")
        .env_remove("PROXMOX_TOKEN")
        .env_remove("PROXMOX_INSECURE")
        .env_remove("PROXMOX_CLUSTER")
        .env_remove("PASSWORD_STORE_DIR")
        .env_remove("GNUPGHOME")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match token_env {
        TokenEnv::Unset => {}
        TokenEnv::Empty => {
            cmd.env("PROXMOX_TOKEN", "");
        }
        TokenEnv::LiteralPlaceholder => {
            cmd.env("PROXMOX_TOKEN", "${PROXMOX_TOKEN}");
        }
    }
    let mut child = cmd.spawn().expect("spawn proxmox-mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();

    let mut send = async |msg: Value| {
        stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
    };
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "launch-path-test", "version": "0"}
        }}),
    )
    .await;
    read_response(&mut stdout, 1).await;
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "proxmox_version_get", "arguments": {}
        }}),
    )
    .await;
    let response = read_response(&mut stdout, 2).await;
    response["result"].clone()
}

/// Read stdout lines until the JSON-RPC response with `id` arrives.
async fn read_response<R>(lines: &mut tokio::io::Lines<R>, id: u64) -> Value
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    tokio::time::timeout(STEP_TIMEOUT, async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("server closed stdout before responding");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == id {
                return msg;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no response to request {id} within {STEP_TIMEOUT:?}"))
}

async fn assert_uses_pass_token(token_env: TokenEnv) {
    let proxmox = fake_proxmox().await;
    let result = call_through_binary(&proxmox, token_env).await;
    let seen = proxmox.seen_auth.lock().unwrap().clone();
    assert_ne!(
        result["isError"],
        json!(true),
        "{token_env:?}: tool call failed: {result} (Authorization headers seen: {seen:?})"
    );
    assert!(
        result.to_string().contains("9.0.3"),
        "{token_env:?}: unexpected result: {result}"
    );
    assert_eq!(
        seen,
        vec![format!("PVEAPIToken={PASS_TOKEN}")],
        "{token_env:?}"
    );
}

#[tokio::test]
async fn pass_entry_is_used_when_token_env_is_unset() {
    assert_uses_pass_token(TokenEnv::Unset).await;
}

#[tokio::test]
async fn pass_entry_is_used_when_token_env_is_empty() {
    assert_uses_pass_token(TokenEnv::Empty).await;
}

#[tokio::test]
async fn pass_entry_is_used_when_client_passes_a_literal_placeholder() {
    assert_uses_pass_token(TokenEnv::LiteralPlaceholder).await;
}
