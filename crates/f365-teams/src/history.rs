//! Histórico regional: HISTORY-PATH-001, HISTORY-SHAPE-001 e HISTORY-PAGE-001 (UNVERIFIED).
use crate::{send::validate_chat_id, TeamsClient, TeamsError};
use f365_auth::{transport::HttpRequest, AuthOp, Resource};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

#[derive(Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub id: String,
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
    if link.is_none()
        && v.get("_metadata")
            .and_then(|m| m.get("syncState"))
            .is_some_and(|s| !s.is_null() && s.as_str() != Some(""))
    {
        return Err(TeamsError::Malformed(
            "HISTORY-PAGE-001 syncState sem continuação suportada (UNVERIFIED)",
        ));
    }
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
            if !(200..300).contains(&response.status) {
                return Err(TeamsError::Http {
                    service: "chat regional",
                    status: response.status,
                });
            }
            let (messages, link) = parse(&response.body)?;
            out.extend(messages);
            if out.len() >= top {
                out.truncate(top);
                return Ok(out);
            }
            match link {
                Some(link) => {
                    validate_cursor(&link, &path)?;
                    next = link;
                }
                None => return Ok(out),
            }
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
    async fn rejects_unbounded_top_before_request() {
        let exec = fake(&[]);
        assert!(TeamsClient::new(exec.clone())
            .history("48:notes", 1001)
            .await
            .is_err());
        assert!(exec.ops.lock().unwrap().is_empty());
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
