//! Synthetic HTTP-only incident: WireMock cannot provide WebSocket frames.
use async_trait::async_trait;
use f365_auth::transport::HttpResponse;
use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError, ManualClock, Resource};
use f365_teams::{diagnostics::Recorder, TeamsClient};
use std::sync::Arc;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

struct LocalExecutor {
    chat: String,
    trouter: String,
    http: reqwest::Client,
}
#[async_trait]
impl AuthenticatedExecutor for LocalExecutor {
    async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
        let base = match op.resource {
            Resource::ChatService => &self.chat,
            Resource::Trouter => &self.trouter,
            _ => return Err(ExecError::RouteUnavailable("synthetic only")),
        };
        let path = if op.request.url.starts_with('/') {
            op.request.url.clone()
        } else {
            let url = url::Url::parse(&op.request.url).unwrap();
            format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        };
        let method = reqwest::Method::from_bytes(op.request.method.as_bytes()).unwrap();
        let mut request = self.http.request(method, format!("{base}{path}"));
        if let Some(body) = op.request.body {
            request = request.body(body);
        }
        let resp = request
            .send()
            .await
            .map_err(|_| ExecError::Transport("synthetic".into()))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap().to_vec();
        Ok(HttpResponse {
            status,
            headers: vec![],
            body,
        })
    }
}

#[tokio::test]
async fn incident_handshake_failure_and_chat_http_produce_safe_bundle() {
    let chat = MockServer::start().await;
    let trouter = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v4/a")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "socketio":"https://synthetic.trouter.teams.microsoft.com/", "surl":"synthetic", "connectparams":{"sig":"secret"}
    }))).expect(1).mount(&trouter).await;
    Mock::given(method("GET"))
        .and(path("/socket.io/1/"))
        .respond_with(ResponseTemplate::new(400).set_body_string("private token=secret"))
        .expect(1)
        .mount(&trouter)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/users/ME/conversations/19:synthetic/messages"))
        .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
        .expect(1)
        .mount(&chat)
        .await;
    let recorder = Arc::new(Recorder::new(Arc::new(LocalExecutor {
        chat: chat.uri(),
        trouter: trouter.uri(),
        http: reqwest::Client::new(),
    })));
    let client = TeamsClient::new(recorder.clone());
    client
        .send_text("19:synthetic", "private token=secret", "123")
        .await
        .unwrap();
    let err = client
        .listen(&ManualClock::default(), &mut |_| {})
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        f365_teams::TeamsError::Http { status: 400, .. }
    ));
    let bundle = serde_json::to_string(&recorder.bundle()).unwrap();
    assert!(bundle.contains("http_error"));
    assert!(bundle.contains("TROUTER-CONNECT-001"));
    assert!(bundle.contains("TROUTER-HANDSHAKE-001"));
    for forbidden in [
        "private",
        "secret",
        "token",
        "https://",
        "123",
        "19:synthetic",
    ] {
        assert!(!bundle.contains(forbidden));
    }
}
