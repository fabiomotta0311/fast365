//! Structured diagnostics: no free-form strings, request bodies, headers or URLs.
mod recorder;
pub use recorder::{RecordedBundle, Recorder};
