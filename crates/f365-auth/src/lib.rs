//! `f365-auth` — executor autenticado do core: SSO Microsoft, captura de sessão (WebView, atrás da
//! feature `webview`), keyring, troca de tokens. Ver `docs/ARQUITETURA.md` §1 e `AGENTS.md`.
//!
//! Contrato central: o provider descreve uma OPERAÇÃO (`AuthOp`); a sessão a EXECUTA. O token é
//! detalhe interno — nunca cruza a fronteira de crate.

#[cfg(feature = "webview")]
pub mod capture;
pub mod clock;
pub mod executor;
// Política de captura e parser de sessão só têm consumidor com a WebView (e nos testes).
#[cfg_attr(not(feature = "webview"), allow(dead_code))]
mod probe;
pub mod profile;
mod secret;
#[cfg_attr(not(feature = "webview"), allow(dead_code))]
mod session;
pub mod tokens;
pub mod transport;
#[cfg_attr(not(feature = "webview"), allow(dead_code))]
pub mod vault;

pub use clock::{Clock, ManualClock, SystemClock};
pub use session::AccountInfo;

use async_trait::async_trait;
use std::time::Duration;
use transport::{HttpRequest, HttpResponse};

/// Autorização que a operação exige. Cada uma tem um esquema de credencial e os hosts para onde
/// ela pode ir — o executor recusa qualquer outro (ver `executor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// Agregador REST de conversas: `Authorization: Bearer` (aud `chatsvcagg.teams.microsoft.com`).
    ChatSvcAgg,
    /// Serviço de chat regional (`<região>.ng.msg.teams.microsoft.com`, descoberto no authsvc):
    /// skype token.
    ChatService,
    /// Tempo real (Trouter): skype token no cabeçalho `X-Skypetoken`, só para `*.trouter.teams.
    /// microsoft.com` e o registrador `teams.microsoft.com/registrar/…`.
    Trouter,
    /// Placeholder Fase 4 — OWA/Substrate.
    OutlookSubstrate,
}

/// Operação autenticada declarada por um provider. **NÃO contém token.**
#[derive(Debug, Clone)]
pub struct AuthOp {
    /// Qual autorização a operação exige.
    pub resource: Resource,
    /// Método, path, headers de domínio e body (sem credencial).
    pub request: HttpRequest,
    /// Exige cookies / execução no contexto da página parqueada.
    pub needs_page_context: bool,
    /// Pode ser repetida com segurança (importa para reconciliação de envio).
    pub idempotent: bool,
    /// Prazo máximo da operação.
    pub timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("sessão não pareada (rode `f365 auth`)")]
    NotPaired,
    #[error("falha de autenticação: {0}")]
    Auth(String),
    /// Erro de rede: o efeito no servidor é **desconhecido** — nunca repetido automaticamente.
    #[error("falha de transporte (resultado desconhecido): {0}")]
    Transport(String),
    #[error("rota indisponível: {0}")]
    RouteUnavailable(&'static str),
    /// O destino não é um host permitido para a credencial do recurso: nada foi enviado.
    #[error("host não permitido para {resource:?}: {url}")]
    HostNotAllowed { resource: Resource, url: String },
    #[error("operação excedeu o prazo de {0:?} (resultado desconhecido)")]
    TimedOut(Duration),
}

/// Contrato central do core: executa uma operação autenticada, resolvendo auth + rota de transporte
/// internamente. O token vive dentro da implementação; providers nunca o veem.
#[async_trait]
pub trait AuthenticatedExecutor: Send + Sync {
    async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError>;
}
