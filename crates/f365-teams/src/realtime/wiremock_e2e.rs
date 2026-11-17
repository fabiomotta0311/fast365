//! Synthetic loopback transport; the production host allowlist stays unchanged.
use super::*;
use crate::diagnostics::Recorder;
use async_trait::async_trait;
use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError, ManualClock, Resource};
use std::sync::Arc;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

struct LoopbackExecutor {
    base: String,
    http: reqwest::Client,
}
#[async_trait]
impl AuthenticatedExecutor for LoopbackExecutor {
    async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
        assert_eq!(op.resource, Resource::Trouter);
        let original = url::Url::parse(&op.request.url).unwrap();
        let target = format!(
            "{}{}{}",
            self.base,
            original.path(),
            original
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default()
        );
        let mut request = self.http.request(
            reqwest::Method::from_bytes(op.request.method.as_bytes()).unwrap(),
            target,
        );
        if let Some(body) = op.request.body {
            request = request.body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| ExecError::Transport("loopback".into()))?;
        Ok(HttpResponse {
            status: response.status().as_u16(),
            headers: vec![],
            body: response.bytes().await.unwrap().to_vec(),
        })
    }
}

#[tokio::test]
async fn trouter_wiremock_websocket_recorder_end_to_end() {
    let http = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v4/a")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "socketio":"https://test.trouter.teams.microsoft.com/", "surl":"https://private.invalid/SURL_MARKER", "ccid":"CCID_MARKER", "registrarUrl":"https://communications.svc.cloud.microsoft/registrar/test/v3/registrations", "connectparams":{"sig":"SIGNATURE_MARKER", "cookie":"COOKIE_MARKER", "token":"TOKEN_MARKER"}, "CONTENT_MARKER":"CONTENT_MARKER"
    }))).expect(1).mount(&http).await;
    Mock::given(method("GET"))
        .and(path("/socket.io/1/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("SID_MARKER:40:60:websocket"))
        .expect(1)
        .mount(&http)
        .await;
    Mock::given(method("POST"))
        .and(path("/registrar/test/v3/registrations"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&http)
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", listener.local_addr().unwrap());
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(Message::text("1::")).await.unwrap();
        ws.send(Message::text(r#"3:::{"id":77,"body":{"resourceLink":"https://private.invalid/conversations/19:CHAT_MARKER/messages/1?sig=URL_MARKER","content":"CONTENT_MARKER","cookie":"COOKIE_MARKER","token":"TOKEN_MARKER"}}"#)).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(
            reply.to_text().unwrap(),
            r#"3:::{"body":"","headers":{},"id":77,"status":200}"#
        );
        let _ = ack_tx.send(());
        ws.send(Message::text("7:::TOKEN_MARKER COOKIE_MARKER URL_MARKER"))
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let recorder = Arc::new(Recorder::new(Arc::new(LoopbackExecutor {
        base: http.uri(),
        http: reqwest::Client::new(),
    })));
    let mut client = TeamsClient::with_recorder(recorder.clone());
    client.realtime_test_url = Some(ws_url);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        client
            .listen(&ManualClock::default(), &mut |event| {
                let _ = tx.send(event);
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut ready = false;
        while let Some(event) = rx.recv().await {
            if event == RealtimeEvent::Ready {
                ready = true;
            }
            if event
                == (RealtimeEvent::Changed {
                    conversation: Some("19:CHAT_MARKER".into()),
                })
            {
                assert!(ready);
                return;
            }
        }
        panic!("missing notification");
    })
    .await
    .expect("listener timed out");
    tokio::time::timeout(Duration::from_secs(5), ack_rx)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if matches!(event, RealtimeEvent::Reconnecting { .. }) {
                return;
            }
        }
        panic!("missing websocket failure");
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    server.abort();
    let _ = server.await;
    let requests = http.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let handshake = requests
        .iter()
        .find(|r| r.url.path() == "/socket.io/1/")
        .unwrap();
    let params: std::collections::HashMap<_, _> =
        handshake.url.query_pairs().into_owned().collect();
    assert_eq!(
        params.get("sig").map(String::as_str),
        Some("SIGNATURE_MARKER")
    );
    assert_eq!(params.get("con_num").map(String::as_str), Some("0_1"));
    let registrar = requests
        .iter()
        .find(|r| r.url.path().starts_with("/registrar/"))
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&registrar.body).unwrap();
    assert_eq!(
        body["transports"]["TROUTER"][0]["path"],
        "https://private.invalid/SURL_MARKER"
    );
    assert_eq!(body["registrationId"], params["epid"]);
    let bundle = recorder.bundle().export_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&bundle).unwrap();
    let events = parsed["events"].as_array().unwrap();
    assert_eq!(events.len(), 6);
    assert!(events
        .iter()
        .any(|e| e["contract"] == "TROUTER-WS-001" && e["outcome"] == "session_error"));
    for contract in [
        "TROUTER-CONNECT-001",
        "TROUTER-HANDSHAKE-001",
        "TROUTER-REGISTRAR-001",
    ] {
        assert!(bundle.contains(contract));
    }
    for secret in [
        "CONTENT_MARKER",
        "COOKIE_MARKER",
        "TOKEN_MARKER",
        "SIGNATURE_MARKER",
        "URL_MARKER",
        "SURL_MARKER",
        "CCID_MARKER",
        "SID_MARKER",
        "CHAT_MARKER",
        "https://",
        "ws://",
        "127.0.0.1",
        &params["epid"],
    ] {
        assert!(!bundle.contains(secret));
    }
}

#[tokio::test]
async fn malformed_connect_records_http_and_protocol_outcomes_without_body() {
    let http = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/a"))
        .respond_with(ResponseTemplate::new(200).set_body_string("SECRET_MALFORMED"))
        .mount(&http)
        .await;
    let recorder = Arc::new(Recorder::new(Arc::new(LoopbackExecutor {
        base: http.uri(),
        http: reqwest::Client::new(),
    })));
    let client = TeamsClient::with_recorder(recorder.clone());
    assert!(client
        .listen(&ManualClock::default(), &mut |_| {})
        .await
        .is_err());
    let bundle = recorder.bundle().export_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&bundle).unwrap();
    let events = value["events"].as_array().unwrap();
    assert!(events
        .iter()
        .any(|e| e["contract"] == "TROUTER-CONNECT-001" && e["outcome"] == "http_success"));
    assert!(events
        .iter()
        .any(|e| e["contract"] == "TROUTER-CONNECT-001" && e["outcome"] == "protocol_error"));
    assert!(!bundle.contains("SECRET_MALFORMED"));
}

#[tokio::test]
async fn malformed_handshake_records_protocol_error_without_body() {
    let http = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v4/a")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "socketio":"https://test.trouter.teams.microsoft.com/", "surl":"https://private.invalid/secret", "ccid":"ccid", "registrarUrl":"https://communications.svc.cloud.microsoft/registrar/test/v3/registrations", "connectparams":{}
    }))).mount(&http).await;
    Mock::given(method("GET"))
        .and(path("/socket.io/1/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("SECRET_HANDSHAKE"))
        .mount(&http)
        .await;
    let recorder = Arc::new(Recorder::new(Arc::new(LoopbackExecutor {
        base: http.uri(),
        http: reqwest::Client::new(),
    })));
    let client = TeamsClient::with_recorder(recorder.clone());
    assert!(client
        .listen(&ManualClock::default(), &mut |_| {})
        .await
        .is_err());
    let bundle = recorder.bundle().export_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&bundle).unwrap();
    let events = value["events"].as_array().unwrap();
    for outcome in ["http_success", "protocol_error"] {
        assert!(events
            .iter()
            .any(|e| e["contract"] == "TROUTER-HANDSHAKE-001" && e["outcome"] == outcome));
    }
    assert!(!bundle.contains("SECRET_HANDSHAKE"));
}
