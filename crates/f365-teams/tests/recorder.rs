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
    assert!(json.contains("SEND-RECONCILE-001"));
    assert!(json.contains("authentication"));
}
