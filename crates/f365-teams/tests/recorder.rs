use async_trait::async_trait;
use f365_auth::transport::HttpResponse;
use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError};
use f365_teams::{diagnostics::Recorder, TeamsClient};
use std::sync::Arc;

struct Hostile;
#[async_trait]
impl AuthenticatedExecutor for Hostile {
    async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
        Err(ExecError::Auth(
            "token=secret; cookie=secret; https://evil.test/?sig=secret; message=private".into(),
        ))
    }
}

#[tokio::test]
async fn actual_send_records_only_allowlisted_data_despite_hostile_input_and_error() {
    let recorder = Arc::new(Recorder::new(Arc::new(Hostile)));
    let client = TeamsClient::new(recorder.clone());
    assert!(client
        .send_text(
            "19:private@thread.v2",
            "message=private token=secret",
            "123"
        )
        .await
        .is_err());
    let json = serde_json::to_string(&recorder.bundle()).unwrap();
    for forbidden in [
        "secret", "private", "token", "cookie", "https://", "123", "19:",
    ] {
        assert!(!json.contains(forbidden), "leaked {forbidden}: {json}");
    }
    assert!(json.contains("SEND-PATH-001"));
    assert!(!json.contains("SEND-RECONCILE-001"));
    assert!(json.contains("authentication"));
}

#[tokio::test]
async fn unrelated_chat_requests_are_not_classified_as_message_contracts() {
    use f365_auth::{transport::HttpRequest, Resource};
    use std::time::Duration;
    let recorder = Recorder::new(Arc::new(Hostile));
    for method in ["GET", "POST"] {
        let _ = recorder
            .execute(AuthOp {
                resource: Resource::ChatService,
                request: HttpRequest {
                    method: method.into(),
                    url: "/v1/users/ME/profile?token=secret".into(),
                    headers: vec![],
                    body: None,
                },
                needs_page_context: false,
                idempotent: method == "GET",
                timeout: Duration::from_secs(1),
            })
            .await;
    }
    let json = serde_json::to_string(&recorder.bundle()).unwrap();
    assert!(!json.contains("SEND-"), "{json}");
    assert!(!json.contains("HISTORY-"), "{json}");
    assert!(!json.contains("secret"));
}

#[tokio::test]
async fn actual_history_uses_path_contract_not_pagination_contract() {
    let recorder = Arc::new(Recorder::new(Arc::new(Hostile)));
    let client = TeamsClient::new(recorder.clone());
    assert!(client.history("19:synthetic", 1).await.is_err());
    let json = serde_json::to_string(&recorder.bundle()).unwrap();
    assert!(json.contains("HISTORY-PATH-001"));
    assert!(!json.contains("HISTORY-PAGE-001"));
}

#[tokio::test]
async fn recorder_caps_events_and_sets_truncated() {
    let recorder = Arc::new(Recorder::new(Arc::new(Hostile)));
    let client = TeamsClient::new(recorder.clone());
    for _ in 0..260 {
        let _ = client.send_text("19:synthetic", "private", "123").await;
    }
    let json = serde_json::to_value(recorder.bundle()).unwrap();
    assert_eq!(json["events"].as_array().unwrap().len(), 256);
    assert_eq!(json["truncated"], true);
    assert_eq!(json["truncated_count"], 4);
}

struct Status(u16);
#[async_trait]
impl AuthenticatedExecutor for Status {
    async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
        Ok(HttpResponse {
            status: self.0,
            headers: vec![],
            body: vec![],
        })
    }
}

#[tokio::test]
async fn http_status_boundaries_are_classified() {
    for (status, outcome) in [
        (199, "http_error"),
        (200, "http_success"),
        (299, "http_success"),
        (300, "http_error"),
    ] {
        let recorder = Arc::new(Recorder::new(Arc::new(Status(status))));
        let client = TeamsClient::new(recorder.clone());
        let _ = client.send_text("19:synthetic", "test", "123").await;
        let json = serde_json::to_value(recorder.bundle()).unwrap();
        assert_eq!(json["events"][0]["outcome"], outcome, "status {status}");
    }
}
