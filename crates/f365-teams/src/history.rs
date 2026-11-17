//! Histórico regional: HISTORY-PATH-001, HISTORY-SHAPE-001 e HISTORY-PAGE-001 (UNVERIFIED).
use crate::{send::validate_chat_id, TeamsClient, TeamsError};
use f365_auth::{transport::HttpRequest, AuthOp, Resource};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

#[derive(Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub id: String,
    /// Raw string `clientmessageid` when present; HISTORY-SHAPE-001 is UNVERIFIED.
    /// This field alone does not perform send reconciliation.
    pub client_message_id: Option<String>,
    pub time: Option<String>,
    pub sender: Option<String>,
    pub message_type: Option<String>,
    pub content: Option<String>,
}

fn field(v: &Value, k: &str) -> Option<String> {
    match v.get(k) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn parse(body: &[u8]) -> Result<(Vec<HistoryMessage>, Option<String>), TeamsError> {
    let bad = || TeamsError::Malformed("HISTORY-SHAPE-001 (UNVERIFIED)");
    let v: Value = serde_json::from_slice(body).map_err(|_| bad())?;
    let messages = v
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(bad)?;
    let mut out = Vec::new();
    for m in messages {
        if !m.is_object() {
            return Err(bad());
        }
        let id = field(m, "id").filter(|s| !s.is_empty()).ok_or_else(bad)?;
        out.push(HistoryMessage {
            id,
            client_message_id: m
                .get("clientmessageid")
                .and_then(Value::as_str)
                .map(str::to_owned),
            time: field(m, "composetime").or_else(|| field(m, "originalarrivaltime")),
            sender: field(m, "imdisplayname").or_else(|| field(m, "from")),
            message_type: field(m, "messagetype"),
            content: field(m, "content"),
        });
    }
    let link = match v.get("_metadata") {
        None | Some(Value::Null) => None,
        Some(Value::Object(meta)) => match meta.get("backwardLink") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => return Err(TeamsError::Malformed("HISTORY-PAGE-001 (UNVERIFIED)")),
        },
        _ => return Err(TeamsError::Malformed("HISTORY-PAGE-001 (UNVERIFIED)")),
    };
    let link = if link.is_some() {
        link
    } else {
        match v.get("_metadata").and_then(|m| m.get("syncState")) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => return Err(TeamsError::Malformed("HISTORY-PAGE-001 (UNVERIFIED)")),
        }
    };
    Ok((out, link))
}

fn validate_cursor(cursor: &str, path: &str) -> Result<(), TeamsError> {
    let bad = || TeamsError::Malformed("HISTORY-PAGE-001 cursor inválido ou cíclico (UNVERIFIED)");
    if cursor
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
        || cursor.contains('#')
    {
        return Err(bad());
    }
    let raw_path = if cursor.starts_with('/') && !cursor.starts_with("//") {
        cursor.split('?').next().unwrap_or("")
    } else {
        let u = url::Url::parse(cursor).map_err(|_| bad())?;
        if u.scheme() != "https"
            || !u.username().is_empty()
            || u.password().is_some()
            || u.fragment().is_some()
        {
            return Err(bad());
        }
        let rest = cursor.split_once("://").ok_or_else(bad)?.1;
        let slash = rest.find('/').ok_or_else(bad)?;
        rest[slash..].split('?').next().unwrap_or("")
    };
    if raw_path != path {
        return Err(bad());
    }
    Ok(())
}

fn op(url: String) -> AuthOp {
    AuthOp {
        resource: Resource::ChatService,
        request: HttpRequest {
            method: "GET".into(),
            url,
            headers: vec![("Accept".into(), "application/json".into())],
            body: None,
        },
        needs_page_context: false,
        idempotent: true,
        timeout: Duration::from_secs(30),
    }
}

impl TeamsClient {
    pub async fn history(
        &self,
        chat_id: &str,
        top: usize,
    ) -> Result<Vec<HistoryMessage>, TeamsError> {
        self.history_with_clock(chat_id, top, &f365_auth::SystemClock)
            .await
    }

    async fn history_with_clock(
        &self,
        chat_id: &str,
        top: usize,
        clock: &dyn f365_auth::Clock,
    ) -> Result<Vec<HistoryMessage>, TeamsError> {
        let start = std::time::Instant::now();
        tokio::select! {
            biased;
            _ = clock.sleep(Duration::from_secs(60)) => {
                if let Some(recorder) = &self.recorder {
                    recorder.history_timeout(start);
                }
                Err(TeamsError::Malformed("HISTORY-PAGE-001 prazo total excedido"))
            },
            result = self.history_pages(chat_id, top) => result,
        }
    }

    async fn history_pages(
        &self,
        chat_id: &str,
        top: usize,
    ) -> Result<Vec<HistoryMessage>, TeamsError> {
        validate_chat_id(chat_id)?;
        if top > 1000 {
            return Err(TeamsError::Malformed("HISTORY-PAGE-001 top excede 1000"));
        }
        if top == 0 {
            return Ok(Vec::new());
        }
        let path = format!("/v1/users/ME/conversations/{chat_id}/messages");
        let mut next = format!(
            "{path}?view=msnp24Equivalent|supportsMessageProperties&pageSize={top}&startTime=1"
        );
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for _ in 0..10 {
            if !seen.insert(next.clone()) {
                return Err(TeamsError::Malformed(
                    "HISTORY-PAGE-001 cursor cíclico (UNVERIFIED)",
                ));
            }
            let response = self.exec.execute(op(next)).await?;
            // Executores alternativos podem ter recebido o corpo inteiro; valide antes do parse.
            if response.body.len() > 8 * 1024 * 1024 {
                if (200..300).contains(&response.status) {
                    if let Some(recorder) = &self.recorder {
                        recorder.response_protocol(true, false, std::time::Instant::now());
                    }
                }
                return Err(TeamsError::Malformed("HISTORY-PAGE-001 corpo excede 8 MiB"));
            }
            if !(200..300).contains(&response.status) {
                return Err(TeamsError::Http {
                    service: "chat regional",
                    status: response.status,
                });
            }
            let start = std::time::Instant::now();
            let parsed = (|| {
                let (messages, link) = parse(&response.body)?;
                if let Some(link) = &link {
                    validate_cursor(link, &path)?;
                    if seen.contains(link) {
                        return Err(TeamsError::Malformed(
                            "HISTORY-PAGE-001 cursor cíclico (UNVERIFIED)",
                        ));
                    }
                }
                Ok((messages, link))
            })();
            if let Some(recorder) = &self.recorder {
                recorder.response_protocol(true, parsed.is_ok(), start);
            }
            let (messages, link) = parsed?;
            out.extend(messages);
            if out.len() >= top {
                out.truncate(top);
                return Ok(out);
            }
            match link {
                Some(link) => next = link,
                None => return Ok(out),
            }
        }
        if let Some(recorder) = &self.recorder {
            recorder.response_protocol(true, false, std::time::Instant::now());
        }
        Err(TeamsError::Malformed(
            "HISTORY-PAGE-001 limite de páginas (UNVERIFIED)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use f365_auth::{transport::HttpResponse, AuthenticatedExecutor, ExecError};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Fake {
        bodies: Mutex<VecDeque<&'static str>>,
        ops: Mutex<Vec<AuthOp>>,
    }
    #[async_trait]
    impl AuthenticatedExecutor for Fake {
        async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
            self.ops.lock().unwrap().push(op);
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: self
                    .bodies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap()
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    fn fake(bodies: &[&'static str]) -> Arc<Fake> {
        Arc::new(Fake {
            bodies: Mutex::new(bodies.iter().copied().collect()),
            ops: Mutex::new(vec![]),
        })
    }

    #[tokio::test]
    async fn global_deadline_cancels_a_stalled_page() {
        struct Stalled;
        #[async_trait]
        impl AuthenticatedExecutor for Stalled {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                std::future::pending().await
            }
        }
        let clock = f365_auth::ManualClock::default();
        let recorder = Arc::new(crate::diagnostics::Recorder::new(Arc::new(Stalled)));
        let client = TeamsClient::with_recorder(recorder.clone());
        let task = tokio::spawn({
            let clock = clock.clone();
            async move { client.history_with_clock("48:notes", 1, &clock).await }
        });
        tokio::task::yield_now().await;
        clock.advance(Duration::from_secs(60));
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(TeamsError::Malformed(s)) if s.contains("prazo")));
        let json = recorder.bundle().export_json().unwrap();
        let bundle: Value = serde_json::from_str(&json).unwrap();
        assert!(bundle["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["contract"] == "HISTORY-PATH-001" && e["outcome"] == "timeout"));
        assert!(!json.contains("48:notes"));
    }

    #[tokio::test]
    async fn deadline_spans_pages_and_never_returns_partial_history() {
        struct TwoPages(Mutex<usize>, f365_auth::ManualClock);
        #[async_trait]
        impl AuthenticatedExecutor for TwoPages {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                let n = {
                    let mut n = self.0.lock().unwrap();
                    *n += 1;
                    *n
                };
                if n == 1 {
                    self.1.advance(Duration::from_secs(40));
                    Ok(HttpResponse { status: 200, headers: vec![], body: br#"{"messages":[{"id":"secret"}],"_metadata":{"backwardLink":"/v1/users/ME/conversations/48:notes/messages?cursor=syntheticsecret"}}"#.to_vec() })
                } else {
                    std::future::pending().await
                }
            }
        }
        let clock = f365_auth::ManualClock::default();
        let exec = Arc::new(TwoPages(Mutex::new(0), clock.clone()));
        let client = TeamsClient::new(exec.clone());
        let task = tokio::spawn({
            let clock = clock.clone();
            async move { client.history_with_clock("48:notes", 2, &clock).await }
        });
        for _ in 0..100 {
            if *exec.0.lock().unwrap() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*exec.0.lock().unwrap(), 2);
        clock.advance(Duration::from_secs(20));
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        match result {
            Err(e) => {
                assert!(e.to_string().contains("prazo"));
                assert!(!e.to_string().contains("secret"));
            }
            Ok(_) => panic!("partial history returned"),
        }
    }

    #[tokio::test]
    async fn oversized_page_from_any_executor_fails_without_leaking_content() {
        struct Oversized;
        #[async_trait]
        impl AuthenticatedExecutor for Oversized {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: vec![b'x'; 8 * 1024 * 1024 + 1],
                })
            }
        }
        let result = TeamsClient::new(Arc::new(Oversized))
            .history("48:notes", 1)
            .await;
        assert!(
            matches!(result, Err(TeamsError::Malformed(s)) if s.contains("8 MiB") && !s.contains("xxxx"))
        );
    }

    #[tokio::test]
    async fn page_limit_rejects_continuation_without_partial_history() {
        struct ManyPages(Mutex<usize>);
        #[async_trait]
        impl AuthenticatedExecutor for ManyPages {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                let mut n = self.0.lock().unwrap();
                *n += 1;
                let body = format!(
                    r#"{{"messages":[],"_metadata":{{"backwardLink":"/v1/users/ME/conversations/48:notes/messages?cursor={}"}}}}"#,
                    *n
                );
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: body.into_bytes(),
                })
            }
        }
        let exec = Arc::new(ManyPages(Mutex::new(0)));
        let recorder = Arc::new(crate::diagnostics::Recorder::new(exec.clone()));
        let result = TeamsClient::with_recorder(recorder.clone())
            .history("48:notes", 1)
            .await;
        assert!(matches!(result, Err(TeamsError::Malformed(s)) if s.contains("limite de páginas")));
        assert_eq!(*exec.0.lock().unwrap(), 10);
        let json = recorder.bundle().export_json().unwrap();
        let bundle: Value = serde_json::from_str(&json).unwrap();
        let events = bundle["events"].as_array().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e["outcome"] == "http_success")
                .count(),
            10
        );
        assert!(events
            .iter()
            .any(|e| e["contract"] == "HISTORY-PATH-001" && e["outcome"] == "protocol_error"));
        assert!(!json.contains("48:notes"));
        assert!(!json.contains("cursor="));
    }

    #[tokio::test]
    async fn rejects_unbounded_top_before_request() {
        let exec = fake(&[]);
        assert!(TeamsClient::new(exec.clone())
            .history("48:notes", 1001)
            .await
            .is_err());
        assert!(exec.ops.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sync_state_url_fetches_multiple_pages() {
        let exec = fake(&[
            r#"{"messages":[{"id":"1"}],"_metadata":{"syncState":"/v1/users/ME/conversations/48:notes/messages?cursor=2"}}"#,
            r#"{"messages":[{"id":"2"}],"_metadata":{"syncState":"https://amer.ng.msg.teams.microsoft.com/v1/users/ME/conversations/48:notes/messages?cursor=3"}}"#,
            r#"{"messages":[{"id":"3"}]}"#,
        ]);
        let result = TeamsClient::new(exec.clone())
            .history("48:notes", 4)
            .await
            .unwrap();
        assert_eq!(
            result.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        assert_eq!(exec.ops.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn sync_state_rejects_unsafe_and_ambiguous_values_without_partial_results() {
        for value in [
            serde_json::json!("opaque-secret"),
            serde_json::json!({"cursor": "secret"}),
            serde_json::json!("//evil.example/v1/users/ME/conversations/48:notes/messages"),
            serde_json::json!(
                "https://user@evil.example/v1/users/ME/conversations/48:notes/messages"
            ),
            serde_json::json!("http://evil.example/v1/users/ME/conversations/48:notes/messages"),
            serde_json::json!("/v1/users/ME/conversations/other/messages"),
            serde_json::json!("/v1/users/ME/conversations/48:notes/../notes/messages"),
        ] {
            let body =
                serde_json::json!({"messages": [{"id": "1"}], "_metadata": {"syncState": value}})
                    .to_string();
            let (messages, link) = match parse(body.as_bytes()) {
                Ok(parsed) => parsed,
                Err(e) => {
                    assert!(!e.to_string().contains("secret"));
                    continue;
                }
            };
            assert_eq!(messages.len(), 1);
            let error = validate_cursor(
                link.as_deref().unwrap(),
                "/v1/users/ME/conversations/48:notes/messages",
            )
            .unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[tokio::test]
    async fn sync_state_loop_fails_without_partial_results() {
        let exec = fake(&[
            r#"{"messages":[{"id":"1"}],"_metadata":{"syncState":"/v1/users/ME/conversations/48:notes/messages?cursor=2"}}"#,
            r#"{"messages":[{"id":"2"}],"_metadata":{"syncState":"/v1/users/ME/conversations/48:notes/messages?cursor=2"}}"#,
        ]);
        assert!(TeamsClient::new(exec.clone())
            .history("48:notes", 3)
            .await
            .is_err());
        assert_eq!(exec.ops.lock().unwrap().len(), 2);
    }

    async fn rejects_cycle_at_top(body: &'static str) {
        let exec = fake(&[body, body]);
        let recorder = Arc::new(crate::diagnostics::Recorder::new(exec.clone()));
        let result = TeamsClient::with_recorder(recorder.clone())
            .history("48:notes", 2)
            .await;
        let json = recorder.bundle().export_json().unwrap();
        let bundle: Value = serde_json::from_str(&json).unwrap();
        assert!(bundle["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["contract"] == "HISTORY-PATH-001" && e["outcome"] == "protocol_error"));
        assert!(!json.contains("syntheticsecret"));
        assert!(!json.contains("48:notes"));
        match result {
            Err(e) => {
                assert!(e.to_string().contains("cursor cíclico"));
                assert!(!e.to_string().contains("syntheticsecret"));
            }
            Ok(_) => panic!("cyclic cursor returned success at top"),
        }
        assert_eq!(exec.ops.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn backward_link_cycle_at_top_fails() {
        rejects_cycle_at_top(r#"{"messages":[{"id":"1"}],"_metadata":{"backwardLink":"/v1/users/ME/conversations/48:notes/messages?cursor=syntheticsecret"}}"#).await;
    }

    #[tokio::test]
    async fn sync_state_cycle_at_top_fails() {
        rejects_cycle_at_top(r#"{"messages":[{"id":"1"}],"_metadata":{"syncState":"/v1/users/ME/conversations/48:notes/messages?cursor=syntheticsecret"}}"#).await;
    }

    #[test]
    fn backward_link_takes_precedence_over_sync_state() {
        let (_, link) = parse(br#"{"messages":[],"_metadata":{"backwardLink":"/v1/users/ME/conversations/48:notes/messages?cursor=2","syncState":{"unexpected":true}}}"#).unwrap();
        assert_eq!(
            link.as_deref(),
            Some("/v1/users/ME/conversations/48:notes/messages?cursor=2")
        );
    }

    #[tokio::test]
    async fn sync_state_only_never_returns_partial_success() {
        let exec = fake(&[r#"{"messages":[{"id":"1"}],"_metadata":{"syncState":"synthetic"}}"#]);
        let result = TeamsClient::new(exec).history("48:notes", 2).await;
        assert!(matches!(result, Err(TeamsError::Malformed(s)) if s.contains("HISTORY-PAGE-001")));
    }

    #[tokio::test]
    async fn paginates_only_within_the_conversation_and_respects_top() {
        let exec = fake(&[
            r#"{"messages":[{"id":"1"}],"_metadata":{"backwardLink":"/v1/users/ME/conversations/48:notes/messages?cursor=2"}}"#,
            r#"{"messages":[{"id":"2"},{"id":"3"}]}"#,
        ]);
        let result = TeamsClient::new(exec.clone())
            .history("48:notes", 2)
            .await
            .unwrap();
        assert_eq!(
            result.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["1", "2"]
        );
        let ops = exec.ops.lock().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].request.url, "/v1/users/ME/conversations/48:notes/messages?view=msnp24Equivalent|supportsMessageProperties&pageSize=2&startTime=1");
        assert!(ops.iter().all(|op| op.resource == Resource::ChatService
            && op.idempotent
            && op.request.method == "GET"
            && op.request.body.is_none()));
    }

    #[tokio::test]
    async fn rejects_cross_conversation_and_repeated_cursor() {
        let cross = fake(&[
            r#"{"messages":[],"_metadata":{"backwardLink":"/v1/users/ME/conversations/other/messages?x=1"}}"#,
        ]);
        assert!(TeamsClient::new(cross.clone())
            .history("48:notes", 1)
            .await
            .is_err());
        assert_eq!(cross.ops.lock().unwrap().len(), 1);
        let cycle = fake(&[
            r#"{"messages":[],"_metadata":{"backwardLink":"/v1/users/ME/conversations/48:notes/messages?cursor=2"}}"#,
            r#"{"messages":[],"_metadata":{"backwardLink":"/v1/users/ME/conversations/48:notes/messages?cursor=2"}}"#,
        ]);
        assert!(TeamsClient::new(cycle.clone())
            .history("48:notes", 1)
            .await
            .is_err());
        assert_eq!(cycle.ops.lock().unwrap().len(), 2);
    }
    #[test]
    fn client_message_id_is_optional_and_preserves_supported_json_values() {
        let (messages, _) = parse(br#"{"messages":[{"id":"1","clientmessageid":"SYNTHETIC-123"},{"id":"2"},{"id":"3","clientmessageid":{"unsupported":"secret"}},{"id":"4","clientmessageid":12345}]}"#).unwrap();
        assert_eq!(
            messages[0].client_message_id.as_deref(),
            Some("SYNTHETIC-123")
        );
        assert_eq!(messages[1].client_message_id, None);
        assert_eq!(messages[2].client_message_id, None);
        assert_eq!(messages[3].client_message_id, None);
    }

    #[test]
    fn parser_and_cursor_enforce_contract() {
        let (m, link) = parse(br#"{"messages":[{"id":"1","originalarrivaltime":"today","from":"u","content":"secret"}]}"#).unwrap();
        assert_eq!(m[0].time.as_deref(), Some("today"));
        assert!(link.is_none());
        assert!(parse(br#"{"value":[]}"#).is_err());
        let p = "/v1/users/ME/conversations/48:notes/messages";
        assert!(validate_cursor(&format!("{p}?x=1"), p).is_ok());
        for s in [
            "//evil/x",
            "https://evil/v1/users/ME/conversations/other/messages",
            "/v1/users/ME/conversations/48:notes/../other/messages",
            "/v1/users/ME/conversations/48:notes/messages#x",
        ] {
            assert!(validate_cursor(s, p).is_err());
        }
    }
}
