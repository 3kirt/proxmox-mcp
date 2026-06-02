use std::time::Duration;

use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use thiserror::Error;

use crate::config::Connection;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
}

impl ProxmoxError {
    /// Returns a message safe to forward to the MCP client.
    /// Truncates the API error body to 300 bytes so values echoed in 4xx
    /// responses don't blow up the assistant's context window.
    pub fn to_tool_message(&self) -> String {
        match self {
            ProxmoxError::Api { status, body } => {
                let cut = body
                    .char_indices()
                    .nth(300)
                    .map(|(i, _)| i)
                    .unwrap_or(body.len());
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
}

impl ProxmoxClient {
    pub fn new(conn: Connection) -> anyhow::Result<Self> {
        // Proxmox uses "PVEAPIToken=USER@REALM!TOKENID=UUID", not "Bearer".
        let auth = format!("PVEAPIToken={}", conn.token);
        let auth_value = header::HeaderValue::from_str(&auth).map_err(|_| {
            anyhow::anyhow!(
                "Proxmox token contains characters not valid in HTTP headers (must be visible ASCII)"
            )
        })?;

        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, auth_value);

        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .default_headers(headers)
            .danger_accept_invalid_certs(conn.insecure)
            .build()?;

        Ok(ProxmoxClient {
            http,
            base_url: conn.url.trim_end_matches('/').to_string(),
        })
    }

    /// GET `{base}{path}?{params}` and return the unwrapped `data` value.
    /// `path` is the fully-interpolated API path, e.g. `/nodes/pve1/qemu`.
    pub async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, ProxmoxError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.http.get(&url).query(params).send().await?;
        let body = self.handle_response(resp).await?;

        // Some endpoints (notably storage content) answer 200 with an empty
        // `data` array *and* a non-empty `errors` map describing a partial
        // failure — typically a privilege gap. Surface that instead of silently
        // returning the empty payload.
        //
        // Only do this when `data` is empty: aggregating endpoints
        // (`/cluster/resources`, `/cluster/tasks`) also use `errors` to report
        // per-item failures while still returning useful data for everything
        // else, and we must not discard that good data over one bad item.
        if data_is_empty(&body)
            && let Some(errors) = body.get("errors").filter(|e| !is_errors_empty(e))
        {
            return Err(ProxmoxError::Partial(errors.to_string()));
        }
        Ok(unwrap_data(body))
    }

    async fn handle_response(&self, resp: reqwest::Response) -> Result<Value, ProxmoxError> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxmoxError::Api { status, body });
        }
        Ok(resp.json().await?)
    }
}

/// Whether the envelope's `data` carries no useful payload (absent, `null`, or
/// an empty array/object). Used to decide whether a non-empty `errors` map
/// represents a total failure worth surfacing or merely a partial one.
fn data_is_empty(body: &Value) -> bool {
    match body.get("data") {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(m)) => m.is_empty(),
        _ => false,
    }
}

/// Whether an envelope `errors` value carries no actual error. Proxmox may
/// include the key as `null`, an empty object, or an empty array on success.
fn is_errors_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Connection;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mock_client(uri: &str) -> ProxmoxClient {
        ProxmoxClient::new(Connection {
            url: uri.to_string(),
            token: "root@pam!mcp=secret".to_string(),
            insecure: false,
        })
        .unwrap()
    }

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
    fn is_errors_empty_classifies_envelope_shapes() {
        assert!(is_errors_empty(&json!(null)));
        assert!(is_errors_empty(&json!({})));
        assert!(is_errors_empty(&json!([])));
        assert!(is_errors_empty(&json!("")));
        assert!(!is_errors_empty(&json!({ "store": "denied" })));
        assert!(!is_errors_empty(&json!(["denied"])));
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
}
