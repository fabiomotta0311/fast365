//! `f365-teams` — provider Teams: descreve operações REST `chatsvcagg` + WS `Trouter`.
//! Ver `docs/ARQUITETURA.md` §2/§3.
//!
//! Consome `f365_auth::AuthenticatedExecutor` (descreve `AuthOp`, não vê token). Não conhece a
//! WebView, a UI nem o storage. Organizado por capacidade: `chats/`, `send/`, `realtime/`; depois
//! `history/`.

pub mod chats;
pub mod diagnostics;
pub mod history;
pub mod realtime;
pub mod send;

pub use chats::{Conversation, ConversationKind};
pub use history::HistoryMessage;
pub use realtime::RealtimeEvent;
pub use send::{new_client_message_id, SendReceipt};

use f365_auth::{AuthenticatedExecutor, ExecError};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum TeamsError {
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// Status fora de 2xx que o executor devolveu (os de autenticação ele já tratou).
    #[error("{service} respondeu HTTP {status}")]
    Http { service: &'static str, status: u16 },
    #[error("resposta inesperada — {0}")]
    Malformed(&'static str),
    /// Entrada recusada antes de qualquer envio.
    #[error("entrada inválida: {0}")]
    InvalidInput(&'static str),
    /// Falha do canal de tempo real (WebSocket/socket.io) — sem URL nem parâmetros assinados.
    #[error("tempo real: {0}")]
    Realtime(String),
}

/// Cliente do provider Teams.
pub struct TeamsClient {
    exec: Arc<dyn AuthenticatedExecutor>,
    recorder: Option<Arc<diagnostics::Recorder>>,
    #[cfg(test)]
    realtime_test_url: Option<String>,
}

impl TeamsClient {
    pub fn new(exec: Arc<dyn AuthenticatedExecutor>) -> Self {
        Self {
            exec,
            recorder: None,
            #[cfg(test)]
            realtime_test_url: None,
        }
    }

    /// Instrument HTTP and realtime failures into the same bounded bundle.
    pub fn with_recorder(recorder: Arc<diagnostics::Recorder>) -> Self {
        let mut client = Self::new(recorder.clone());
        client.recorder = Some(recorder);
        client
    }

    /// Corpo cru da listagem — **só** para o diagnóstico de campos (`f365 chats --fields`).
    ///
    /// Serve para descobrir de onde tirar o nome de um 1:1 (`TEAMS-CSA-SHAPE-001`). Quem imprime
    /// usa [`chats::field_names`], que mostra apenas nomes de campo: valores nunca saem daqui.
    pub async fn list_raw(&self) -> Result<Vec<u8>, TeamsError> {
        let resp = self.exec.execute(chats::list_op()).await?;
        if !(200..300).contains(&resp.status) {
            return Err(TeamsError::Http {
                service: "chatsvcagg",
                status: resp.status,
            });
        }
        Ok(resp.body)
    }

    /// Conversas e canais visíveis para o usuário (no máximo `top`, na ordem da API).
    pub async fn list_conversations(&self, top: usize) -> Result<Vec<Conversation>, TeamsError> {
        let resp = self.exec.execute(chats::list_op()).await?;
        if !(200..300).contains(&resp.status) {
            return Err(TeamsError::Http {
                service: "chatsvcagg",
                status: resp.status,
            });
        }
        let start = std::time::Instant::now();
        let parsed = chats::parse(&resp.body);
        if let Some(recorder) = &self.recorder {
            recorder.response_protocol(false, parsed.is_ok(), start);
        }
        let mut all = parsed?;
        all.truncate(top);
        Ok(all)
    }

    /// Envia `text` (texto puro) para a conversa. `client_message_id` vem de quem chama
    /// ([`new_client_message_id`]), gerado ANTES: se der erro de rede/prazo, o resultado é
    /// desconhecido e essa é a chave para conferir — nunca reenviar às cegas.
    pub async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
        client_message_id: &str,
    ) -> Result<SendReceipt, TeamsError> {
        let op = send::send_op(chat_id, text, client_message_id)?;
        let resp = self.exec.execute(op).await?;
        if !(200..300).contains(&resp.status) {
            return Err(TeamsError::Http {
                service: "chat regional",
                status: resp.status,
            });
        }
        Ok(send::receipt(&resp, client_message_id))
    }

    // Próximos: history / listen montam `AuthOp` e chamam `exec.execute(..)`. Ver §2.3.
    // Evento do Trouter = invalidação → buscar conteúdo fresco via REST (§2.3).
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use f365_auth::transport::HttpResponse;
    use f365_auth::AuthOp;
    use serde_json::json;
    use std::sync::Mutex;

    /// Executor falso: devolve o roteiro e guarda as operações recebidas.
    struct FakeExec {
        reply: Mutex<Option<Result<HttpResponse, ExecError>>>,
        ops: Mutex<Vec<AuthOp>>,
    }

    impl FakeExec {
        fn replying(r: Result<HttpResponse, ExecError>) -> Arc<Self> {
            Arc::new(Self {
                reply: Mutex::new(Some(r)),
                ops: Mutex::default(),
            })
        }
    }

    #[async_trait]
    impl AuthenticatedExecutor for FakeExec {
        async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
            self.ops.lock().unwrap().push(op);
            self.reply.lock().unwrap().take().expect("uma chamada só")
        }
    }

    fn ok(v: serde_json::Value) -> Result<HttpResponse, ExecError> {
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: v.to_string().into_bytes(),
        })
    }

    #[tokio::test]
    async fn lists_through_the_executor_and_truncates() {
        let exec = FakeExec::replying(ok(json!({
            "chats": [{"id": "1", "title": "A"}, {"id": "2", "title": "B"}, {"id": "3", "title": "C"}]
        })));
        let client = TeamsClient::new(exec.clone());

        let got = client.list_conversations(2).await.unwrap();

        assert_eq!(
            got.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        let ops = exec.ops.lock().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].resource, f365_auth::Resource::ChatSvcAgg);
    }

    #[tokio::test]
    async fn non_success_status_is_reported_with_service_and_code() {
        let exec = FakeExec::replying(Ok(HttpResponse {
            status: 404,
            headers: vec![],
            body: vec![],
        }));
        let err = TeamsClient::new(exec)
            .list_conversations(5)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TeamsError::Http {
                service: "chatsvcagg",
                status: 404
            }
        ));
    }

    #[tokio::test]
    async fn send_goes_through_the_executor_once_and_returns_receipt() {
        let exec = FakeExec::replying(Ok(HttpResponse {
            status: 201,
            headers: vec![],
            body: br#"{"OriginalArrivalTime": 42}"#.to_vec(),
        }));
        let r = TeamsClient::new(exec.clone())
            .send_text("19:a@thread.v2", "oi", "1234567890123456789")
            .await
            .unwrap();
        assert_eq!(r.server_message_id.as_deref(), Some("42"));
        let ops = exec.ops.lock().unwrap();
        assert_eq!(ops.len(), 1);
        assert!(!ops[0].idempotent);
    }

    #[tokio::test]
    async fn history_rejects_dot_segments_before_calling_executor() {
        let exec = FakeExec::replying(Err(ExecError::NotPaired));
        let client = TeamsClient::new(exec.clone());
        for id in [".", "..", "a/b", "a?b", "a#b", "a%b", "a\\b", "a b", "a\n"] {
            assert!(matches!(
                client.history(id, 1).await,
                Err(TeamsError::InvalidInput(_))
            ));
        }
        assert!(exec.ops.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_send_never_reaches_the_executor() {
        let exec = FakeExec::replying(Err(ExecError::NotPaired));
        let err = TeamsClient::new(exec.clone())
            .send_text("19:a/../x", "oi", "1")
            .await
            .unwrap_err();
        assert!(matches!(err, TeamsError::InvalidInput(_)));
        assert!(exec.ops.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn executor_errors_pass_through() {
        let exec = FakeExec::replying(Err(ExecError::NotPaired));
        let err = TeamsClient::new(exec)
            .list_conversations(5)
            .await
            .unwrap_err();
        assert!(matches!(err, TeamsError::Exec(ExecError::NotPaired)));
    }
}
