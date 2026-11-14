//! `f365-teams` — provider Teams: descreve operações REST `chatsvcagg` + WS `Trouter`.
//! Ver `docs/ARQUITETURA.md` §2/§3.
//!
//! Consome `f365_auth::AuthenticatedExecutor` (descreve `AuthOp`, não vê token). Não conhece a
//! WebView, a UI nem o storage. Organizado por capacidade: `chats/` (aqui), depois `history/`,
//! `send/`, `realtime/`.

pub mod chats;

pub use chats::{Conversation, ConversationKind};

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
}

/// Cliente do provider Teams.
pub struct TeamsClient {
    exec: Arc<dyn AuthenticatedExecutor>,
}

impl TeamsClient {
    pub fn new(exec: Arc<dyn AuthenticatedExecutor>) -> Self {
        Self { exec }
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
        let mut all = chats::parse(&resp.body)?;
        all.truncate(top);
        Ok(all)
    }

    // Próximos: history / send / listen montam `AuthOp` e chamam `exec.execute(..)`. Ver §2.3.
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
    async fn executor_errors_pass_through() {
        let exec = FakeExec::replying(Err(ExecError::NotPaired));
        let err = TeamsClient::new(exec)
            .list_conversations(5)
            .await
            .unwrap_err();
        assert!(matches!(err, TeamsError::Exec(ExecError::NotPaired)));
    }
}
