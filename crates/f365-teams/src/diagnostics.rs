//! Structured diagnostics: no free-form strings, request bodies, headers or URLs.
mod recorder;
pub(crate) use recorder::WebSocketFailure;
pub use recorder::{RecordedBundle, Recorder};
