use async_trait::async_trait;
use f365_auth::{transport::HttpResponse, AuthOp, AuthenticatedExecutor, ExecError};
use f365_teams::{diagnostics::Recorder, TeamsClient};
use std::sync::Arc;

struct Synthetic;
#[async_trait]
impl AuthenticatedExecutor for Synthetic {
    async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
        assert!(
            String::from_utf8_lossy(op.request.body.as_ref().unwrap()).contains("CONTENT_MARKER")
        );
        Err(ExecError::Transport(
            "TOKEN_MARKER COOKIE_MARKER https://secret.invalid/?sig=URL_MARKER".into(),
        ))
    }
}

#[tokio::test]
async fn explicitly_exported_bundle_contains_only_typed_diagnostics() {
    let recorder = Arc::new(Recorder::new(Arc::new(Synthetic)));
    let client = TeamsClient::with_recorder(recorder.clone());
    let _ = client
        .send_text("19:CHAT_MARKER", "CONTENT_MARKER", "1234567890123")
        .await;
    let json = recorder.bundle().export_json().unwrap();
    let bundle: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(bundle["schema_version"], 2);
    assert_eq!(bundle["events"][0]["contract"], "SEND-PATH-001");
    assert_eq!(bundle["events"][0]["outcome"], "network");
    for secret in [
        "TOKEN_MARKER",
        "COOKIE_MARKER",
        "CONTENT_MARKER",
        "URL_MARKER",
        "CHAT_MARKER",
        "secret.invalid",
    ] {
        assert!(!json.contains(secret), "bundle leaked {secret}");
    }
}

struct Malformed;
#[async_trait]
impl AuthenticatedExecutor for Malformed {
    async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: b"SECRET_MALFORMED".to_vec(),
        })
    }
}

#[tokio::test]
async fn malformed_successful_responses_are_protocol_errors_without_body() {
    let recorder = Arc::new(Recorder::new(Arc::new(Malformed)));
    let client = TeamsClient::with_recorder(recorder.clone());
    assert!(client.list_conversations(1).await.is_err());
    assert!(client.history("19:example", 1).await.is_err());
    let json = recorder.bundle().export_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    for contract in ["TEAMS-CSA-LIST-001", "HISTORY-PATH-001"] {
        assert!(value["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["contract"] == contract && event["outcome"] == "protocol_error"));
    }
    assert!(!json.contains("SECRET_MALFORMED"));
}
