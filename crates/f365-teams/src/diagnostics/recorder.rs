//! Explicitly instrument an executor; only completed HTTP operations are recorded.
//! Payloads, arbitrary strings, identifiers and raw numeric values are never retained.
use async_trait::async_trait;
use f365_auth::transport::HttpResponse;
use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError, Resource};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MAX_EVENTS: usize = 256;
#[derive(Clone, Copy, Debug, Serialize)]
enum Contract {
    #[serde(rename = "SEND-RECONCILE-001")]
    Send,
    #[serde(rename = "HISTORY-PAGE-001")]
    History,
    #[serde(rename = "TROUTER-CONNECT-001")]
    TrouterConnect,
    #[serde(rename = "TROUTER-HANDSHAKE-001")]
    TrouterHandshake,
    #[serde(rename = "TROUTER-REGISTRAR-001")]
    TrouterRegistrar,
    #[serde(rename = "TEAMS-CSA-LIST-001")]
    Conversations,
    Other,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Success,
    HttpError,
    Authentication,
    Timeout,
    Network,
    Route,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum DurationBucket {
    UnderOneSecond,
    UnderTenSeconds,
    Longer,
}
#[derive(Clone, Debug, Serialize)]
struct RecordedEvent {
    contract: Contract,
    outcome: Outcome,
    duration: DurationBucket,
}
#[derive(Debug, Serialize)]
pub struct RecordedBundle {
    schema_version: u8,
    events: Vec<RecordedEvent>,
    truncated: bool,
}
struct Log {
    events: Vec<RecordedEvent>,
    truncated: bool,
}
/// The recorder does not itself access a token store, network, or filesystem.
pub struct Recorder {
    inner: Arc<dyn AuthenticatedExecutor>,
    log: Mutex<Log>,
}
impl Recorder {
    pub fn new(inner: Arc<dyn AuthenticatedExecutor>) -> Self {
        Self {
            inner,
            log: Mutex::new(Log {
                events: Vec::new(),
                truncated: false,
            }),
        }
    }
    pub fn bundle(&self) -> RecordedBundle {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        RecordedBundle {
            schema_version: 1,
            events: log.events.clone(),
            truncated: log.truncated,
        }
    }
}
#[async_trait]
impl AuthenticatedExecutor for Recorder {
    async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
        let contract = match (op.resource, op.request.method.as_str()) {
            (Resource::ChatService, "POST") => Contract::Send,
            (Resource::ChatService, "GET") => Contract::History,
            (Resource::ChatSvcAgg, "GET") => Contract::Conversations,
            (Resource::Trouter, method) => {
                let path = url::Url::parse(&op.request.url).ok();
                match (method, path.as_ref().map(url::Url::path)) {
                    ("POST", Some("/v4/a")) => Contract::TrouterConnect,
                    ("GET", Some("/socket.io/1/")) => Contract::TrouterHandshake,
                    ("POST", Some(p))
                        if p.starts_with("/registrar/") && p.ends_with("/registrations") =>
                    {
                        Contract::TrouterRegistrar
                    }
                    _ => Contract::Other,
                }
            }
            _ => Contract::Other,
        };
        let start = Instant::now();
        let result = self.inner.execute(op).await;
        let outcome = match &result {
            Ok(r) if (200..300).contains(&r.status) => Outcome::Success,
            Ok(_) => Outcome::HttpError,
            Err(ExecError::NotPaired | ExecError::Auth(_)) => Outcome::Authentication,
            Err(ExecError::Transport(_)) => Outcome::Network,
            Err(ExecError::TimedOut(_)) => Outcome::Timeout,
            Err(ExecError::RouteUnavailable(_) | ExecError::HostNotAllowed { .. }) => {
                Outcome::Route
            }
        };
        let elapsed = start.elapsed();
        let duration = if elapsed.as_secs() < 1 {
            DurationBucket::UnderOneSecond
        } else if elapsed.as_secs() < 10 {
            DurationBucket::UnderTenSeconds
        } else {
            DurationBucket::Longer
        };
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        if log.events.len() < MAX_EVENTS {
            log.events.push(RecordedEvent {
                contract,
                outcome,
                duration,
            });
        } else {
            log.truncated = true;
        }
        result
    }
}
