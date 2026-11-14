use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use thiserror::Error;
use tracing::{debug, trace};

use crate::config::{Connection, Credential};
use crate::credentials::PassEntry;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Tracing target for request/response logs; matches the `--debug` filter
/// directive (`proxmox_mcp=debug`) set in `main`.
const LOG_TARGET: &str = "proxmox_mcp";

#[derive(Debug, Error)]
pub enum ProxmoxError {
    #[error("Proxmox API error {status}: {body}")]
    Api { status: StatusCode, body: String },
    #[error(
        "Proxmox reported errors for this request (often a missing token privilege, \
         e.g. Datastore.Audit on the storage): {0}"
    )]
    Partial(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// The token couldn't be obtained (e.g. its GPG key is locked). The
    /// message is written for the MCP client to relay to the user.
    #[error("{0}")]
    Credentials(String),
}

impl ProxmoxError {
    /// Returns a message safe to forward to the MCP client.
    /// Truncates the API error body to 300 bytes so values echoed in 4xx
    /// responses don't blow up the assistant's context window.
    pub fn to_tool_message(&self) -> String {
        match self {
            Self::Api { status, body } => {
                let cut = body.char_indices().nth(300).map_or(body.len(), |(i, _)| i);
                if cut < body.len() {
                    format!("Proxmox API error {status}: {}… (truncated)", &body[..cut])
                } else {
                    format!("Proxmox API error {status}: {body}")
                }
            }
            other => other.to_string(),
        }
    }
}

/// Thin HTTP client for the Proxmox VE REST API.
///
/// All responses are returned as `serde_json::Value` (the unwrapped `data`
/// field) — tools serialize them directly to text, so typed structs provide
/// no benefit. Proxmox list endpoints return plain arrays with no pagination.
#[derive(Clone)]
pub struct ProxmoxClient {
    http: Client,
    base_url: String,
    auth: Auth,
}

/// How the `Authorization` header is supplied.
#[derive(Clone)]
enum Auth {
    /// Known up front; baked into the client's default headers.
    Static,
    /// Decrypted from a pass entry on first use and added per request.
    Pass(Arc<PassToken>),
}

/// A token decrypted from a `pass` entry on demand. Failures are *not*
/// cached: while the GPG key is locked every request fails fast with a
/// [`ProxmoxError::Credentials`] explaining how to unlock, and the first
/// request after the user unlocks succeeds — no server restart. Once
/// decrypted the token is held in memory, so gpg-agent's cache TTL no longer
/// matters for this process.
struct PassToken {
    entry: PassEntry,
    // Held across the decrypt so concurrent requests run gpg once, not N times.
    state: tokio::sync::Mutex<TokenState>,
}

#[derive(Default)]
struct TokenState {
    value: Option<header::HeaderValue>,
    /// Proxmox rejected `value` (401): re-read the entry before the next
    /// request, in case the token was rotated with `proxmox-mcp login`.
    stale: bool,
    /// A re-read after a 401 failed: don't run gpg again before this, so a
    /// revoked token with a locked key doesn't put a decrypt in front of
    /// every request.
    retry_at: Option<Instant>,
}

/// How long to keep reusing a rejected token after re-reading it failed.
const REREAD_BACKOFF: Duration = Duration::from_secs(30);

impl PassToken {
    async fn header(&self) -> Result<header::HeaderValue, ProxmoxError> {
        let mut state = self.state.lock().await;
        if let Some(value) = state.value.as_ref()
            && (!state.stale || state.retry_at.is_some_and(|t| Instant::now() < t))
        {
            return Ok(value.clone());
        }
        let read = self.entry.read_token().await;
        let result = match (read, state.value.as_ref()) {
            (Ok(token), _) => {
                let value = auth_header(&token).map_err(|e| {
                    ProxmoxError::Credentials(format!(
                        "{e} (from pass entry {})",
                        self.entry.name()
                    ))
                })?;
                debug!(target: LOG_TARGET, entry = self.entry.name(), "decrypted Proxmox token from pass");
                state.value = Some(value.clone());
                state.stale = false;
                state.retry_at = None;
                Ok(value)
            }
            // The re-read after a 401 failed (typically: gpg-agent's cache has
            // expired since the first decrypt). Keep sending the rejected
            // token so the caller sees Proxmox's real auth error rather than a
            // misleading "locked"; the re-read is retried on the next request.
            (Err(e), Some(old)) => {
                debug!(target: LOG_TARGET, error = %e, "re-reading rejected token failed; reusing it");
                let old = old.clone();
                state.retry_at = Some(Instant::now() + REREAD_BACKOFF);
                Ok(old)
            }
            (Err(e), None) => Err(ProxmoxError::Credentials(e.to_string())),
        };
        drop(state);
        result
    }

    /// Proxmox answered 401 to a request sent with `used`. Mark the token for
    /// re-reading — but only if it's still the current one, so a 401 for an
    /// already-replaced token can't discard its fresh successor.
    async fn mark_rejected(&self, used: &header::HeaderValue) {
        let mut state = self.state.lock().await;
        if state.value.as_ref() == Some(used) {
            state.stale = true;
        }
    }
}

/// Build the sensitive `Authorization` header value for a full API token.
fn auth_header(token: &str) -> anyhow::Result<header::HeaderValue> {
    // Proxmox uses "PVEAPIToken=USER@REALM!TOKENID=UUID", not "Bearer".
    let mut value = header::HeaderValue::from_str(&format!("PVEAPIToken={token}")).map_err(|_| {
        anyhow::anyhow!(
            "Proxmox token contains characters not valid in HTTP headers (must be visible ASCII)"
        )
    })?;
    // Keeps the token out of `Debug` output and out of HTTP/2 HPACK
    // dynamic tables (where header values could otherwise be inferred).
    value.set_sensitive(true);
    Ok(value)
}

/// Install ring as rustls's process-wide crypto provider. reqwest is built
/// with `rustls-no-provider` (see Cargo.toml), so a client can't be built
/// until this has run. Idempotent: later calls are no-ops.
pub fn install_crypto_provider() {
    // Err only means a provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl ProxmoxClient {
    /// A client for `conn`. A pass-entry token is decrypted on the first
    /// request (see [`PassToken`]), so the server starts even while the GPG
    /// key is locked.
    pub fn new(conn: Connection) -> anyhow::Result<Self> {
        let Connection {
            url,
            credential,
            insecure,
        } = conn;
        install_crypto_provider();
        let mut headers = header::HeaderMap::new();
        let auth = match credential {
            Credential::Env(token) | Credential::File(token) => {
                headers.insert(header::AUTHORIZATION, auth_header(&token)?);
                Auth::Static
            }
            Credential::Pass(entry) => Auth::Pass(Arc::new(PassToken {
                entry,
                state: tokio::sync::Mutex::new(TokenState::default()),
            })),
        };

        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .default_headers(headers)
            .danger_accept_invalid_certs(insecure)
            .build()?;

        Ok(Self {
            http,
            base_url: url.trim_end_matches('/').to_string(),
            auth,
        })
    }

    /// GET `{base}{path}?{params}` and return the unwrapped `data` value.
    /// `path` is the fully-interpolated API path, e.g. `/nodes/pve1/qemu`.
    pub async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, ProxmoxError> {
        let url = format!("{}{}", self.base_url, path);
        let builder = self.http.get(&url).query(params);
        // Log the method + final URL (with query) once. Headers — and thus the
        // PVEAPIToken — are never logged. The `enabled!` guard keeps the extra
        // clone + build out of the hot path when debug logging is off.
        if tracing::enabled!(target: LOG_TARGET, tracing::Level::DEBUG)
            && let Some(req) = builder.try_clone().and_then(|b| b.build().ok())
        {
            debug!(target: LOG_TARGET, method = %req.method(), url = %req.url(), "proxmox request");
        }
        let (builder, sent) = match &self.auth {
            Auth::Static => (builder, None),
            Auth::Pass(token) => {
                let value = token.header().await?;
                (
                    builder.header(header::AUTHORIZATION, value.clone()),
                    Some(value),
                )
            }
        };
        let resp = builder.send().await?;
        if resp.status() == StatusCode::UNAUTHORIZED
            && let (Auth::Pass(token), Some(sent)) = (&self.auth, &sent)
        {
            token.mark_rejected(sent).await;
        }
        let body = handle_response(resp).await?;

        // Some endpoints (notably storage content) answer 200 with an empty
        // `data` array *and* a non-empty `errors` map describing a partial
        // failure — typically a privilege gap. Surface that instead of silently
        // returning the empty payload.
        //
        // Only do this when `data` is empty: aggregating endpoints
        // (`/cluster/resources`, `/cluster/tasks`) also use `errors` to report
        // per-item failures while still returning useful data for everything
        // else, and we must not discard that good data over one bad item.
        if body.get("data").is_none_or(is_blank)
            && let Some(errors) = body.get("errors").filter(|e| !is_blank(e))
        {
            return Err(ProxmoxError::Partial(errors.to_string()));
        }
        Ok(unwrap_data(body))
    }
}

async fn handle_response(resp: reqwest::Response) -> Result<Value, ProxmoxError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // Log the full, untruncated error body — `to_tool_message()` clips
        // it to 300 chars for the MCP client, but a debug trace wants it all.
        debug!(target: LOG_TARGET, %status, body = %body, "proxmox error response");
        return Err(ProxmoxError::Api { status, body });
    }
    trace!(target: LOG_TARGET, %status, "proxmox response");
    Ok(resp.json().await?)
}

/// Whether a value carries nothing: `null`, or an empty array, object, or
/// string. Proxmox uses all of these for an absent `data` payload or `errors`
/// map, so an `errors` value counts only when it is not blank.
fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// Proxmox wraps every successful response as `{ "data": <payload> }`. Return
/// the inner payload; if the envelope is absent, pass the value through.
fn unwrap_data(v: Value) -> Value {
    match v {
        Value::Object(mut map) if map.contains_key("data") => {
            map.remove("data").unwrap_or(Value::Null)
        }
        other => other,
    }
}

/// A client for a mock server, authenticating as `root@pam!mcp=secret`.
#[cfg(test)]
pub fn mock_client(uri: &str) -> ProxmoxClient {
    ProxmoxClient::new(Connection {
        url: uri.to_string(),
        credential: Credential::File("root@pam!mcp=secret".to_string()),
        insecure: false,
    })
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn get_unwraps_data_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/version"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"release": "8.2"}})),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let v = client.get("/version", &[]).await.unwrap();
        // The `{ "data": ... }` envelope is unwrapped — caller sees the payload.
        assert_eq!(v, json!({"release": "8.2"}));
    }

    #[tokio::test]
    async fn get_sends_pveapitoken_auth_header() {
        let server = MockServer::start().await;
        // The mock only matches when the PVEAPIToken header is present and exact;
        // a wrong/missing header falls through to 404 and fails the unwrap below.
        Mock::given(method("GET"))
            .and(path("/version"))
            .and(header("authorization", "PVEAPIToken=root@pam!mcp=secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(client.get("/version", &[]).await.is_ok());
    }

    #[tokio::test]
    async fn get_sends_query_params() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cluster/resources"))
            .and(query_param("type", "vm"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let v = client
            .get("/cluster/resources", &[("type", "vm".to_string())])
            .await
            .unwrap();
        assert_eq!(v, json!([]));
    }

    #[tokio::test]
    async fn get_non_success_returns_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/ghost/status"))
            .respond_with(ResponseTemplate::new(500).set_body_string("no such node 'ghost'"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = client.get("/nodes/ghost/status", &[]).await.unwrap_err();
        match err {
            ProxmoxError::Api { status, body } => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert!(body.contains("no such node"), "body was: {body}");
            }
            other => panic!("expected Api error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_surfaces_non_empty_errors_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nodes/pve1/storage/backup/content"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [],
                "errors": { "backup": "permission denied" }
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = client
            .get("/nodes/pve1/storage/backup/content", &[])
            .await
            .unwrap_err();
        match err {
            ProxmoxError::Partial(body) => assert!(body.contains("permission denied")),
            other => panic!("expected Partial error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_keeps_partial_data_alongside_errors() {
        // Aggregating endpoints report per-item failures in `errors` while
        // still returning useful `data`. We must not discard the good data.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cluster/resources"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "id": "qemu/100" }],
                "errors": { "pve2": "node down" }
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let v = client.get("/cluster/resources", &[]).await.unwrap();
        assert_eq!(v, json!([{ "id": "qemu/100" }]));
    }

    #[tokio::test]
    async fn get_ignores_empty_errors_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cluster/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "upid": "UPID:..." }],
                "errors": {}
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let v = client.get("/cluster/tasks", &[]).await.unwrap();
        assert_eq!(v, json!([{ "upid": "UPID:..." }]));
    }

    #[test]
    fn is_blank_classifies_envelope_shapes() {
        assert!(is_blank(&json!(null)));
        assert!(is_blank(&json!({})));
        assert!(is_blank(&json!([])));
        assert!(is_blank(&json!("")));
        assert!(!is_blank(&json!({ "store": "denied" })));
        assert!(!is_blank(&json!(["denied"])));
        assert!(!is_blank(&json!(0)));
    }

    #[test]
    fn unwrap_data_extracts_inner_payload() {
        assert_eq!(unwrap_data(json!({"data": [1, 2, 3]})), json!([1, 2, 3]));
        assert_eq!(unwrap_data(json!({"data": {"k": "v"}})), json!({"k": "v"}));
    }

    #[test]
    fn unwrap_data_null_data_becomes_null() {
        assert_eq!(unwrap_data(json!({"data": null})), json!(null));
    }

    #[test]
    fn unwrap_data_passes_through_when_no_envelope() {
        assert_eq!(unwrap_data(json!([1, 2])), json!([1, 2]));
        assert_eq!(unwrap_data(json!({"k": "v"})), json!({"k": "v"}));
    }

    #[test]
    fn to_tool_message_short_body_passes_through() {
        let e = ProxmoxError::Api {
            status: StatusCode::BAD_REQUEST,
            body: "short error".to_string(),
        };
        assert_eq!(
            e.to_tool_message(),
            "Proxmox API error 400 Bad Request: short error"
        );
    }

    #[test]
    fn to_tool_message_long_body_is_truncated() {
        let e = ProxmoxError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "x".repeat(500),
        };
        let msg = e.to_tool_message();
        assert!(msg.ends_with("… (truncated)"), "got: {msg}");
        assert!(msg.contains(&"x".repeat(300)));
        assert!(!msg.contains(&"x".repeat(301)));
    }

    #[test]
    fn to_tool_message_multibyte_at_boundary_does_not_panic() {
        let body = "a".repeat(299) + &"€".repeat(100);
        let e = ProxmoxError::Api {
            status: StatusCode::BAD_REQUEST,
            body,
        };
        assert!(e.to_tool_message().ends_with("… (truncated)"));
    }

    mod pass_auth {
        use super::*;
        use crate::credentials::PassStore;
        use crate::credentials::test_support::{LOCKED_SNIPPET, fake_gpg, ok_snippet};

        const OLD: &str = "root@pam!mcp=old-secret";
        const NEW: &str = "root@pam!mcp=rotated-secret";

        fn pass_client(
            server: &MockServer,
        ) -> (tempfile::TempDir, std::path::PathBuf, ProxmoxClient) {
            let dir = tempfile::tempdir().unwrap();
            let entry = PassStore::new(dir.path(), fake_gpg()).entry("t").unwrap();
            let file = entry.path();
            let client = ProxmoxClient::new(Connection {
                url: server.uri(),
                credential: Credential::Pass(entry),
                insecure: false,
            })
            .unwrap();
            (dir, file, client)
        }

        fn version_ok() -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(json!({"data": {"release": "9.0"}}))
        }

        #[tokio::test]
        async fn locked_key_fails_then_succeeds_after_unlock() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/version"))
                .and(header(
                    "Authorization",
                    format!("PVEAPIToken={OLD}").as_str(),
                ))
                .respond_with(version_ok())
                .expect(2)
                .mount(&server)
                .await;
            let (_dir, file, client) = pass_client(&server);

            std::fs::write(&file, LOCKED_SNIPPET).unwrap();
            let err = client.get("/version", &[]).await.unwrap_err();
            assert!(matches!(err, ProxmoxError::Credentials(_)), "{err}");
            assert!(
                err.to_tool_message().contains("proxmox-mcp unlock"),
                "{err}"
            );

            // "Unlock": the next request decrypts successfully — no restart.
            std::fs::write(&file, ok_snippet(OLD)).unwrap();
            client.get("/version", &[]).await.unwrap();

            // Cached in memory: re-locking the key doesn't affect this process.
            std::fs::write(&file, LOCKED_SNIPPET).unwrap();
            client.get("/version", &[]).await.unwrap();
        }

        #[tokio::test]
        async fn unauthorized_drops_cached_token() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/version"))
                .and(header(
                    "Authorization",
                    format!("PVEAPIToken={OLD}").as_str(),
                ))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/version"))
                .and(header(
                    "Authorization",
                    format!("PVEAPIToken={NEW}").as_str(),
                ))
                .respond_with(version_ok())
                .mount(&server)
                .await;
            let (_dir, file, client) = pass_client(&server);

            std::fs::write(&file, ok_snippet(OLD)).unwrap();
            assert!(client.get("/version", &[]).await.is_err());
            // Rotated in the store (e.g. via `proxmox-mcp login`) — picked up.
            std::fs::write(&file, ok_snippet(NEW)).unwrap();
            client.get("/version", &[]).await.unwrap();
        }

        #[tokio::test]
        async fn rejected_token_with_locked_key_reports_the_real_401() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/version"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            let (_dir, file, client) = pass_client(&server);

            std::fs::write(&file, ok_snippet(OLD)).unwrap();
            assert!(client.get("/version", &[]).await.is_err());
            // gpg-agent's cache expires; the 401 must not turn into "locked".
            std::fs::write(&file, LOCKED_SNIPPET).unwrap();
            let err = client.get("/version", &[]).await.unwrap_err();
            assert!(
                matches!(err, ProxmoxError::Api { status, .. } if status == StatusCode::UNAUTHORIZED),
                "{err}"
            );
        }

        #[tokio::test]
        async fn failed_reread_backs_off_instead_of_decrypting_per_request() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/version"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            let (dir, file, client) = pass_client(&server);
            std::fs::write(&file, ok_snippet(OLD)).unwrap();
            assert!(client.get("/version", &[]).await.is_err());

            // Locked from now on; count every decrypt attempt.
            let counter = dir.path().join("decrypts");
            std::fs::write(
                &file,
                format!("echo x >> '{}'\n{LOCKED_SNIPPET}", counter.display()),
            )
            .unwrap();
            for _ in 0..4 {
                let err = client.get("/version", &[]).await.unwrap_err();
                assert!(matches!(err, ProxmoxError::Api { .. }), "{err}");
            }
            let n = std::fs::read_to_string(&counter).unwrap().lines().count();
            assert_eq!(n, 1, "gpg ran {n} times within the backoff");
        }

        #[tokio::test]
        async fn concurrent_first_requests_decrypt_once() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/version"))
                .respond_with(version_ok())
                .mount(&server)
                .await;
            let (dir, file, client) = pass_client(&server);
            // Each decrypt appends a line to a counter file.
            let counter = dir.path().join("decrypts");
            std::fs::write(
                &file,
                format!("echo x >> '{}'\n{}", counter.display(), ok_snippet(OLD)),
            )
            .unwrap();
            let calls = (0..8).map(|_| client.get("/version", &[]));
            for r in futures::future::join_all(calls).await {
                r.unwrap();
            }
            let n = std::fs::read_to_string(&counter).unwrap().lines().count();
            assert_eq!(n, 1, "gpg ran {n} times");
        }
    }
}
