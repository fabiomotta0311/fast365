//! Transport trait — abstrai *como* o request HTTP sai, para degradar graciosamente
//! quando a Microsoft bloquear o caminho puro. Ver `docs/ARQUITETURA.md` §1 e §2.1.
//!
//! - [`RustTransport`] (padrão): request puro via `reqwest` (rápido, leve).
//! - `InPageTransport` (fallback, a implementar no #9): executa `fetch` DENTRO da WebView
//!   parqueada, no contexto genuíno do cliente (cookies + token que o MSAL da página já renova).
//!
//! A política de transporte usa (1) por padrão e cai para (2) em 401/403/challenge
//! de Conditional Access. WebSocket e chamadas cookie-bound rodam sempre via (2).

use async_trait::async_trait;
use std::time::Duration;

/// Cabeçalhos cujo valor nunca aparece em `Debug`.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "x-skypetoken",
    "authentication",
];

#[derive(Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Corpo e cabeçalhos de autenticação podem conter RT/token/cookie: `Debug` os omite.
fn redacted_headers(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(k, v)| {
            let hidden = SENSITIVE_HEADERS.iter().any(|s| k.eq_ignore_ascii_case(s));
            (k.as_str(), if hidden { "<oculto>" } else { v.as_str() })
        })
        .collect()
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &redacted_headers(&self.headers))
            .field(
                "body",
                &self
                    .body
                    .as_ref()
                    .map(|b| format!("<{} bytes ocultos>", b.len())),
            )
            .finish()
    }
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &redacted_headers(&self.headers))
            .field("body", &format!("<{} bytes ocultos>", self.body.len()))
            .finish()
    }
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

/// Rota Rust: `reqwest` com rustls. Não segue redirecionamento (o chamador decide) e não guarda
/// cookies entre requests (sessão é do executor, não do transporte).
pub struct RustTransport {
    client: reqwest::Client,
}

impl RustTransport {
    pub fn new() -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| TransportError::Other(e.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Transport for RustTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let mut builder = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        // Mensagem de erro do reqwest pode conter a URL; nunca o corpo nem os cabeçalhos.
        let resp = builder
            .send()
            .await
            .map_err(|e| TransportError::Other(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = resp
            .bytes()
            .await
            .map_err(|e| TransportError::Other(e.without_url().to_string()))?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_body_and_auth_headers() {
        let req = HttpRequest {
            method: "POST".into(),
            url: "https://login.microsoftonline.com/t/oauth2/v2.0/token".into(),
            headers: vec![
                ("Authorization".into(), "Bearer eyJ-segredo".into()),
                ("Origin".into(), "https://teams.cloud.microsoft".into()),
            ],
            body: Some(b"refresh_token=rt-segredo".to_vec()),
        };
        let shown = format!("{req:?}");
        assert!(!shown.contains("segredo"), "{shown}");
        assert!(
            shown.contains("teams.cloud.microsoft"),
            "cabeçalho comum continua visível"
        );

        let resp = HttpResponse {
            status: 200,
            headers: vec![("Set-Cookie".into(), "ESTSAUTH=segredo".into())],
            body: b"{\"access_token\":\"segredo\"}".to_vec(),
        };
        assert!(!format!("{resp:?}").contains("segredo"));
    }
}
