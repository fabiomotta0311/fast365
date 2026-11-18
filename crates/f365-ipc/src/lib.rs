//! `f365-ipc` — a superfície que a UI chama (Fase 3, #21). **Sem toolkit.**
//!
//! Aqui mora só o contrato: DTOs serializáveis, comandos assíncronos e a **leitura do cache**. O
//! shell Tauri embrulha cada comando em `#[tauri::command]`; por isso este crate builda no CI
//! headless (sem GTK/WebKit) e é testado sem abrir janela.
//!
//! Regra da fronteira: a UI **nunca** faz I/O de rede nem fala com o executor. Ela chama comandos
//! aqui e lê o que já está no cache — o "render-before-network" da arquitetura.

use f365_auth::Clock;
use f365_driver::{Driver, DriverError, Effect, Round};
use f365_storage::{Conversation, Derivative, Message, StorageError};
use f365_sync::{ConversationId, Event};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;

/// Teto de itens por pedido: a UI não escolhe "tudo", e o cache não vira memória sem limite.
pub const MAX_LIMIT: usize = 200;

/// Erro que a UI precisa distinguir. Nunca carrega segredo nem corpo de resposta.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("cache: {0}")]
    Storage(#[from] StorageError),
    #[error("driver: {0}")]
    Driver(#[from] DriverError),
    #[error("limite inválido: {0} (máximo {MAX_LIMIT})")]
    BadLimit(usize),
    #[error("a busca não pode ser vazia")]
    EmptyQuery,
    #[error("conversa inválida")]
    BadConversation,
    #[error("mensagem inválida")]
    BadMessage,
}

/// Conversa como a UI desenha a lista (lida do cache, sem rede).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversationDto {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub last_activity_ms: Option<i64>,
    /// Texto da última mensagem em cache (sem marcação), para a linha da lista. `None` = vazio.
    pub preview: Option<String>,
    /// Quem mandou a última mensagem, quando o cache sabe.
    pub preview_sender: Option<String>,
}

/// Mensagem como a UI desenha a bolha (texto puro; HTML só com sanitização na Fase 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageDto {
    pub id: String,
    pub conversation_id: String,
    pub client_message_id: Option<String>,
    pub sender: Option<String>,
    pub body: String,
    pub message_type: String,
    pub created_ms: i64,
}

/// Conteúdo derivado (tradução, resumo…) de uma ou mais mensagens. **Nunca** substitui o original:
/// a UI mostra ao lado dele. `stale` = o original mudou depois que isto foi gerado; não é
/// aprovável nem deve ser apresentado como a tradução atual.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DerivativeDto {
    pub id: i64,
    pub kind: String,
    pub model: String,
    pub lang: Option<String>,
    pub body: String,
    pub created_ms: i64,
    pub approved_ms: Option<i64>,
    pub stale: bool,
    pub source_message_ids: Vec<String>,
}

impl From<Derivative> for DerivativeDto {
    fn from(derivative: Derivative) -> Self {
        Self {
            id: derivative.id,
            kind: derivative.kind,
            model: derivative.model,
            lang: derivative.lang,
            body: derivative.body,
            created_ms: derivative.created_ms,
            approved_ms: derivative.approved_ms,
            stale: derivative.stale,
            source_message_ids: derivative
                .sources
                .into_iter()
                .map(|source| source.message_id)
                .collect(),
        }
    }
}

/// Resultado de "carregar mais histórico".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OlderDto {
    pub round: RoundDto,
    /// `true` quando o provider já entregou tudo o que tinha (ou o teto de 1000 foi atingido): a
    /// UI para de pedir mais. `false` também quando a busca não rodou (ex.: já em andamento).
    pub reached_start: bool,
}

/// Quantas mensagens a mais cada "carregar mais" pede ao provider.
const OLDER_STEP: usize = 50;

/// Efeito de uma rodada do motor, no dialeto da UI (tagged enum, fácil de consumir em TS).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectDto {
    ConversationsRefreshed {
        count: usize,
    },
    Fetched {
        conversation: String,
        messages: usize,
    },
    Persisted {
        conversation: String,
    },
    Published {
        conversation: String,
    },
    SendAttempted {
        client_id: String,
    },
    Sent {
        client_id: String,
    },
    /// Não deu para confirmar: a UI **tem** de mostrar como incerto, nunca como entregue.
    Uncertain {
        client_id: String,
    },
    ReconnectAttempt,
    /// O middleware gravou um derivado (tradução, resumo…); o original não mudou.
    Derived {
        message_id: String,
        derivative_kind: String,
        derivative_id: i64,
    },
    /// O `post_receive` falhou nesta mensagem; o resto da busca seguiu.
    MiddlewareFailed {
        message_id: String,
    },
    TokenRefreshed,
    TokenRefreshFailed {
        retry_in_ms: u64,
    },
    /// Só o dono resolve (login). A UI deve oferecer "entrar novamente".
    SessionExpired,
}

/// Resultado de um comando que mexeu no motor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoundDto {
    pub effects: Vec<EffectDto>,
    /// Em quanto tempo o comando deve ser chamado de novo (`wake`), em ms.
    pub wake_in_ms: Option<u64>,
    /// Resumo por capacidade (§2.5), pronto para o rodapé de status.
    pub capabilities: Vec<String>,
}

impl RoundDto {
    fn new(round: Round, now: Instant) -> Self {
        Self {
            effects: round.effects.into_iter().map(EffectDto::from).collect(),
            wake_in_ms: round
                .wake_at
                .map(|at| at.saturating_duration_since(now).as_millis() as u64),
            capabilities: round.capabilities,
        }
    }
}

impl From<Effect> for EffectDto {
    fn from(effect: Effect) -> Self {
        match effect {
            Effect::ConversationsRefreshed(count) => EffectDto::ConversationsRefreshed { count },
            Effect::Fetched {
                conversation,
                messages,
            } => EffectDto::Fetched {
                conversation: conversation.0,
                messages,
            },
            Effect::Persisted(conversation) => EffectDto::Persisted {
                conversation: conversation.0,
            },
            Effect::Published(conversation) => EffectDto::Published {
                conversation: conversation.0,
            },
            Effect::SendAttempted { client_id, .. } => EffectDto::SendAttempted {
                client_id: client_id.0,
            },
            Effect::Sent { client_id, .. } => EffectDto::Sent {
                client_id: client_id.0,
            },
            Effect::Uncertain { client_id, .. } => EffectDto::Uncertain {
                client_id: client_id.0,
            },
            Effect::ReconnectAttempt => EffectDto::ReconnectAttempt,
            Effect::Derived {
                message_id,
                kind,
                derivative_id,
            } => EffectDto::Derived {
                message_id,
                derivative_kind: kind,
                derivative_id,
            },
            Effect::MiddlewareFailed { message_id } => EffectDto::MiddlewareFailed { message_id },
            Effect::TokenRefreshed => EffectDto::TokenRefreshed,
            Effect::TokenRefreshFailed { retry_in } => EffectDto::TokenRefreshFailed {
                retry_in_ms: retry_in.as_millis() as u64,
            },
            Effect::SessionExpired => EffectDto::SessionExpired,
        }
    }
}

impl From<Conversation> for ConversationDto {
    fn from(conversation: Conversation) -> Self {
        Self {
            id: conversation.id,
            title: conversation.title,
            kind: conversation.kind,
            last_activity_ms: conversation.last_activity_ms,
            preview: None,
            preview_sender: None,
        }
    }
}

/// Máximo de caracteres da prévia na lista.
const PREVIEW_CHARS: usize = 140;

/// Texto puro para a prévia: remove marcação HTML, decodifica o básico, junta espaços e corta.
/// A UI o exibe como texto (escapado), então isto é só legibilidade, não sanitização.
fn preview_text(body: &str) -> Option<String> {
    let mut plain = String::with_capacity(body.len());
    let mut in_tag = false;
    for ch in body.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                plain.push(' ');
            }
            _ if in_tag => {}
            _ => plain.push(ch),
        }
    }
    let plain = plain
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= PREVIEW_CHARS {
        return Some(collapsed);
    }
    let cut: String = collapsed.chars().take(PREVIEW_CHARS).collect();
    Some(format!("{}…", cut.trim_end()))
}

impl From<Message> for MessageDto {
    fn from(message: Message) -> Self {
        Self {
            id: message.id,
            conversation_id: message.conversation_id,
            client_message_id: message.client_message_id,
            sender: message.sender,
            body: message.body,
            message_type: message.message_type,
            created_ms: message.created_ms,
        }
    }
}

/// Fachada única: a UI tem um `Shell` e chama comandos. Nada de I/O de rede aqui além do que o
/// driver já executa.
pub struct Shell {
    driver: Driver,
    clock: Arc<dyn Clock>,
}

impl Shell {
    pub fn new(driver: Driver, clock: Arc<dyn Clock>) -> Self {
        Self { driver, clock }
    }

    // ---- leitura (do cache, sem rede) ----

    /// Lista de conversas já em cache. É o que a UI desenha no primeiro frame.
    pub fn conversations(&self, limit: usize) -> Result<Vec<ConversationDto>, IpcError> {
        let limit = checked_limit(limit)?;
        let store = self.driver.store();
        store
            .list_conversations(limit)?
            .into_iter()
            .map(|conversation| {
                let id = conversation.id.clone();
                let mut dto = ConversationDto::from(conversation);
                if let Some(last) = store.page_messages(&id, None, 1)?.into_iter().next() {
                    dto.preview = preview_text(&last.body);
                    dto.preview_sender = last.sender;
                }
                Ok(dto)
            })
            .collect()
    }

    /// Rascunho salvo da conversa (sobrevive a reinício). `None` = não há.
    pub fn draft(&self, conversation_id: &str) -> Result<Option<String>, IpcError> {
        if conversation_id.trim().is_empty() {
            return Err(IpcError::BadConversation);
        }
        Ok(self.driver.store().draft(conversation_id)?)
    }

    /// Grava o rascunho; texto vazio apaga. A conversa precisa existir no cache.
    pub fn set_draft(&self, conversation_id: &str, text: &str) -> Result<(), IpcError> {
        if conversation_id.trim().is_empty() {
            return Err(IpcError::BadConversation);
        }
        let store = self.driver.store();
        if text.trim().is_empty() {
            store.delete_draft(conversation_id)?;
        } else {
            let now_ms = self
                .clock
                .utc_now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or(0);
            store.put_draft(conversation_id, text, now_ms)?;
        }
        Ok(())
    }

    /// Meu nome de exibição, aprendido da listagem. A UI usa para alinhar as minhas mensagens.
    pub fn me(&self) -> Option<String> {
        self.driver.store().my_display_name().ok().flatten()
    }

    /// Página de histórico (keyset: `before_ms` = a mensagem mais antiga já exibida).
    pub fn messages(
        &self,
        conversation_id: &str,
        before_ms: Option<i64>,
        limit: usize,
    ) -> Result<Vec<MessageDto>, IpcError> {
        let limit = checked_limit(limit)?;
        if conversation_id.trim().is_empty() {
            return Err(IpcError::BadConversation);
        }
        Ok(self
            .driver
            .store()
            .page_messages(conversation_id, before_ms, limit)?
            .into_iter()
            .map(MessageDto::from)
            .collect())
    }

    /// Busca no texto local (FTS5). Entrada vazia é recusada — evita varrer o índice inteiro.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MessageDto>, IpcError> {
        let limit = checked_limit(limit)?;
        if query.trim().is_empty() {
            return Err(IpcError::EmptyQuery);
        }
        Ok(self
            .driver
            .store()
            .search(query, limit)?
            .into_iter()
            .map(MessageDto::from)
            .collect())
    }

    /// Estado por capacidade (§2.5), para o rodapé de status.
    pub fn capabilities(&self) -> Vec<String> {
        self.driver.capabilities().summary()
    }

    /// Derivados (traduções, resumos…) que citam a mensagem, do mais novo ao mais antigo. Vem do
    /// cache; o original continua na própria mensagem.
    pub fn derivatives(&self, message_id: &str) -> Result<Vec<DerivativeDto>, IpcError> {
        if message_id.trim().is_empty() {
            return Err(IpcError::BadMessage);
        }
        Ok(self
            .driver
            .store()
            .derivatives_for(message_id)?
            .into_iter()
            .map(DerivativeDto::from)
            .collect())
    }

    /// Aprova **este** derivado e devolve o texto exato aprovado. Recusa o obsoleto.
    pub fn approve_derivative(&self, derivative_id: i64) -> Result<String, IpcError> {
        Ok(self.driver.approve_derivative(derivative_id)?)
    }

    // ---- comandos (mexem no motor) ----

    /// Atualiza a lista de conversas a partir do provider.
    pub async fn refresh_conversations(&mut self) -> Result<RoundDto, IpcError> {
        let round = self.driver.refresh_conversations().await?;
        Ok(RoundDto::new(round, self.clock.now()))
    }

    /// Envia texto. O `clientmessageid` é gerado no driver, antes do envio.
    pub async fn send(&mut self, conversation_id: &str, text: &str) -> Result<RoundDto, IpcError> {
        if conversation_id.trim().is_empty() {
            return Err(IpcError::BadConversation);
        }
        let conversation = ConversationId(conversation_id.to_string());
        let round = self.driver.send_message(&conversation, text).await?;
        Ok(RoundDto::new(round, self.clock.now()))
    }

    /// "Carregar mais histórico": aprofunda a janela da conversa e deixa o motor buscá-la.
    ///
    /// O provider só entrega "as N mais novas" (`HISTORY-PAGE-001`, UNVERIFIED); então isto pede
    /// mais N e relê. Mensagens já em cache são regravadas sem duplicar. `reached_start` diz à UI
    /// quando parar de pedir.
    pub async fn load_older(&mut self, conversation_id: &str) -> Result<OlderDto, IpcError> {
        if conversation_id.trim().is_empty() {
            return Err(IpcError::BadConversation);
        }
        let conversation = ConversationId(conversation_id.to_string());
        let depth = self.driver.deepen(&conversation, OLDER_STEP);
        let round = self
            .driver
            .drive(Event::Invalidated {
                conversation: conversation.clone(),
            })
            .await?;
        let fetched = round.effects.iter().find_map(|effect| match effect {
            Effect::Fetched {
                conversation: fetched,
                messages,
            } if *fetched == conversation => Some(*messages),
            _ => None,
        });
        let reached_start = match fetched {
            Some(count) => count < depth || depth >= f365_driver::MAX_HISTORY,
            None => false,
        };
        Ok(OlderDto {
            round: RoundDto::new(round, self.clock.now()),
            reached_start,
        })
    }

    /// O tempo real avisou que algo mudou (invalidação, não dado).
    pub async fn invalidated(&mut self, conversation_id: &str) -> Result<RoundDto, IpcError> {
        if conversation_id.trim().is_empty() {
            return Err(IpcError::BadConversation);
        }
        let round = self
            .driver
            .drive(Event::Invalidated {
                conversation: ConversationId(conversation_id.to_string()),
            })
            .await?;
        Ok(RoundDto::new(round, self.clock.now()))
    }

    /// A rede voltou (a UI percebeu). Reabre o tempo real e libera o que estava preso.
    pub async fn connection_restored(&mut self) -> Result<RoundDto, IpcError> {
        let round = self.driver.drive(Event::ConnectionRestored).await?;
        Ok(RoundDto::new(round, self.clock.now()))
    }

    /// Acorda o motor no prazo que o último `RoundDto::wake_in_ms` pediu.
    pub async fn wake(&mut self) -> Result<RoundDto, IpcError> {
        let round = self.driver.drive(Event::WakeUp).await?;
        Ok(RoundDto::new(round, self.clock.now()))
    }
}

fn checked_limit(limit: usize) -> Result<usize, IpcError> {
    if limit == 0 || limit > MAX_LIMIT {
        return Err(IpcError::BadLimit(limit));
    }
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use f365_auth::transport::HttpResponse;
    use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError, ManualClock};
    use f365_storage::Store;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    const CHATS: &str = r#"{"chats":[{"id":"19:a@thread.v2","title":"Time"}]}"#;
    const HISTORY: &str = r#"{"messages":[
        {"id":"m1","content":"primeira mensagem","composetime":"2024-01-01T00:00:00Z"},
        {"id":"m2","content":"relatório trimestral","composetime":"2024-01-01T00:00:05Z"}
    ]}"#;

    struct FakeExec {
        replies: Mutex<VecDeque<Result<HttpResponse, ExecError>>>,
        calls: AtomicUsize,
    }

    impl FakeExec {
        fn replying(replies: Vec<Result<HttpResponse, ExecError>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into_iter().collect()),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl AuthenticatedExecutor for FakeExec {
        async fn execute(&self, _op: AuthOp) -> Result<HttpResponse, ExecError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies
                .lock()
                .expect("replies")
                .pop_front()
                .unwrap_or(Err(ExecError::Transport("script vazio".into())))
        }
    }

    fn ok(status: u16, body: &str) -> Result<HttpResponse, ExecError> {
        Ok(HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        })
    }

    fn shell(exec: Arc<FakeExec>) -> (ManualClock, Shell) {
        let clock = ManualClock::default();
        let store = Store::open_in_memory().expect("banco");
        let driver = Driver::new(exec, store, Arc::new(clock.clone()));
        let shell = Shell::new(driver, Arc::new(clock.clone()));
        (clock, shell)
    }

    /// Semeia o cache pelo caminho real: o motor pede, o provider responde, o driver persiste.
    async fn seed(shell: &mut Shell) {
        shell.refresh_conversations().await.expect("conversas");
        shell
            .invalidated("19:a@thread.v2")
            .await
            .expect("histórico");
    }

    /// O caminho que a janela usa: **um** `refresh` já devolve o 1:1 com nome, sem abrir nada.
    #[tokio::test]
    async fn refresh_brings_the_one_on_one_name() {
        const LISTING: &str = r#"{"chats":[{"id":"19:dm@unq.gbl.spaces","title":null,
            "isOneOnOne":true,"isLastMessageFromMe":true,
            "lastMessage":{"imDisplayName":"Fabio Motta"}}]}"#;
        const HISTORY: &str = r#"{"messages":[
            {"id":"m1","content":"oi","composetime":"2024-01-01T00:00:00Z","imdisplayname":"Ana"}
        ]}"#;
        let (_clock, mut shell) =
            shell(FakeExec::replying(vec![ok(200, LISTING), ok(200, HISTORY)]));

        shell.refresh_conversations().await.expect("atualiza");

        let list = shell.conversations(10).expect("lista");
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].title, "Ana",
            "o Atualizar já traz o nome do outro lado"
        );
    }

    #[tokio::test]
    async fn conversations_come_from_the_cache_without_touching_the_network() {
        let exec = FakeExec::replying(vec![ok(200, CHATS)]);
        let (_clock, mut shell) = shell(exec.clone());
        shell.refresh_conversations().await.expect("atualiza");
        let calls_after_refresh = exec.calls();

        let list = shell.conversations(10).expect("lista");

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Time");
        assert_eq!(
            exec.calls(),
            calls_after_refresh,
            "o primeiro frame não espera rede"
        );
    }

    #[tokio::test]
    async fn history_is_paginated_from_the_cache() {
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY)]);
        let (_clock, mut shell) = shell(exec.clone());
        seed(&mut shell).await;
        let calls = exec.calls();

        let page = shell.messages("19:a@thread.v2", None, 1).expect("página");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "m2", "mais nova primeiro");

        let older = shell
            .messages("19:a@thread.v2", Some(page[0].created_ms), 10)
            .expect("próxima");
        assert_eq!(older[0].id, "m1");
        assert_eq!(exec.calls(), calls, "paginar é leitura de cache");
    }

    #[tokio::test]
    async fn search_reads_the_local_index() {
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY)]);
        let (_clock, mut shell) = shell(exec);
        seed(&mut shell).await;

        let hits = shell.search("relatório", 10).expect("busca");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m2");
    }

    #[test]
    fn invalid_limits_and_empty_queries_are_refused() {
        let exec = FakeExec::replying(vec![]);
        let (_clock, shell) = shell(exec);

        assert!(matches!(shell.conversations(0), Err(IpcError::BadLimit(0))));
        assert!(matches!(
            shell.conversations(MAX_LIMIT + 1),
            Err(IpcError::BadLimit(_))
        ));
        assert!(matches!(shell.search("  ", 10), Err(IpcError::EmptyQuery)));
        assert!(matches!(
            shell.messages("", None, 10),
            Err(IpcError::BadConversation)
        ));
    }

    #[tokio::test]
    async fn refresh_reaches_the_provider_and_reports_effects() {
        let exec = FakeExec::replying(vec![ok(200, CHATS)]);
        let (_clock, mut shell) = shell(exec);

        let round = shell.refresh_conversations().await.expect("atualiza");

        assert_eq!(
            round.effects,
            vec![EffectDto::ConversationsRefreshed { count: 1 }]
        );
        assert_eq!(shell.conversations(10).expect("lista").len(), 1);
    }

    #[tokio::test]
    async fn lost_send_ends_uncertain_and_never_reports_sent() {
        // POST perde a resposta; a busca de reconciliação volta vazia (sem prova de entrega).
        let exec = FakeExec::replying(vec![
            Err(ExecError::Transport("caiu".into())),
            ok(200, r#"{"messages":[]}"#),
        ]);
        let (clock, mut shell) = shell(exec);

        let round = shell.send("19:a@thread.v2", "oi").await.expect("tenta");
        assert!(round
            .effects
            .iter()
            .any(|effect| matches!(effect, EffectDto::SendAttempted { .. })));
        assert!(!round
            .effects
            .iter()
            .any(|effect| matches!(effect, EffectDto::Sent { .. })));

        // Vence o prazo de reconciliação: incerto, e a UI é avisada.
        clock.advance(Duration::from_secs(61));
        let round = shell.wake().await.expect("acorda");
        assert!(round
            .effects
            .iter()
            .any(|effect| matches!(effect, EffectDto::Uncertain { .. })));
        assert!(!round
            .effects
            .iter()
            .any(|effect| matches!(effect, EffectDto::Sent { .. })));
    }

    // ---- derivados ----

    struct Translator;

    #[async_trait::async_trait]
    impl f365_driver::Middleware for Translator {
        fn model(&self) -> &str {
            "fake@1"
        }

        async fn post_receive(
            &self,
            incoming: &f365_driver::Incoming,
        ) -> Result<Vec<f365_driver::Derived>, f365_driver::MiddlewareError> {
            Ok(vec![f365_driver::Derived {
                kind: "translation".into(),
                lang: Some("en".into()),
                body: format!("[en] {}", incoming.body),
            }])
        }
    }

    fn shell_with_translator(exec: Arc<FakeExec>) -> Shell {
        let clock = ManualClock::default();
        let store = Store::open_in_memory().expect("banco");
        let driver =
            Driver::new(exec, store, Arc::new(clock.clone())).with_middleware(Arc::new(Translator));
        Shell::new(driver, Arc::new(clock))
    }

    #[tokio::test]
    async fn derivatives_are_listed_next_to_the_untouched_original() {
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY)]);
        let mut shell = shell_with_translator(exec);
        seed(&mut shell).await;

        let list = shell.derivatives("m1").expect("derivados");

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].body, "[en] primeira mensagem");
        assert_eq!(list[0].kind, "translation");
        assert_eq!(list[0].lang.as_deref(), Some("en"));
        assert_eq!(list[0].source_message_ids, vec!["m1".to_string()]);
        assert!(!list[0].stale && list[0].approved_ms.is_none());
        let page = shell.messages("19:a@thread.v2", None, 10).expect("cache");
        assert!(
            page.iter().any(|m| m.body == "primeira mensagem"),
            "o original segue intacto"
        );
    }

    #[tokio::test]
    async fn approving_returns_the_exact_text_and_marks_it() {
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY)]);
        let mut shell = shell_with_translator(exec);
        seed(&mut shell).await;
        let id = shell.derivatives("m1").unwrap()[0].id;

        assert_eq!(
            shell.approve_derivative(id).expect("aprova"),
            "[en] primeira mensagem"
        );
        assert!(shell.derivatives("m1").unwrap()[0].approved_ms.is_some());
    }

    #[tokio::test]
    async fn edited_message_makes_the_derivative_stale_and_unapprovable() {
        let edited = HISTORY.replace("primeira mensagem", "primeira editada");
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY), ok(200, &edited)]);
        let mut shell = shell_with_translator(exec);
        seed(&mut shell).await;
        let old_id = shell.derivatives("m1").unwrap()[0].id;

        shell.invalidated("19:a@thread.v2").await.expect("reedição");

        let list = shell.derivatives("m1").unwrap();
        assert_eq!(list.len(), 2, "o novo e o velho (histórico)");
        assert!(!list[0].stale && list[0].body.contains("editada"));
        assert!(list[1].stale);
        assert!(
            shell.approve_derivative(old_id).is_err(),
            "obsoleto não aprova"
        );
    }

    #[test]
    fn derivative_commands_validate_their_input() {
        let (_clock, shell) = shell(FakeExec::replying(vec![]));
        assert!(matches!(shell.derivatives("  "), Err(IpcError::BadMessage)));
        assert!(shell.approve_derivative(999).is_err());
    }

    // ---- prévia e meu nome ----

    #[test]
    fn preview_strips_markup_collapses_space_and_truncates() {
        assert_eq!(
            preview_text("<p>Olá&nbsp;<b>mundo</b></p>\n<p>tudo   bem &amp; você?</p>").as_deref(),
            Some("Olá mundo tudo bem & você?")
        );
        assert_eq!(preview_text("  <br/> "), None);
        let long = "a".repeat(500);
        let cut = preview_text(&long).expect("prévia");
        assert_eq!(cut.chars().count(), PREVIEW_CHARS + 1);
        assert!(cut.ends_with('…'));
        // Corte em fronteira de caractere, não de byte.
        assert!(preview_text(&"ção ".repeat(100)).is_some());
    }

    #[tokio::test]
    async fn conversation_list_carries_the_last_message_preview() {
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY)]);
        let (_clock, mut shell) = shell(exec);
        seed(&mut shell).await;

        let list = shell.conversations(10).expect("lista");

        assert_eq!(list[0].preview.as_deref(), Some("relatório trimestral"));
    }

    #[test]
    fn conversation_without_messages_has_no_preview() {
        let (_clock, shell) = shell(FakeExec::replying(vec![]));
        shell
            .driver
            .store()
            .upsert_conversation(&Conversation::new("19:vazia@thread.v2", "Vazia", "chat"))
            .expect("conversa");

        let list = shell.conversations(10).expect("lista");

        assert_eq!(list[0].preview, None);
        assert_eq!(list[0].preview_sender, None);
    }

    #[test]
    fn me_comes_from_the_learned_display_name() {
        let (_clock, shell) = shell(FakeExec::replying(vec![]));
        assert_eq!(shell.me(), None);
        shell
            .driver
            .store()
            .set_my_display_name("Fabio Motta", 1)
            .expect("nome");
        assert_eq!(shell.me().as_deref(), Some("Fabio Motta"));
    }

    // ---- rascunho e histórico mais antigo ----

    #[tokio::test]
    async fn draft_roundtrips_and_empty_text_deletes_it() {
        let exec = FakeExec::replying(vec![ok(200, CHATS)]);
        let (_clock, mut shell) = shell(exec);
        shell.refresh_conversations().await.expect("conversas");

        assert_eq!(shell.draft("19:a@thread.v2").unwrap(), None);
        shell.set_draft("19:a@thread.v2", "rascunho").unwrap();
        assert_eq!(
            shell.draft("19:a@thread.v2").unwrap().as_deref(),
            Some("rascunho")
        );
        shell.set_draft("19:a@thread.v2", "   ").unwrap();
        assert_eq!(shell.draft("19:a@thread.v2").unwrap(), None);
    }

    #[test]
    fn draft_commands_validate_the_conversation() {
        let (_clock, shell) = shell(FakeExec::replying(vec![]));
        assert!(matches!(shell.draft(" "), Err(IpcError::BadConversation)));
        assert!(matches!(
            shell.set_draft("", "x"),
            Err(IpcError::BadConversation)
        ));
        assert!(
            shell.set_draft("19:desconhecida", "x").is_err(),
            "rascunho exige a conversa em cache"
        );
    }

    #[tokio::test]
    async fn load_older_reports_the_start_when_the_provider_has_no_more() {
        // HISTORY traz 2 mensagens; a janela pedida (30 + 50) é maior que isso.
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY), ok(200, HISTORY)]);
        let (_clock, mut shell) = shell(exec.clone());
        seed(&mut shell).await;

        let older = shell.load_older("19:a@thread.v2").await.expect("mais");

        assert!(older.reached_start);
        assert!(exec.calls() >= 3, "buscou de novo");
        assert!(matches!(
            shell.load_older("").await,
            Err(IpcError::BadConversation)
        ));
    }

    #[tokio::test]
    async fn load_older_without_a_fetch_does_not_claim_the_start() {
        // Falha de rede na busca: não dá para afirmar que o começo foi alcançado.
        let exec = FakeExec::replying(vec![ok(200, CHATS), ok(200, HISTORY), ok(500, "{}")]);
        let (_clock, mut shell) = shell(exec);
        seed(&mut shell).await;

        let older = shell.load_older("19:a@thread.v2").await.expect("rodada");

        assert!(!older.reached_start);
    }
}
