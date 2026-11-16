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
            .field("url", &self.url.split(['?', '#']).next().unwrap_or(""))
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

fn is_history_request(req: &HttpRequest) -> bool {
    if req.method != "GET" {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(&req.url) else {
        return false;
    };
    if !url
        .host_str()
        .is_some_and(|host| host.ends_with(".teams.microsoft.com"))
    {
        return false;
    }
    let Some(rest) = url.path().strip_prefix("/v1/users/ME/conversations/") else {
        return false;
    };
    let Some(chat) = rest.strip_suffix("/messages") else {
        return false;
    };
    !chat.is_empty() && !chat.contains('/')
}

#[async_trait]
impl Transport for RustTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        // A API existente não carrega opções por request. Circunscreve-se o limite
        // ao GET do endpoint regional de histórico, sem afetar POSTs ou outros hosts.
        let history = is_history_request(&req);
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
        // Conta bytes descomprimidos, inclusive respostas sem Content-Length confiável.
        const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
        let mut resp = resp;
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| TransportError::Other(e.without_url().to_string()))?
        {
            if history && chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(TransportError::Other(
                    "corpo HTTP excede limite de 8 MiB".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
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

    fn history_transport(addr: std::net::SocketAddr) -> RustTransport {
        RustTransport {
            client: reqwest::Client::builder()
                .no_proxy()
                .resolve("regional.teams.microsoft.com", addr)
                .build()
                .unwrap(),
        }
    }

    #[test]
    fn limit_applies_only_to_regional_history_get() {
        let mut req = HttpRequest { method: "GET".into(), url: "https://regional.teams.microsoft.com/v1/users/ME/conversations/48:notes/messages?cursor=x".into(), headers: vec![], body: None };
        assert!(is_history_request(&req));
        req.method = "POST".into();
        assert!(!is_history_request(&req));
        req.method = "GET".into();
        req.url = "https://other.example/v1/users/ME/conversations/48:notes/messages".into();
        assert!(!is_history_request(&req));
        req.url =
            "https://regional.teams.microsoft.com/v1/users/ME/conversations/48:notes/messages/1"
                .into();
        assert!(!is_history_request(&req));
    }

    #[tokio::test]
    async fn unrelated_endpoint_accepts_body_larger_than_history_limit() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8388609\r\n\r\n")
                .unwrap();
            let _ = socket.write_all(&vec![b'x'; 8 * 1024 * 1024 + 1]);
        });
        let response = RustTransport::new()
            .unwrap()
            .send(HttpRequest {
                method: "GET".into(),
                url: format!("http://{addr}/other"),
                headers: vec![],
                body: None,
            })
            .await;
        server.join().unwrap();
        assert_eq!(response.unwrap().body.len(), 8 * 1024 * 1024 + 1);
    }

    #[tokio::test]
    async fn response_exactly_at_limit_is_accepted() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8388608\r\n\r\n")
                .unwrap();
            socket.write_all(&vec![b'x'; 8 * 1024 * 1024]).unwrap();
        });
        let response = history_transport(addr)
            .send(HttpRequest {
                method: "GET".into(),
                url: format!("http://regional.teams.microsoft.com:{}/v1/users/ME/conversations/48:notes/messages", addr.port()),
                headers: vec![],
                body: None,
            })
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.body.len(), 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn oversized_chunked_response_is_rejected_without_leaking_body() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..9 {
                if socket
                    .write_all(b"100000\r\n")
                    .and_then(|_| socket.write_all(&chunk))
                    .and_then(|_| socket.write_all(b"\r\n"))
                    .is_err()
                {
                    break;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n");
        });
        let result = history_transport(addr)
            .send(HttpRequest {
                method: "GET".into(),
                url: format!("http://regional.teams.microsoft.com:{}/v1/users/ME/conversations/48:notes/messages?secret=syntheticsecret", addr.port()),
                headers: vec![],
                body: None,
            })
            .await;
        server.join().unwrap();
        let error = result.unwrap_err().to_string();
        assert!(!error.contains("syntheticsecret"));
        assert!(!error.contains("xxxx"));
    }

    #[test]
    fn debug_hides_signed_url_query_and_fragment() {
        let req = HttpRequest {
            method: "GET".into(),
            url: "https://example.test/path?sig=syntheticsecret#syntheticfragment".into(),
            headers: vec![],
            body: None,
        };
        let debug = format!("{req:?}");
        assert!(!debug.contains("syntheticsecret"), "{debug}");
        assert!(!debug.contains("syntheticfragment"), "{debug}");
    }

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
