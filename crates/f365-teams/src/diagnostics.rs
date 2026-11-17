//! Structured diagnostics: no free-form strings, request bodies, headers or URLs.
//!
//! Collection is opt-in: wrap an executor with [`Recorder`] and construct a
//! `TeamsClient::with_recorder(recorder.clone())`. After operations complete, call
//! `recorder.bundle().export_json()` to obtain a JSON snapshot in memory. This module
//! neither writes files nor sends the snapshot anywhere. Ask for consent before export
//! or sharing; if saving it, use an explicitly chosen private destination and OS-specific
//! owner-only permissions. The current CLI does not expose this API.
mod recorder;
pub(crate) use recorder::WebSocketFailure;
pub use recorder::{RecordedBundle, Recorder};
