// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! HTTP client adapter for pre-creating SEAL sessions on the gateway (ADR-088 §A8).
//!
//! Implements [`SealGatewayClient`] by POSTing session data to the SEAL gateway's
//! control plane endpoint before the container is spawned. This eliminates the
//! shared-default security context fallback that was the highest-priority AEGIS gap.
//!
//! The control plane is the gateway's operator API: each call carries the
//! operator token [`OperatorTokenSource`] obtains and refreshes.

use std::sync::Arc;

use crate::application::ports::{SealGatewayClient, SealSessionCreateRequest};
use crate::infrastructure::seal::operator_token::OperatorTokenSource;

/// HTTP-based SEAL gateway client for session pre-creation.
pub struct HttpSealGatewayClient {
    client: reqwest::Client,
    gateway_url: String,
    operator_token: Option<Arc<OperatorTokenSource>>,
}

impl HttpSealGatewayClient {
    /// `operator_token` is `None` only where no operator credentials are
    /// configured; the gateway then refuses the call unless its own operator
    /// authentication is off.
    pub fn new(gateway_url: String, operator_token: Option<Arc<OperatorTokenSource>>) -> Self {
        Self {
            client: reqwest::Client::new(),
            gateway_url,
            operator_token,
        }
    }
}

#[async_trait::async_trait]
impl SealGatewayClient for HttpSealGatewayClient {
    async fn create_session(
        &self,
        request: SealSessionCreateRequest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/v1/seal/sessions", self.gateway_url);
        let mut req = self.client.post(&url).json(&request);
        if let Some(source) = &self.operator_token {
            let authorization = source.authorization().await?;
            req = req.header("Authorization", authorization.expose());
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(
                format!("SEAL gateway session creation failed (HTTP {status}): {body}").into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::secrets::SensitiveString;
    use crate::infrastructure::seal::operator_token::OperatorCredentials;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use std::sync::Mutex;

    /// One loopback server acting as both the identity provider's token
    /// endpoint and the gateway's session endpoint, recording the
    /// `Authorization` header each session call carried.
    #[derive(Default)]
    struct Recorder {
        grants: Mutex<usize>,
        authorizations: Mutex<Vec<Option<String>>>,
    }

    async fn token(State(r): State<Arc<Recorder>>) -> axum::Json<serde_json::Value> {
        let mut grants = r.grants.lock().unwrap();
        *grants += 1;
        axum::Json(serde_json::json!({
            "access_token": format!("operator-token-{grants}"),
            "expires_in": 300,
            "token_type": "Bearer",
        }))
    }

    async fn session(State(r): State<Arc<Recorder>>, headers: HeaderMap) -> StatusCode {
        let value = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let authorized = value.as_deref() == Some("Bearer operator-token-1");
        r.authorizations.lock().unwrap().push(value);
        if authorized {
            StatusCode::OK
        } else {
            StatusCode::UNAUTHORIZED
        }
    }

    fn session_request() -> SealSessionCreateRequest {
        SealSessionCreateRequest {
            execution_id: "exec-1".to_string(),
            agent_id: "agent-1".to_string(),
            security_context: "aegis-system-default".to_string(),
            public_key_b64: "AAAA".to_string(),
            security_token: "seal-token".into(),
            session_status: "Active".to_string(),
            expires_at: "2026-09-28T00:00:00Z".to_string(),
            allowed_tool_patterns: vec!["*".to_string()],
        }
    }

    #[tokio::test]
    async fn session_precreation_presents_the_operator_token() {
        let recorder = Arc::new(Recorder::default());
        let app = Router::new()
            .route("/token", post(token))
            .route("/v1/seal/sessions", post(session))
            .with_state(recorder.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let source = Arc::new(OperatorTokenSource::new(OperatorCredentials {
            token_url: format!("{base}/token"),
            client_id: "aegis-orchestrator".to_string(),
            client_secret: SensitiveString::new("test-secret"),
        }));
        let client = HttpSealGatewayClient::new(base, Some(source));
        client
            .create_session(session_request())
            .await
            .expect("first");
        client
            .create_session(session_request())
            .await
            .expect("second");

        assert_eq!(
            *recorder.authorizations.lock().unwrap(),
            vec![
                Some("Bearer operator-token-1".to_string()),
                Some("Bearer operator-token-1".to_string())
            ]
        );
        assert_eq!(
            *recorder.grants.lock().unwrap(),
            1,
            "the token is reused while fresh"
        );
    }
    /// The JSON the gateway receives for a session pre-creation. Captured
    /// from the derived serde form while `security_token` was a `String`.
    const SESSION_REQUEST_FIXTURE: &str = r#"{"execution_id":"exec-1","agent_id":"agent-1","security_context":"aegis-system-default","public_key_b64":"AAAA","security_token":"seal-token","session_status":"Active","expires_at":"2026-09-28T00:00:00Z","allowed_tool_patterns":["*"]}"#;

    #[test]
    fn session_request_wire_form_is_unchanged() {
        assert_eq!(
            serde_json::to_string(&session_request()).unwrap(),
            SESSION_REQUEST_FIXTURE
        );
    }

    #[test]
    fn session_request_debug_does_not_print_the_security_token() {
        let printed = format!("{:?}", session_request());
        assert!(
            !printed.contains("seal-token"),
            "SealSessionCreateRequest's Debug printed its security token: {printed}"
        );
        assert!(
            printed.contains("exec-1"),
            "Debug lost the execution id: {printed}"
        );
    }
}
