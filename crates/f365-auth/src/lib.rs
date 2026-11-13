//! `f365-auth` — SSO Microsoft, captura de sessão (WebView), armazenamento no keyring
//! e token-exchange genérico. Ver `docs/ARQUITETURA.md` §1 e §2.1.
//!
//! Este crate NÃO conhece Teams nem Outlook — só os conceitos de `Resource`/`scope`.
//! Os providers (`f365-teams`, futuro `f365-outlook`) consomem o trait [`SessionProvider`].

pub mod transport;

use async_trait::async_trait;
use std::time::SystemTime;

/// Resource/scope alvo de um access token (o `aud` que o provider precisa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// Tokens de chat/skype do Teams (`chatsvcagg` + Trouter).
    TeamsChatSvc,
    /// Placeholder Fase 4 — OWA/Substrate.
    OutlookSubstrate,
}

/// Access token com validade, entregue pela sessão sob demanda.
#[derive(Debug, Clone)]
pub struct AccessToken {
    pub value: String,
    pub expires_at: SystemTime,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("sessão não encontrada / não pareada (rode `f365 auth`)")]
    NotPaired,
    #[error("falha ao renovar token: {0}")]
    Refresh(String),
}

/// Entrega um access token válido para um `resource`, renovando em silêncio se expirado.
///
/// Implementado na Fase 0/2 por um provider concreto que lê o refresh token do keyring
/// e troca pelos tokens da cadeia (ver §2.2).
#[async_trait]
pub trait SessionProvider: Send + Sync {
    async fn token_for(&self, resource: Resource) -> Result<AccessToken, AuthError>;
}
