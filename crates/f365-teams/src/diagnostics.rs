//! Structured diagnostics: no free-form strings, request bodies, headers or URLs.
//!
//! This is a building block, not automatic incident collection. Callers must only
//! supply genuine measurements, never encode user data in numeric fields.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub enum Contract {
    #[serde(rename = "SEND-RECONCILE-001")]
    SendReconcile,
    #[serde(rename = "HISTORY-PAGE-001")]
    HistoryPage,
    #[serde(rename = "TROUTER-HANDSHAKE-001")]
    TrouterHandshake,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Uncertain,
    Failed,
    Succeeded,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Timeout,
    Network,
    Authentication,
    Protocol,
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub contract: Contract,
    pub state: State,
    pub error: Option<ErrorCategory>,
    pub http_status: Option<u16>,
    pub elapsed_ms: u64,
    pub count: u32,
}

#[derive(Debug, Serialize)]
pub struct Bundle {
    schema_version: u8,
    events: Vec<Event>,
}

impl Bundle {
    pub fn new(events: Vec<Event>) -> Self {
        Self {
            schema_version: 1,
            events,
        }
    }
}
