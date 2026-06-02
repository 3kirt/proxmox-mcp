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
    use serde_json::json;

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
