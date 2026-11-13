//! Transport trait — abstrai *como* o request HTTP sai, para degradar graciosamente
//! quando a Microsoft bloquear o caminho puro. Ver `docs/ARQUITETURA.md` §1 e §2.1.
//!
//! - `RustTransport`  (padrão, a implementar): request puro via `reqwest` (rápido, leve).
//! - `InPageTransport` (fallback, a implementar): executa `fetch` DENTRO da WebView parqueada,
//!   no contexto genuíno do cliente (cookies + token que o MSAL da página já renova).
//!
//! A política de transporte usa (1) por padrão e cai para (2) em 401/403/challenge
//! de Conditional Access. WebSocket e chamadas cookie-bound rodam sempre via (2).

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("erro de transporte: {0}")]
    Other(String),
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError>;
}
