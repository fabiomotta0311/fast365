//! `f365-teams` — provider Teams: descreve operações REST `chatsvcagg` + WS `Trouter`.
//! Ver `docs/ARQUITETURA.md` §2/§3.
//!
//! Consome `f365_auth::AuthenticatedExecutor` (descreve `AuthOp`, não vê token). Não conhece a
//! WebView, a UI nem o storage. Organizar por capacidade: `chats/`, `history/`, `send/`, `realtime/`.

use f365_auth::AuthenticatedExecutor;
use std::sync::Arc;

/// Cliente do provider Teams (Fase 0 — stubs).
pub struct TeamsClient {
    #[allow(dead_code)]
    exec: Arc<dyn AuthenticatedExecutor>,
}

impl TeamsClient {
    pub fn new(exec: Arc<dyn AuthenticatedExecutor>) -> Self {
        Self { exec }
    }

    // Fase 0: list_chats / send / listen montam um `AuthOp` e chamam `exec.execute(..)`. Ver §2.3.
    // Evento do Trouter = invalidação → buscar conteúdo fresco via REST (§2.3).
}
