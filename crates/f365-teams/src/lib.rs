//! `f365-teams` — provider Teams: REST `chatsvcagg` + WebSocket `Trouter`.
//! Ver `docs/ARQUITETURA.md` §2 e §3.
//!
//! Consome [`f365_auth::SessionProvider`] para obter tokens; não conhece a WebView,
//! a UI nem o storage.

use f365_auth::SessionProvider;
use std::sync::Arc;

/// Cliente do provider Teams (Fase 0 — stubs).
pub struct TeamsClient {
    #[allow(dead_code)]
    session: Arc<dyn SessionProvider>,
}

impl TeamsClient {
    pub fn new(session: Arc<dyn SessionProvider>) -> Self {
        Self { session }
    }

    // Fase 0-B — GET chatsvcagg → lista conversas/canais (§2.3).
    // pub async fn list_chats(&self, top: usize) -> anyhow::Result<Vec<Chat>> { todo!() }

    // Fase 0-D — POST chatsvcagg → envia mensagem (§2.3).
    // pub async fn send(&self, chat_id: &str, text: &str) -> anyhow::Result<String> { todo!() }

    // Fase 0-C — conecta no Trouter (WS); evento = invalidação (§2.3).
    // pub async fn listen(&self) -> anyhow::Result<impl futures_util::Stream<Item = Event>> { todo!() }
}
