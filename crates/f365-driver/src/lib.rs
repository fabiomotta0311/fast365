//! `f365-driver` — a costura entre o motor (decisão) e o mundo (rede + disco). Fase 2.
//!
//! O motor devolve [`Decision`]; quem executa é este crate: busca histórico, envia, grava no cache,
//! publica à UI e devolve o resultado ao motor como [`Event::OperationFinished`]. Tudo é injetado
//! (executor autenticado, [`Clock`], [`Realtime`], [`Notifier`]), então o caminho inteiro roda em
//! teste sem rede e sem esperar.
//!
//! Fronteira: é o **único** lugar que conhece motor **e** cache. O motor segue sem I/O e o storage
//! segue sem decisão.
//!
//! O que este crate **não** inventa: qual campo da resposta prova entrega. `HISTORY-SHAPE-001` ainda
//! não expõe `clientmessageid` no conteúdo buscado, então a reconciliação automática fica atrás de
//! [`Driver::note_observed`] — quem souber do fato (o provider, quando mapear o campo) avisa. Sem
//! aviso, o envio termina em `Uncertain`, que é o comportamento seguro (`SEND-RECONCILE-001`).

use f365_auth::{AuthenticatedExecutor, Clock, ExecError};
use f365_storage::{
    Conversation as StoredConversation, Message as StoredMessage, StorageError, Store,
};
use f365_sync::capability::{Capabilities, Capability, DegradedReason, SessionGeneration};
use f365_sync::{
    ClientMessageId, Config, ConversationId, Decision, Event, Outcome, SendState, SyncMachine,
};
use f365_teams::{
    Conversation as TeamsConversation, ConversationKind, HistoryMessage, TeamsClient, TeamsError,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Profundidade máxima de uma cadeia causal; operações independentes não consomem o orçamento umas das outras.
const MAX_STEPS: usize = 64;

/// Primeira renovação silenciosa: o access token dura ~1 h, então renovamos com folga.
const REFRESH_FIRST: Duration = Duration::from_secs(30 * 60);
/// Backoff de renovação que falhou (dobra até o teto) — nunca fica martelando o login.
const REFRESH_BACKOFF_MIN: Duration = Duration::from_secs(5);
const REFRESH_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// Prefixo do título provisório de 1:1 (o provider usa `1:1 · <sufixo>` quando não há nome).
const ONE_ON_ONE_TITLE_PREFIX: &str = "1:1 · ";

/// Teto de conversas varridas ao completar títulos a partir do cache.
const TITLE_REPAIR_LIMIT: usize = 500;

/// Buscas de histórico por `refresh` para descobrir o nome de um 1:1 (uma por conversa pendente).
const NAMES_FETCH_LIMIT: usize = 25;

/// Piso de mensagens lidas ao descobrir o nome do outro lado.
///
/// Tem de acompanhar o `top` do driver: com janela menor que a da conversa aberta, um 1:1 em que as
/// últimas N mensagens são todas minhas fica sem nome no `refresh` e só se resolve ao clicar.
const NAMES_FETCH_TOP: usize = 20;

/// O outro lado de um 1:1: o remetente mais frequente que **não** sou eu.
///
/// Empate resolvido pelo nome, para o resultado não depender da ordem da página.
fn counterpart<'a>(senders: impl Iterator<Item = &'a str>, mine: &str) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for sender in senders {
        if sender == mine {
            continue;
        }
        *counts.entry(sender).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(name, _)| name.to_string())
}

/// Falha de uma operação, já classificada para o motor e para o modelo de capacidade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpFailure {
    /// Resultado **desconhecido** (rede/prazo): para envio, reconcilia — nunca repete às cegas.
    Lost,
    /// Falhou de forma retentável (5xx, tempo real caiu).
    Retryable,
    /// Recusa definitiva desta rota.
    Refused,
    /// Sessão inválida: invalida o conhecimento da geração (rotas incluídas).
    Auth,
}

impl OpFailure {
    fn outcome(self) -> Outcome {
        match self {
            OpFailure::Lost => Outcome::Lost,
            OpFailure::Retryable => Outcome::Failed { retryable: true },
            OpFailure::Refused | OpFailure::Auth => Outcome::Failed { retryable: false },
        }
    }

    fn classify(error: &TeamsError) -> Self {
        match error {
            TeamsError::Exec(ExecError::Transport(_) | ExecError::TimedOut(_)) => OpFailure::Lost,
            TeamsError::Exec(ExecError::NotPaired | ExecError::Auth(_)) => OpFailure::Auth,
            TeamsError::Exec(_) => OpFailure::Refused,
            TeamsError::Http { status, .. } if *status >= 500 || *status == 429 => {
                OpFailure::Retryable
            }
            TeamsError::Http { .. } => OpFailure::Refused,
            TeamsError::Realtime(_) => OpFailure::Retryable,
            TeamsError::Malformed(_) | TeamsError::InvalidInput(_) => OpFailure::Refused,
        }
    }
}

/// Erro do driver (o que não é resultado de operação, e sim falha de composição).
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("armazenamento: {0}")]
    Storage(#[from] StorageError),
    #[error("provider: {0}")]
    Provider(#[from] TeamsError),
    #[error("o motor não convergiu em 64 transições")]
    NotConverged,
    #[error("nada a enviar: o texto está vazio")]
    EmptyText,
    /// O middleware recusou o envio (ex.: tradução indisponível). Nada foi enviado nem registrado:
    /// não se manda em silêncio o texto original quando o usuário pediu a versão transformada.
    #[error("middleware pre_send: {0}")]
    Middleware(String),
}

/// O que aconteceu de observável nesta rodada. A UI/IPC da Fase 3 consome isto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Uma conversa foi gravada no cache.
    Persisted(ConversationId),
    /// A UI deve reler essa conversa.
    Published(ConversationId),
    /// Histórico buscado e persistido.
    Fetched {
        conversation: ConversationId,
        messages: usize,
    },
    /// Um envio saiu para o executor.
    SendAttempted {
        conversation: ConversationId,
        client_id: ClientMessageId,
    },
    /// Envio confirmado pelo servidor.
    Sent {
        conversation: ConversationId,
        client_id: ClientMessageId,
    },
    /// Envio que não deu para confirmar — a UI precisa mostrar como incerto.
    Uncertain {
        conversation: ConversationId,
        client_id: ClientMessageId,
    },
    /// Tentativa de reabrir o canal de tempo real.
    ReconnectAttempt,
    /// Lista de conversas atualizada no cache.
    ConversationsRefreshed(usize),
    /// O middleware gravou um derivado (tradução, resumo…) de uma mensagem. O original não mudou.
    Derived {
        message_id: String,
        kind: String,
        derivative_id: i64,
    },
    /// O `post_receive` falhou numa mensagem. É isolado: a busca e as demais mensagens seguem.
    MiddlewareFailed { message_id: String },
    /// Token renovado em silêncio (sem WebView).
    TokenRefreshed,
    /// Renovação falhou de forma retentável; o driver reagendou.
    TokenRefreshFailed { retry_in: Duration },
    /// A sessão precisa de login novo: tudo voltou a `NeedsAuth`.
    SessionExpired,
}

/// Resultado de uma rodada: efeitos + próximos passos para quem chama.
#[derive(Debug, Default)]
pub struct Round {
    pub effects: Vec<Effect>,
    /// Quando o driver precisa ser acordado (`Decision::ScheduleWake`). Quem chama decide dormir.
    pub wake_at: Option<Instant>,
    /// Resumo por capacidade, pronto para a UI (§2.5).
    pub capabilities: Vec<String>,
}

/// Canal de tempo real. O driver trata a falha como capacidade isolada.
#[async_trait::async_trait]
pub trait Realtime: Send + Sync {
    async fn reconnect(&self) -> Result<(), String>;
}

/// Composição sem tempo real (o laço do Trouter entra numa próxima fatia).
pub struct NoRealtime;

#[async_trait::async_trait]
impl Realtime for NoRealtime {
    async fn reconnect(&self) -> Result<(), String> {
        Err("tempo real não composto nesta execução".into())
    }
}

/// Renovação silenciosa do token (sem WebView). O driver só agenda; quem renova é o keeper.
#[async_trait::async_trait]
pub trait TokenKeeper: Send + Sync {
    /// Renova agora e diz quanto tempo até a próxima renovação.
    async fn refresh(&self) -> std::result::Result<Duration, RefreshError>;
}

/// Falha de renovação. `retryable = false` significa "só login do dono resolve".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshError {
    pub retryable: bool,
    pub reason: String,
}

impl RefreshError {
    pub fn retryable(reason: impl Into<String>) -> Self {
        Self {
            retryable: true,
            reason: reason.into(),
        }
    }

    pub fn needs_login(reason: impl Into<String>) -> Self {
        Self {
            retryable: false,
            reason: reason.into(),
        }
    }
}

/// Quem é avisado quando uma conversa muda (UI, IPC, log).
pub trait Notifier: Send + Sync {
    fn published(&self, conversation: &ConversationId);
}

pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn published(&self, _conversation: &ConversationId) {}
}

/// Texto prestes a sair, entregue ao `pre_send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub conversation: ConversationId,
    pub text: String,
}

/// Mensagem **já gravada** no cache, entregue ao `post_receive`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub conversation: ConversationId,
    pub message_id: String,
    pub sender: Option<String>,
    pub body: String,
    pub message_type: String,
    /// Revisão do fato (1 = como chegou). O derivado fica preso a ela.
    pub revision: i64,
}

/// Conteúdo derivado produzido pelo `post_receive`. Vira linha em `derivatives`
/// (`docs/ARQUITETURA.md` §6): nunca substitui a mensagem original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    /// `translation`, `summary`, `minutes`, `proposed_action`…
    pub kind: String,
    pub lang: Option<String>,
    pub body: String,
}

/// Falha de um middleware. O texto **não** pode carregar conteúdo de mensagem (vai a log/UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiddlewareError(pub String);

/// Seam de middleware (§6): lógica própria no pipeline de mensagens, sem tocar no core.
///
/// - `pre_send` roda **uma vez**, em [`Driver::send_message`], antes de o envio existir. O texto
///   devolvido é o que fica registrado e é o que reenvios/reconciliação usam — nunca é recalculado,
///   então "o texto aprovado" e "o texto enviado" são o mesmo.
/// - `post_receive` roda depois de a mensagem estar no cache e devolve **derivados**. Não existe
///   caminho para trocar o corpo da mensagem: o fato é imutável por aqui (#22).
///
/// O que este seam **não** faz: escolher motor de tradução, chamar rede por conta própria no
/// driver, nem impor timeout — quem implementa responde por latência (a busca espera o
/// `post_receive`).
#[async_trait::async_trait]
pub trait Middleware: Send + Sync {
    /// Identificador estável do modelo/versão (ex.: `deepl@2026-01`). Vai em `derivatives.model` e
    /// é a chave para não reprocessar: mensagem que já tem derivado válido deste modelo é pulada.
    fn model(&self) -> &str;

    async fn pre_send(&self, outgoing: Outgoing) -> Result<Outgoing, MiddlewareError> {
        Ok(outgoing)
    }

    async fn post_receive(&self, _incoming: &Incoming) -> Result<Vec<Derived>, MiddlewareError> {
        Ok(Vec::new())
    }
}

/// Costura motor ⇄ executor ⇄ cache.
pub struct Driver {
    clock: Arc<dyn Clock>,
    machine: SyncMachine,
    store: Store,
    client: TeamsClient,
    realtime: Arc<dyn Realtime>,
    notifier: Arc<dyn Notifier>,
    capabilities: Capabilities,
    generation: u64,
    top: usize,
    /// Texto por envio. Some no restart — e então o envio fica sem como sair (ver `send`).
    pending_text: BTreeMap<ClientMessageId, String>,
    /// Renovação silenciosa de token: o driver só agenda, quem renova é o keeper.
    token_keeper: Option<Arc<dyn TokenKeeper>>,
    refresh_at: Option<Instant>,
    refresh_backoff: Duration,
    /// Meu nome de exibição, inferido da listagem (`f365_teams::Listing`): é o que permite
    /// descobrir a outra pessoa de um 1:1 cruzando com os remetentes do histórico.
    my_display_name: Option<String>,
    middleware: Option<Arc<dyn Middleware>>,
}

impl Driver {
    pub fn new(exec: Arc<dyn AuthenticatedExecutor>, store: Store, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        // Nome aprendido numa execução anterior: sem isso o 1:1 volta a ficar sem nome.
        let my_display_name = store.my_display_name().ok().flatten();
        Self {
            machine: SyncMachine::new(),
            store,
            client: TeamsClient::new(exec),
            realtime: Arc::new(NoRealtime),
            notifier: Arc::new(NoNotifier),
            capabilities: Capabilities::new(now, SessionGeneration(1)),
            generation: 1,
            top: 30,
            pending_text: BTreeMap::new(),
            token_keeper: None,
            refresh_at: None,
            refresh_backoff: REFRESH_BACKOFF_MIN,
            my_display_name,
            middleware: None,
            clock,
        }
    }

    /// Liga a renovação em background (primeira antes do `exp` do access token).
    pub fn with_token_keeper(mut self, keeper: Arc<dyn TokenKeeper>) -> Self {
        let now = self.clock.now();
        self.token_keeper = Some(keeper);
        self.refresh_at = Some(now + REFRESH_FIRST);
        self
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.machine = SyncMachine::with_config(config);
        self
    }

    pub fn with_realtime(mut self, realtime: Arc<dyn Realtime>) -> Self {
        self.realtime = realtime;
        self
    }

    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Liga o seam de middleware (`pre_send`/`post_receive`).
    pub fn with_middleware(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.middleware = Some(middleware);
        self
    }

    pub fn with_top(mut self, top: usize) -> Self {
        self.top = top.max(1);
        self
    }

    pub fn machine(&self) -> &SyncMachine {
        &self.machine
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Envia texto: gera o `clientmessageid` **antes** e registra o texto (é a chave de
    /// reconciliação se a resposta se perder).
    pub async fn send_message(
        &mut self,
        conversation: &ConversationId,
        text: &str,
    ) -> Result<Round, DriverError> {
        if text.trim().is_empty() {
            return Err(DriverError::EmptyText);
        }
        // O texto final é decidido **uma vez**, antes de existir envio: reenvio e reconciliação
        // reusam `pending_text`, nunca chamam o middleware de novo.
        let text = match self.middleware.clone() {
            Some(middleware) => {
                let outgoing = Outgoing {
                    conversation: conversation.clone(),
                    text: text.to_string(),
                };
                middleware
                    .pre_send(outgoing)
                    .await
                    .map_err(|error| DriverError::Middleware(error.0))?
                    .text
            }
            None => text.to_string(),
        };
        if text.trim().is_empty() {
            return Err(DriverError::EmptyText);
        }
        let client_id = ClientMessageId(f365_teams::new_client_message_id());
        self.pending_text.insert(client_id.clone(), text);
        self.drive(Event::SendRequested {
            conversation: conversation.clone(),
            client_id,
        })
        .await
    }

    /// Alguém (o provider, um teste, um comando do dono) **provou** que a mensagem existe no
    /// servidor. É o único caminho para tirar um envio de `Uncertain`/`Reconciling`.
    pub async fn note_observed(
        &mut self,
        conversation: &ConversationId,
        client_id: &ClientMessageId,
    ) -> Result<Round, DriverError> {
        self.drive(Event::MessageObserved {
            conversation: conversation.clone(),
            client_id: client_id.clone(),
        })
        .await
    }

    /// Preenche o cache com a lista de conversas (é o que a UI abre **sem rede**, depois).
    pub async fn refresh_conversations(&mut self) -> Result<Round, DriverError> {
        let now = self.clock.now();
        let mut round = Round::default();
        // `top` limita mensagens, não a lista retornada pelo provider (sem paginação aqui).
        match self.client.list_conversations(usize::MAX).await {
            Ok(listing) => {
                let now_ms = self.now_ms();
                if let Some(name) = listing.my_display_name.clone() {
                    self.store.set_my_display_name(&name, now_ms)?;
                    self.my_display_name = Some(name);
                }
                for conversation in &listing.conversations {
                    let mut stored = stored_conversation(conversation, now_ms);
                    // Não rebaixar: um nome aprendido do histórico sobrevive ao `Atualizar`.
                    if stored.title.starts_with(ONE_ON_ONE_TITLE_PREFIX) {
                        if let Ok(Some(existing)) = self.store.conversation(&stored.id) {
                            if !existing.title.starts_with(ONE_ON_ONE_TITLE_PREFIX) {
                                stored.title = existing.title;
                            }
                        }
                    }
                    self.store.upsert_conversation(&stored)?;
                    self.machine
                        .register_conversation(ConversationId(conversation.id.clone()));
                }
                // Completa o que o cache já permite saber, sem esperar alguém abrir a conversa.
                self.repair_titles()?;
                // Para os que ainda estão sem nome, busca o histórico agora: o `Atualizar` já
                // devolve a lista com nome, não só na próxima vez.
                let filled = self.fill_missing_names().await;
                round.effects.extend(filled);
                self.capabilities
                    .observe_ok(now, Capability::ListConversations);
                round
                    .effects
                    .push(Effect::ConversationsRefreshed(listing.conversations.len()));
            }
            Err(error) => {
                self.on_failure(now, Capability::ListConversations, &error);
                return Err(DriverError::Provider(error));
            }
        }
        self.capabilities.project(now, &self.machine);
        round.capabilities = self.capabilities.summary();
        Ok(round)
    }

    /// Alimenta o motor e executa as decisões até estabilizar.
    pub async fn drive(&mut self, event: Event) -> Result<Round, DriverError> {
        let mut round = Round::default();
        let mut queue = vec![(event, 0usize)];
        let uncertain_before: Vec<ClientMessageId> = self
            .uncertain_sends()
            .into_iter()
            .map(|(_, client_id)| client_id)
            .collect();

        while let Some((event, depth)) = queue.pop() {
            if depth > MAX_STEPS {
                return Err(DriverError::NotConverged);
            }
            let decisions = self.machine.handle(self.clock.now(), event);
            for decision in decisions {
                match decision {
                    Decision::Persist { conversation } => {
                        self.persist_conversation(&conversation)?;
                        round.effects.push(Effect::Persisted(conversation));
                    }
                    Decision::Publish { conversation } => {
                        self.notifier.published(&conversation);
                        round.effects.push(Effect::Published(conversation));
                    }
                    Decision::ScheduleWake { at } => {
                        round.wake_at = Some(match round.wake_at {
                            Some(current) if current <= at => current,
                            _ => at,
                        });
                    }
                    Decision::FetchHistory { op, conversation } => {
                        let (outcome, observed) = self.fetch(&conversation, &mut round).await?;
                        // A fila é LIFO: empilha a operação primeiro para as observações serem
                        // processadas antes — o fato observado vale mais que o resultado do request.
                        queue.push((Event::OperationFinished { op, outcome }, depth + 1));
                        for client_id in observed {
                            queue.push((
                                Event::MessageObserved {
                                    conversation: conversation.clone(),
                                    client_id,
                                },
                                depth + 1,
                            ));
                        }
                    }
                    Decision::AttemptSend {
                        op,
                        conversation,
                        client_id,
                    } => {
                        let outcome = self.send(&conversation, &client_id, &mut round).await;
                        queue.push((Event::OperationFinished { op, outcome }, depth + 1));
                    }
                    Decision::Reconnect { op } => {
                        let outcome = self.reconnect(&mut round).await;
                        queue.push((Event::OperationFinished { op, outcome }, depth + 1));
                    }
                }
            }
        }

        // Transições para `Uncertain` aconteceram nesta rodada: a UI precisa saber explicitamente.
        for (conversation, client_id) in self.uncertain_sends() {
            if !uncertain_before.contains(&client_id) {
                round.effects.push(Effect::Uncertain {
                    conversation,
                    client_id,
                });
            }
        }

        self.maybe_refresh(&mut round).await;

        let now = self.clock.now();
        self.capabilities.project(now, &self.machine);
        round.capabilities = self.capabilities.summary();
        Ok(round)
    }

    // ---- execução das decisões ----

    async fn fetch(
        &mut self,
        conversation: &ConversationId,
        round: &mut Round,
    ) -> Result<(Outcome, Vec<ClientMessageId>), DriverError> {
        let now = self.clock.now();
        match self.client.history(&conversation.0, self.top).await {
            Ok(messages) => {
                let now_ms = self.now_ms();
                self.store.ensure_conversation(&conversation.0, now_ms)?;
                let batch: Vec<StoredMessage> = messages
                    .iter()
                    .map(|message| stored_message(conversation, message))
                    .collect();
                // Só interessa o `clientmessageid` de envio que este cliente ainda está devendo;
                // o motor confirma apenas o da própria conversa (`SEND-RECONCILE-001`).
                let observed: Vec<ClientMessageId> = batch
                    .iter()
                    .filter_map(|message| message.client_message_id.clone())
                    .map(ClientMessageId)
                    .filter(|client_id| self.machine.send_state(client_id).is_some())
                    .collect();
                // Atômico e idempotente: repetir uma busca interrompida não inventa mensagem.
                self.store.upsert_messages(&batch)?;
                self.run_post_receive(conversation, &batch, round).await;
                self.refine_one_on_one_title(conversation, &batch, round);
                self.capabilities.observe_ok(now, Capability::ReadHistory);
                round.effects.push(Effect::Fetched {
                    conversation: conversation.clone(),
                    messages: batch.len(),
                });
                Ok((Outcome::Succeeded, observed))
            }
            Err(error) => {
                self.on_failure(now, Capability::ReadHistory, &error);
                Ok((OpFailure::classify(&error).outcome(), Vec::new()))
            }
        }
    }

    /// Roda o `post_receive` nas mensagens do lote que ainda não têm derivado **válido** deste
    /// modelo. Falha é isolada por mensagem: vira [`Effect::MiddlewareFailed`], nunca derruba a
    /// busca nem deixa de gravar as outras.
    async fn run_post_receive(
        &mut self,
        conversation: &ConversationId,
        batch: &[StoredMessage],
        round: &mut Round,
    ) {
        let Some(middleware) = self.middleware.clone() else {
            return;
        };
        let now_ms = self.now_ms();
        for message in batch {
            let Ok(Some(revision)) = self.store.message_revision(&message.id) else {
                continue;
            };
            let already = self
                .store
                .derivatives_for(&message.id)
                .map(|list| {
                    list.iter()
                        .any(|d| !d.stale && d.model == middleware.model())
                })
                .unwrap_or(false);
            if already {
                continue;
            }
            let incoming = Incoming {
                conversation: conversation.clone(),
                message_id: message.id.clone(),
                sender: message.sender.clone(),
                body: message.body.clone(),
                message_type: message.message_type.clone(),
                revision,
            };
            let derived = match middleware.post_receive(&incoming).await {
                Ok(derived) => derived,
                Err(_) => {
                    round.effects.push(Effect::MiddlewareFailed {
                        message_id: message.id.clone(),
                    });
                    continue;
                }
            };
            for item in derived {
                match self.store.add_derivative(
                    &item.kind,
                    middleware.model(),
                    item.lang.as_deref(),
                    &item.body,
                    &[message.id.as_str()],
                    now_ms,
                ) {
                    Ok(derivative_id) => round.effects.push(Effect::Derived {
                        message_id: message.id.clone(),
                        kind: item.kind,
                        derivative_id,
                    }),
                    Err(_) => round.effects.push(Effect::MiddlewareFailed {
                        message_id: message.id.clone(),
                    }),
                }
            }
        }
    }

    /// Completa títulos provisórios de 1:1 usando **o que já está no cache**.
    ///
    /// Roda na carga (a casca chama antes do primeiro frame), depois de cada `refresh` e depois de
    /// cada busca. Sem isso o nome só aparecia na conversa recém-aberta — e o `Atualizar` apagava o
    /// que tinha sido aprendido.
    pub fn repair_titles(&self) -> Result<usize, DriverError> {
        let Some(mine) = self.store.my_display_name()? else {
            return Ok(0);
        };
        let mut repaired = 0;
        for mut conversation in self.store.list_conversations(TITLE_REPAIR_LIMIT)? {
            if !conversation.title.starts_with(ONE_ON_ONE_TITLE_PREFIX) {
                continue;
            }
            let senders = self.store.senders(&conversation.id)?;
            let Some(name) = counterpart(senders.iter().map(|(sender, _)| sender.as_str()), &mine)
            else {
                continue;
            };
            conversation.title = name;
            self.store.upsert_conversation(&conversation)?;
            repaired += 1;
        }
        Ok(repaired)
    }

    /// Busca o histórico dos 1:1 que **ainda** estão sem nome, para o `Atualizar` já trazer tudo.
    ///
    /// Limitado de propósito: é uma requisição por conversa pendente e a lista tende a ser curta.
    /// Falha de uma conversa não derruba o refresh.
    async fn fill_missing_names(&mut self) -> Vec<Effect> {
        let Some(mine) = self.store.my_display_name().ok().flatten() else {
            return Vec::new();
        };
        let Ok(conversations) = self.store.list_conversations(TITLE_REPAIR_LIMIT) else {
            return Vec::new();
        };
        let pending: Vec<String> = conversations
            .into_iter()
            .filter(|conversation| conversation.title.starts_with(ONE_ON_ONE_TITLE_PREFIX))
            .map(|conversation| conversation.id)
            .take(NAMES_FETCH_LIMIT)
            .collect();

        let now = self.clock.now();
        let mut effects = Vec::new();
        // Mesma janela da conversa aberta (`top`): o resultado não pode depender de quem clicou
        // primeiro. Se as últimas N mensagens forem todas minhas, o nome sai na próxima vez que a
        // janela alcançar a outra pessoa.
        let top = self.top.max(NAMES_FETCH_TOP);
        for id in pending {
            let Ok(messages) = self.client.history(&id, top).await else {
                continue;
            };
            let Some(name) = counterpart(
                messages
                    .iter()
                    .filter_map(|message| message.sender.as_deref()),
                &mine,
            ) else {
                continue;
            };
            let Ok(Some(mut stored)) = self.store.conversation(&id) else {
                continue;
            };
            stored.title = name;
            if self.store.upsert_conversation(&stored).is_ok() {
                effects.push(Effect::Persisted(ConversationId(id)));
            }
        }
        if !effects.is_empty() {
            self.capabilities.observe_ok(now, Capability::ReadHistory);
        }
        effects
    }

    /// Completa o título de um 1:1 sem nome usando o histórico.
    ///
    /// A listagem não traz o nome do outro lado (`members[]` só tem `mri`/`objectId`), mas revela
    /// **quem sou eu** quando alguma conversa mostra a última mensagem minha. Cruzando isso com os
    /// remetentes do histórico, o outro remetente é a outra pessoa — sem endpoint novo.
    fn refine_one_on_one_title(
        &self,
        conversation: &ConversationId,
        batch: &[StoredMessage],
        round: &mut Round,
    ) {
        // O nome pode ter vindo da listagem nesta execução ou de uma execução anterior (cache).
        let mine = match self.my_display_name.clone() {
            Some(name) => name,
            None => match self.store.my_display_name() {
                Ok(Some(name)) => name,
                _ => return,
            },
        };
        let Ok(Some(mut stored)) = self.store.conversation(&conversation.0) else {
            return;
        };
        // Só completa o que ficou no sufixo; título real não é sobrescrito.
        if !stored.title.starts_with(ONE_ON_ONE_TITLE_PREFIX) {
            return;
        }
        let Some(name) = counterpart(
            batch.iter().filter_map(|message| message.sender.as_deref()),
            &mine,
        ) else {
            return;
        };
        stored.title = name;
        if self.store.upsert_conversation(&stored).is_ok() {
            round.effects.push(Effect::Persisted(conversation.clone()));
        }
    }

    async fn send(
        &mut self,
        conversation: &ConversationId,
        client_id: &ClientMessageId,
        round: &mut Round,
    ) -> Outcome {
        let Some(text) = self.pending_text.get(client_id).cloned() else {
            // Sem o texto não há envio possível (a UI perdeu o estado): falha definitiva.
            return Outcome::Failed { retryable: false };
        };
        let now = self.clock.now();
        round.effects.push(Effect::SendAttempted {
            conversation: conversation.clone(),
            client_id: client_id.clone(),
        });
        match self
            .client
            .send_text(&conversation.0, &text, &client_id.0)
            .await
        {
            Ok(_receipt) => {
                self.capabilities.observe_ok(now, Capability::Send);
                round.effects.push(Effect::Sent {
                    conversation: conversation.clone(),
                    client_id: client_id.clone(),
                });
                Outcome::Succeeded
            }
            Err(error) => {
                self.on_failure(now, Capability::Send, &error);
                // HTTP throttling and server failures do not prove that a non-idempotent
                // send was not accepted. Reconcile rather than replay the POST.
                if matches!(&error, TeamsError::Http { status, .. } if *status == 429 || *status >= 500)
                {
                    Outcome::Lost
                } else {
                    OpFailure::classify(&error).outcome()
                }
            }
        }
    }

    async fn reconnect(&mut self, round: &mut Round) -> Outcome {
        let now = self.clock.now();
        round.effects.push(Effect::ReconnectAttempt);
        match self.realtime.reconnect().await {
            Ok(()) => {
                self.capabilities.observe_ok(now, Capability::Realtime);
                Outcome::Succeeded
            }
            Err(_) => {
                self.capabilities.observe_failure(
                    now,
                    Capability::Realtime,
                    DegradedReason::Reconnecting,
                );
                Outcome::Failed { retryable: true }
            }
        }
    }

    // ---- auxiliares ----

    fn persist_conversation(&mut self, conversation: &ConversationId) -> Result<(), DriverError> {
        let now_ms = self.now_ms();
        self.store.ensure_conversation(&conversation.0, now_ms)?;
        Ok(())
    }

    /// Renova o token antes do `exp`, com backoff em erro — sem abrir WebView (quem decide é o
    /// keeper). Falha não-retentável é sessão expirada: invalida a geração, como manda o §2.5.
    async fn maybe_refresh(&mut self, round: &mut Round) {
        let Some(keeper) = self.token_keeper.clone() else {
            return;
        };
        let Some(due) = self.refresh_at else {
            return;
        };
        let now = self.clock.now();
        if now < due {
            note_earliest(&mut round.wake_at, due);
            return;
        }
        match keeper.refresh().await {
            Ok(interval) => {
                let next = now + interval;
                self.refresh_backoff = REFRESH_BACKOFF_MIN;
                self.refresh_at = Some(next);
                round.effects.push(Effect::TokenRefreshed);
                note_earliest(&mut round.wake_at, next);
            }
            Err(error) if error.retryable => {
                let wait = self.refresh_backoff;
                self.refresh_backoff = (wait * 2).min(REFRESH_BACKOFF_MAX);
                let next = now + wait;
                self.refresh_at = Some(next);
                round
                    .effects
                    .push(Effect::TokenRefreshFailed { retry_in: wait });
                note_earliest(&mut round.wake_at, next);
            }
            Err(_) => {
                // Sem renovação silenciosa, só o dono resolve.
                self.refresh_at = None;
                self.generation += 1;
                self.capabilities
                    .invalidate_session(now, SessionGeneration(self.generation));
                round.effects.push(Effect::SessionExpired);
            }
        }
    }

    fn uncertain_sends(&self) -> Vec<(ConversationId, ClientMessageId)> {
        self.machine
            .sends()
            .filter_map(|(client_id, state)| match state {
                SendState::Uncertain { conversation, .. } => {
                    Some((conversation.clone(), client_id.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn on_failure(&mut self, now: Instant, capability: Capability, error: &TeamsError) {
        match OpFailure::classify(error) {
            // Resultado desconhecido não diz nada sobre a capacidade.
            OpFailure::Lost => {}
            OpFailure::Auth => {
                self.generation += 1;
                let generation = SessionGeneration(self.generation);
                self.capabilities.invalidate_session(now, generation);
            }
            OpFailure::Retryable | OpFailure::Refused => {
                self.capabilities
                    .observe_failure(now, capability, DegradedReason::Refused);
            }
        }
    }

    /// O usuário aprova **este** derivado (§6: aprovar exatamente o texto que será enviado).
    /// Devolve o texto aprovado; recusa derivado inexistente ou obsoleto.
    pub fn approve_derivative(&self, id: i64) -> Result<String, DriverError> {
        Ok(self.store.approve_derivative(id, self.now_ms())?)
    }

    fn now_ms(&self) -> i64 {
        self.clock
            .utc_now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Guarda o prazo **mais cedo** entre os já pedidos na rodada.
fn note_earliest(slot: &mut Option<Instant>, at: Instant) {
    *slot = Some(match *slot {
        Some(current) if current <= at => current,
        _ => at,
    });
}

fn stored_conversation(conversation: &TeamsConversation, now_ms: i64) -> StoredConversation {
    let kind = match &conversation.kind {
        ConversationKind::Chat { .. } => "chat",
        ConversationKind::Channel { .. } => "channel",
    };
    StoredConversation {
        id: conversation.id.clone(),
        title: conversation.title.clone(),
        kind: kind.to_string(),
        last_activity_ms: None,
        updated_ms: now_ms,
    }
}

fn stored_message(conversation: &ConversationId, message: &HistoryMessage) -> StoredMessage {
    StoredMessage {
        id: message.id.clone(),
        conversation_id: conversation.0.clone(),
        // Só o que o provider afirmar: `None` quando o campo veio ausente ou num formato que ele
        // não suporta (`HISTORY-SHAPE-001`, UNVERIFIED). O driver não deduz nada daqui.
        client_message_id: message.client_message_id.clone(),
        sender: message.sender.clone(),
        body: message.content.clone().unwrap_or_default(),
        message_type: message
            .message_type
            .clone()
            .unwrap_or_else(|| "Text".to_string()),
        created_ms: message
            .time
            .as_deref()
            .and_then(epoch_ms_from_rfc3339)
            .unwrap_or(0),
    }
}

/// `RFC 3339` em UTC (`...Z`, fração opcional) → época em ms. `None` se não for desse formato.
fn epoch_ms_from_rfc3339(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() < 20 {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis = match text.get(19..) {
        Some(rest) if rest.starts_with('.') => {
            let digits: String = rest[1..].chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                return None;
            }
            let mut padded = digits.clone();
            while padded.len() < 3 {
                padded.push('0');
            }
            padded.get(0..3)?.parse().ok()?
        }
        _ => 0,
    };
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis)
}

/// Dias desde 1970-01-01 (algoritmo de Howard Hinnant), aritmética inteira pura.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use f365_auth::transport::HttpResponse;
    use f365_auth::{AuthOp, ManualClock};
    use f365_storage::Store;
    use f365_sync::SendState;
    use std::sync::Mutex;
    use std::time::Duration;

    // ---- fakes ----

    #[derive(Default)]
    struct FakeExec {
        script: Mutex<std::collections::VecDeque<Result<HttpResponse, ExecError>>>,
        ops: Mutex<Vec<AuthOp>>,
    }

    impl FakeExec {
        fn replying(script: Vec<Result<HttpResponse, ExecError>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into_iter().collect()),
                ops: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.ops.lock().expect("ops").len()
        }

        fn methods(&self) -> Vec<String> {
            self.ops
                .lock()
                .expect("ops")
                .iter()
                .map(|op| op.request.method.clone())
                .collect()
        }

        fn urls(&self) -> Vec<String> {
            self.ops
                .lock()
                .expect("ops")
                .iter()
                .map(|op| op.request.url.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl AuthenticatedExecutor for FakeExec {
        async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
            self.ops.lock().expect("ops").push(op);
            self.script
                .lock()
                .expect("script")
                .pop_front()
                .unwrap_or(Err(ExecError::Transport("script vazio".into())))
        }
    }

    struct FakeRealtime {
        result: Result<(), String>,
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl Realtime for FakeRealtime {
        async fn reconnect(&self) -> Result<(), String> {
            *self.calls.lock().expect("calls") += 1;
            self.result.clone()
        }
    }

    #[derive(Default)]
    struct RecordingNotifier {
        published: Mutex<Vec<ConversationId>>,
    }

    struct FakeKeeper {
        reply: Mutex<std::collections::VecDeque<Result<Duration, RefreshError>>>,
        calls: Mutex<usize>,
    }

    impl FakeKeeper {
        fn new(reply: Vec<Result<Duration, RefreshError>>) -> Arc<Self> {
            Arc::new(Self {
                reply: Mutex::new(reply.into_iter().collect()),
                calls: Mutex::new(0),
            })
        }

        fn calls(&self) -> usize {
            *self.calls.lock().expect("calls")
        }
    }

    #[async_trait::async_trait]
    impl TokenKeeper for FakeKeeper {
        async fn refresh(&self) -> std::result::Result<Duration, RefreshError> {
            *self.calls.lock().expect("calls") += 1;
            self.reply
                .lock()
                .expect("reply")
                .pop_front()
                .unwrap_or(Ok(Duration::from_secs(1_800)))
        }
    }

    impl Notifier for RecordingNotifier {
        fn published(&self, conversation: &ConversationId) {
            self.published
                .lock()
                .expect("published")
                .push(conversation.clone());
        }
    }

    fn ok(status: u16, body: &str) -> Result<HttpResponse, ExecError> {
        Ok(HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        })
    }

    fn lost() -> Result<HttpResponse, ExecError> {
        Err(ExecError::Transport("conexão caiu".into()))
    }

    fn conv() -> ConversationId {
        ConversationId("19:abc@thread.v2".into())
    }

    fn client_id(round: &Round) -> ClientMessageId {
        round
            .effects
            .iter()
            .find_map(|effect| match effect {
                Effect::SendAttempted { client_id, .. } => Some(client_id.clone()),
                _ => None,
            })
            .expect("uma tentativa de envio")
    }

    fn driver(exec: Arc<FakeExec>) -> (ManualClock, Driver) {
        let clock = ManualClock::default();
        let store = Store::open_in_memory().expect("banco em memória");
        let driver = Driver::new(exec, store, Arc::new(clock.clone()));
        (clock, driver)
    }

    // ---- testes ----

    #[tokio::test]
    async fn throttled_send_never_reposts_without_evidence() {
        let exec = FakeExec::replying(vec![
            ok(429, "throttled"),
            ok(429, "throttled"),
            ok(429, "throttled"),
        ]);
        let (_clock, mut driver) = driver(exec.clone());
        driver.send_message(&conv(), "hello").await.unwrap();
        assert_eq!(exec.methods().iter().filter(|m| *m == "POST").count(), 1);
    }

    #[tokio::test]
    async fn socket_loss_wakes_reconnect_without_blocking_send() {
        let exec = FakeExec::replying(vec![ok(201, "{}")]);
        let (clock, mut driver) = driver(exec.clone());
        let realtime = Arc::new(FakeRealtime {
            result: Err("offline".into()),
            calls: Mutex::new(0),
        });
        driver.realtime = realtime.clone();
        let round = driver.drive(Event::RealtimeLost).await.unwrap();
        assert!(round.wake_at.is_some());
        let sent = driver.send_message(&conv(), "hello").await.unwrap();
        assert!(sent
            .effects
            .iter()
            .any(|e| matches!(e, Effect::SendAttempted { .. })));
        clock.advance(Duration::from_secs(2));
        driver.drive(Event::WakeUp).await.unwrap();
        assert_eq!(*realtime.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn history_limit_does_not_truncate_conversations() {
        let exec = FakeExec::replying(vec![ok(
            200,
            r#"{"chats":[{"id":"chat-a"},{"id":"chat-b"}]}"#,
        )]);
        let (_, driver) = driver(exec);
        let mut driver = driver.with_top(1);
        let round = driver.refresh_conversations().await.unwrap();
        assert!(round.effects.contains(&Effect::ConversationsRefreshed(2)));
    }

    #[tokio::test]
    async fn replay_of_more_than_64_conversations_converges() {
        let exec = FakeExec::replying((0..70).map(|_| ok(200, r#"{"messages":[]}"#)).collect());
        let (_, mut driver) = driver(exec.clone());
        driver.realtime = Arc::new(FakeRealtime {
            result: Ok(()),
            calls: Mutex::new(0),
        });
        for i in 0..70 {
            driver
                .machine
                .register_conversation(ConversationId(format!("chat-{i}")));
        }
        let round = driver
            .drive(Event::ConnectionRestored)
            .await
            .expect("replay must converge");
        assert_eq!(
            round
                .effects
                .iter()
                .filter(|e| matches!(e, Effect::Fetched { .. }))
                .count(),
            70
        );
        assert_eq!(exec.calls(), 70);
    }

    #[tokio::test]
    async fn throttled_history_is_retried_not_fresh() {
        let exec = FakeExec::replying(vec![ok(429, "throttled"), ok(200, r#"{"messages":[]}"#)]);
        let (clock, mut driver) = driver(exec.clone());
        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .unwrap();
        assert!(round.wake_at.is_some());
        assert_eq!(
            driver.machine().conversation_state(&conv()),
            Some(&f365_sync::ConversationState::Dirty)
        );
        clock.advance(Duration::from_secs(5));
        driver.drive(Event::WakeUp).await.unwrap();
        assert_eq!(exec.calls(), 2);
        assert_eq!(
            driver.machine().conversation_state(&conv()),
            Some(&f365_sync::ConversationState::Fresh)
        );
    }

    #[test]
    fn rfc3339_is_converted_to_epoch_millis() {
        assert_eq!(epoch_ms_from_rfc3339("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            epoch_ms_from_rfc3339("1970-01-02T00:00:00Z"),
            Some(86_400_000)
        );
        assert_eq!(
            epoch_ms_from_rfc3339("2000-01-01T00:00:00Z"),
            Some(946_684_800_000)
        );
        assert_eq!(
            epoch_ms_from_rfc3339("2024-01-01T00:00:00Z"),
            Some(1_704_067_200_000)
        );
        assert_eq!(
            epoch_ms_from_rfc3339("2024-01-01T00:00:01.250Z"),
            Some(1_704_067_201_250)
        );
        assert_eq!(epoch_ms_from_rfc3339("ontem"), None);
        assert_eq!(epoch_ms_from_rfc3339("2024-13-01T00:00:00Z"), None);
    }

    #[tokio::test]
    async fn startup_conversations_are_known_without_fetching_history() {
        let exec = FakeExec::replying(vec![ok(200, r#"{"chats":[{"id":"19:abc@thread.v2"}]}"#)]);
        let (_clock, mut driver) = driver(exec.clone());
        driver.refresh_conversations().await.expect("lista");
        assert!(driver.machine().conversation_state(&conv()).is_some());
        assert_eq!(exec.calls(), 1, "registrar não busca histórico");
    }

    #[tokio::test]
    async fn duplicate_list_entries_do_not_reset_a_fresh_conversation() {
        let exec = FakeExec::replying(vec![
            ok(
                200,
                r#"{"chats":[{"id":"19:abc@thread.v2"},{"id":"19:abc@thread.v2"},{"id":""}]}"#,
            ),
            ok(200, r#"{"messages":[]}"#),
            ok(200, r#"{"chats":[{"id":"19:abc@thread.v2"}]}"#),
        ]);
        let (_, driver) = driver(exec.clone());
        let mut driver = driver.with_realtime(Arc::new(FakeRealtime {
            result: Ok(()),
            calls: Mutex::new(0),
        }));
        driver.refresh_conversations().await.unwrap();
        assert!(driver
            .machine()
            .conversation_state(&ConversationId(String::new()))
            .is_none());
        driver.drive(Event::ConnectionRestored).await.unwrap();
        assert!(matches!(
            driver.machine().conversation_state(&conv()),
            Some(f365_sync::ConversationState::Fresh)
        ));
        driver.refresh_conversations().await.unwrap();
        assert!(matches!(
            driver.machine().conversation_state(&conv()),
            Some(f365_sync::ConversationState::Fresh)
        ));
        assert_eq!(exec.calls(), 3);
    }

    #[tokio::test]
    async fn startup_then_reconnect_replays_gap_and_retries_rest() {
        let exec = FakeExec::replying(vec![
            ok(200, r#"{"chats":[{"id":"19:abc@thread.v2"}]}"#),
            ok(200, r#"{"messages":[]}"#),
            ok(503, ""),
            ok(200, r#"{"messages":[{"id":"m1","content":"gap"}]}"#),
        ]);
        let (clock, driver) = driver(exec.clone());
        let realtime = Arc::new(FakeRealtime {
            result: Ok(()),
            calls: Mutex::new(0),
        });
        let mut driver = driver.with_realtime(realtime);
        driver.refresh_conversations().await.expect("lista");
        driver
            .drive(Event::ConnectionRestored)
            .await
            .expect("ready");
        assert!(driver
            .store()
            .page_messages(&conv().0, None, 10)
            .unwrap()
            .is_empty());
        driver.drive(Event::ConnectionLost).await.expect("queda");
        let retry = driver
            .drive(Event::ConnectionRestored)
            .await
            .expect("volta");
        assert!(retry.wake_at.is_some());
        assert!(driver
            .store()
            .page_messages(&conv().0, None, 10)
            .unwrap()
            .is_empty());
        clock.advance(Duration::from_secs(60));
        driver.drive(Event::WakeUp).await.expect("retry");
        let messages = driver.store().page_messages(&conv().0, None, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "m1");
        assert_eq!(exec.calls(), 4);
    }

    #[tokio::test]
    async fn refreshing_conversations_fills_the_cache() {
        let exec = FakeExec::replying(vec![ok(
            200,
            r#"{"chats":[{"id":"19:a@thread.v2","title":"Time"},{"id":"19:b@thread.v2","title":"Outro"}]}"#,
        )]);
        let (_clock, mut driver) = driver(exec);
        driver.store().ensure_conversation("19:a@thread.v2", 0).ok();

        let round = driver.refresh_conversations().await.expect("atualiza");

        assert_eq!(round.effects, vec![Effect::ConversationsRefreshed(2)]);
        let cached = driver.store().list_conversations(10).expect("lista");
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].title, "Time");
        assert!(
            round
                .capabilities
                .iter()
                .any(|line| line == "listar conversas: ok"),
            "capability acompanha: {:?}",
            round.capabilities
        );
    }

    #[tokio::test]
    async fn send_success_persists_and_publishes() {
        let exec = FakeExec::replying(vec![ok(201, r#"{"OriginalArrivalTime":42}"#)]);
        let (_clock, driver) = driver(exec);
        let notifier = Arc::new(RecordingNotifier::default());
        let mut driver = driver.with_notifier(notifier.clone());

        let round = driver.send_message(&conv(), "oi").await.expect("envia");
        let sent = client_id(&round);

        assert!(round.effects.contains(&Effect::Sent {
            conversation: conv(),
            client_id: sent.clone()
        }));
        assert!(round.effects.contains(&Effect::Published(conv())));
        assert_eq!(
            notifier.published.lock().expect("published").as_slice(),
            &[conv()][..],
            "o aviso chegou ao sink, não só ao relatório"
        );
        assert!(matches!(
            driver.machine().send_state(&sent),
            Some(SendState::Delivered { .. })
        ));
        assert_eq!(
            driver.store().list_conversations(5).expect("cache").len(),
            1,
            "o driver persiste a conversa ao publicar"
        );
    }

    #[tokio::test]
    async fn lost_send_is_never_retried_and_ends_uncertain_without_evidence() {
        let exec = FakeExec::replying(vec![
            lost(),                                                  // POST: resposta perdida
            ok(200, r#"{"messages":[{"id":"m1","content":"oi"}]}"#), // GET: busca de reconciliação
        ]);
        let (clock, mut driver) = driver(exec.clone());

        let round = driver.send_message(&conv(), "oi").await.expect("tenta");
        let sent = client_id(&round);
        assert_eq!(
            exec.methods().iter().filter(|m| *m == "POST").count(),
            1,
            "nunca reenvia depois de perder a resposta"
        );
        assert!(matches!(
            driver.machine().send_state(&sent),
            Some(SendState::Reconciling { .. })
        ));

        // Sem prova no conteúdo, o prazo vence e o estado é incerto — nunca "entregue".
        clock.advance(Duration::from_secs(61));
        let round = driver.drive(Event::WakeUp).await.expect("acorda");
        assert!(matches!(
            driver.machine().send_state(&sent),
            Some(SendState::Uncertain { .. })
        ));
        assert_eq!(
            exec.methods().iter().filter(|m| *m == "POST").count(),
            1,
            "nem ao desistir ele reenvia"
        );
        assert!(round.effects.contains(&Effect::Uncertain {
            conversation: conv(),
            client_id: sent
        }));
    }

    #[tokio::test]
    async fn observed_message_closes_the_reconciliation() {
        let exec = FakeExec::replying(vec![
            lost(),
            ok(200, r#"{"messages":[{"id":"m1","content":"oi"}]}"#),
        ]);
        let (_clock, mut driver) = driver(exec);
        let round = driver.send_message(&conv(), "oi").await.expect("tenta");
        let sent = client_id(&round);

        // O provider (ou um teste) provou que a mensagem existe: é o único caminho de volta.
        let round = driver.note_observed(&conv(), &sent).await.expect("observa");

        assert!(matches!(
            driver.machine().send_state(&sent),
            Some(SendState::Delivered { .. })
        ));
        assert!(round.effects.contains(&Effect::Published(conv())));
    }

    #[tokio::test]
    async fn history_reveals_the_client_id_and_closes_the_reconciliation() {
        // A busca da reconciliação ainda não vê a mensagem: quem a mostra é uma busca posterior.
        let exec = FakeExec::replying(vec![lost(), ok(200, r#"{"messages":[]}"#)]);
        let (_clock, mut driver) = driver(exec.clone());

        let round = driver.send_message(&conv(), "oi").await.expect("tenta");
        let sent = client_id(&round);
        assert!(matches!(
            driver.machine().send_state(&sent),
            Some(SendState::Reconciling { .. })
        ));

        // Agora o conteúdo traz o `clientmessageid` que o cliente gerou.
        exec.script.lock().expect("script").push_back(ok(
            200,
            &format!(
                r#"{{"messages":[{{"id":"m1","content":"oi","clientmessageid":"{}"}}]}}"#,
                sent.0
            ),
        ));

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("busca de novo");

        assert!(
            matches!(
                driver.machine().send_state(&sent),
                Some(SendState::Delivered { .. })
            ),
            "o fato observado confirma a entrega"
        );
        assert_eq!(
            exec.methods().iter().filter(|m| *m == "POST").count(),
            1,
            "reconciliou sem reenviar"
        );
        assert!(round.effects.contains(&Effect::Published(conv())));

        // O id também fica no cache: a UI/diagnóstico acham a mensagem depois.
        let stored = driver
            .store()
            .find_by_client_message_id(&sent.0)
            .expect("busca")
            .expect("mensagem no cache");
        assert_eq!(stored.id, "m1");
    }

    /// Aprender o nome não pode ser desfeito pelo `Atualizar` (que traz o título provisório).
    #[tokio::test]
    async fn refresh_does_not_downgrade_a_learned_name() {
        const LISTING: &str = r#"{"chats":[{"id":"19:abc@thread.v2","title":null,"isOneOnOne":true,
            "isLastMessageFromMe":true,"lastMessage":{"imDisplayName":"Fabio Motta"}}]}"#;
        const HISTORY: &str = r#"{"messages":[
            {"id":"m1","content":"oi","composetime":"2024-01-01T00:00:00Z","imdisplayname":"Ana"}
        ]}"#;
        let exec = FakeExec::replying(vec![ok(200, LISTING), ok(200, HISTORY), ok(200, LISTING)]);
        let (_clock, mut driver) = driver(exec);

        driver.refresh_conversations().await.expect("listagem");
        assert_eq!(title(&driver), "Ana", "o refresh já traz o nome");

        // `Atualizar` de novo: o provider volta a mandar `1:1 · …`.
        driver.refresh_conversations().await.expect("listagem");
        assert_eq!(
            title(&driver),
            "Ana",
            "o nome aprendido sobrevive ao refresh"
        );
    }

    /// Na carga, o nome sai do cache — sem abrir a conversa e sem rede.
    #[tokio::test]
    async fn repair_titles_completes_from_the_cache_alone() {
        const LISTING: &str = r#"{"chats":[{"id":"19:abc@thread.v2","title":null,"isOneOnOne":true,
            "isLastMessageFromMe":true,"lastMessage":{"imDisplayName":"Fabio Motta"}}]}"#;
        // O refresh não consegue o nome (histórico vazio); a busca depois traz as mensagens.
        const EMPTY: &str = r#"{"messages":[]}"#;
        const HISTORY: &str = r#"{"messages":[
            {"id":"m1","content":"oi","composetime":"2024-01-01T00:00:00Z","imdisplayname":"Ana"}
        ]}"#;
        let exec = FakeExec::replying(vec![ok(200, LISTING), ok(200, EMPTY), ok(200, HISTORY)]);
        let (_clock, mut driver) = driver(exec);
        driver.refresh_conversations().await.expect("listagem");
        assert!(
            title(&driver).starts_with("1:1 · "),
            "sem histórico, sem nome"
        );
        driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("busca");

        // Simula o cache de uma sessão anterior, quando o título ainda era provisório.
        let mut stale = driver
            .store()
            .conversation("19:abc@thread.v2")
            .expect("busca")
            .expect("no cache");
        stale.title = "1:1 · abc123".into();
        driver.store().upsert_conversation(&stale).expect("grava");

        assert_eq!(driver.repair_titles().expect("repara"), 1);
        assert_eq!(title(&driver), "Ana");
    }

    /// O `Atualizar` já traz o nome: busca o histórico dos 1:1 ainda sem nome, sem ninguém abrir.
    #[tokio::test]
    async fn refresh_fetches_history_of_an_unnamed_one_on_one() {
        const LISTING: &str = r#"{"chats":[{"id":"19:abc@thread.v2","title":null,"isOneOnOne":true,
            "isLastMessageFromMe":true,"lastMessage":{"imDisplayName":"Fabio Motta"}}]}"#;
        const HISTORY: &str = r#"{"messages":[
            {"id":"m1","content":"oi","composetime":"2024-01-01T00:00:00Z","imdisplayname":"Ana"}
        ]}"#;
        let exec = FakeExec::replying(vec![ok(200, LISTING), ok(200, HISTORY)]);
        let (_clock, mut driver) = driver(exec);

        driver.refresh_conversations().await.expect("refresh");

        assert_eq!(
            title(&driver),
            "Ana",
            "o Atualizar completa o nome sem abrir a conversa"
        );
    }

    /// A busca de nome lê a **mesma** janela da conversa aberta (`top`).
    ///
    /// Com uma janela menor, um 1:1 cujas últimas mensagens são todas minhas ficaria sem nome no
    /// `Atualizar` e só se resolveria ao clicar.
    #[tokio::test]
    async fn name_fetch_uses_the_same_window_as_the_open_chat() {
        const LISTING: &str = r#"{"chats":[{"id":"19:abc@thread.v2","title":null,"isOneOnOne":true,
            "isLastMessageFromMe":true,"lastMessage":{"imDisplayName":"Fabio Motta"}}]}"#;
        let exec = FakeExec::replying(vec![ok(200, LISTING), ok(200, r#"{"messages":[]}"#)]);
        let clock = ManualClock::default();
        let store = Store::open_in_memory().expect("banco em memória");
        let mut driver = Driver::new(exec.clone(), store, Arc::new(clock)).with_top(50);

        driver.refresh_conversations().await.expect("refresh");

        let urls = exec.urls();
        assert!(
            urls.iter().any(|url| url.contains("pageSize=50")),
            "esperava a busca de nome com pageSize=50, veio {urls:?}"
        );
    }

    fn title(driver: &Driver) -> String {
        driver
            .store()
            .conversation("19:abc@thread.v2")
            .expect("busca")
            .expect("no cache")
            .title
    }

    /// 1:1 sem nome na listagem ganha o nome do outro lado depois de buscar o histórico.
    #[tokio::test]
    async fn one_on_one_title_is_completed_from_history() {
        let exec = FakeExec::replying(vec![
            ok(
                200,
                r#"{"chats":[{"id":"19:abc@thread.v2","title":null,"isOneOnOne":true,
                    "isLastMessageFromMe":true,"lastMessage":{"imDisplayName":"Fabio Motta"}}]}"#,
            ),
            // O refresh não resolve (histórico vazio no primeiro momento).
            ok(200, r#"{"messages":[]}"#),
            ok(
                200,
                r#"{"messages":[
                    {"id":"m1","content":"oi","composetime":"2024-01-01T00:00:00Z","imdisplayname":"Ana"},
                    {"id":"m2","content":"oi de novo","composetime":"2024-01-01T00:00:01Z","imdisplayname":"Ana"},
                    {"id":"m3","content":"meu","composetime":"2024-01-01T00:00:02Z","imdisplayname":"Fabio Motta"}
                ]}"#,
            ),
        ]);
        let (_clock, mut driver) = driver(exec);

        driver.refresh_conversations().await.expect("listagem");
        let before = driver
            .store()
            .conversation("19:abc@thread.v2")
            .expect("busca")
            .expect("no cache");
        assert!(before.title.starts_with("1:1 · "), "{}", before.title);

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("busca");

        let after = driver
            .store()
            .conversation("19:abc@thread.v2")
            .expect("busca")
            .expect("no cache");
        assert_eq!(after.title, "Ana", "o outro remetente é a outra pessoa");
        assert!(round
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::Persisted(_))));
    }

    #[tokio::test]
    async fn fetch_persists_history_into_the_cache() {
        let exec = FakeExec::replying(vec![ok(
            200,
            r#"{"messages":[
                {"id":"m1","content":"primeira","composetime":"2024-01-01T00:00:00Z","imdisplayname":"Ana","messagetype":"Text"},
                {"id":"m2","content":"segunda","composetime":"2024-01-01T00:00:05Z"}
            ]}"#,
        )]);
        let (_clock, mut driver) = driver(exec);

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("busca");

        assert!(round.effects.contains(&Effect::Fetched {
            conversation: conv(),
            messages: 2
        }));
        let page = driver
            .store()
            .page_messages("19:abc@thread.v2", None, 10)
            .expect("histórico do cache");
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, "m2", "mais nova primeiro");
        assert_eq!(page[0].created_ms, 1_704_067_205_000);
        assert_eq!(page[1].sender.as_deref(), Some("Ana"));
    }

    #[tokio::test]
    async fn history_failure_degrades_only_the_history_capability() {
        let exec = FakeExec::replying(vec![ok(500, "{}")]);
        let (_clock, mut driver) = driver(exec);

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("busca falha, mas não explode");

        assert!(round
            .capabilities
            .iter()
            .any(|line| line == "ler histórico: servidor recusou"));
        assert!(
            round
                .capabilities
                .iter()
                .any(|line| line == "enviar: não exercitada"),
            "falha de uma capacidade não toca as outras"
        );
    }

    #[tokio::test]
    async fn session_error_invalidates_the_generation_and_all_capabilities() {
        let exec = FakeExec::replying(vec![Err(ExecError::NotPaired)]);
        let (_clock, mut driver) = driver(exec);

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("falha de sessão");

        assert!(round
            .capabilities
            .iter()
            .all(|line| line.contains("exige novo login") || line.contains("reconectando")));
        assert_eq!(driver.capabilities().generation(), SessionGeneration(2));
    }

    #[tokio::test]
    async fn realtime_failure_is_isolated_from_send() {
        let exec = FakeExec::replying(vec![
            ok(201, "{}"), // o envio, depois da queda do tempo real
        ]);
        let realtime = Arc::new(FakeRealtime {
            result: Err("socket recusado".into()),
            calls: Mutex::new(0),
        });
        let (_clock, driver) = driver(exec);
        let mut driver = driver.with_realtime(realtime.clone());

        let round = driver
            .drive(Event::ConnectionRestored)
            .await
            .expect("reconecta");
        assert!(round.effects.contains(&Effect::ReconnectAttempt));
        assert!(round
            .capabilities
            .iter()
            .any(|line| line == "tempo real: reconectando"));

        // Envio continua funcionando com o tempo real fora (§2.5).
        let round = driver
            .send_message(&conv(), "mesmo sem socket")
            .await
            .expect("envia");
        assert!(round
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::Sent { .. })));
        assert_eq!(*realtime.calls.lock().expect("calls"), 1);
    }

    #[tokio::test]
    async fn empty_text_is_refused_before_any_op() {
        let exec = FakeExec::replying(vec![]);
        let (_clock, mut driver) = driver(exec.clone());
        assert!(matches!(
            driver.send_message(&conv(), "   ").await,
            Err(DriverError::EmptyText)
        ));
        assert_eq!(exec.calls(), 0);
    }

    #[tokio::test]
    async fn lost_send_reports_a_wake_deadline() {
        let exec = FakeExec::replying(vec![lost(), ok(200, r#"{"messages":[]}"#)]);
        let (_clock, mut driver) = driver(exec);

        let round = driver.send_message(&conv(), "oi").await.expect("tenta");

        assert!(
            round.wake_at.is_some(),
            "reconciliação sem prazo nunca viraria 'incerto'"
        );
    }

    #[tokio::test]
    async fn token_is_refreshed_before_it_expires() {
        let exec = FakeExec::replying(vec![]);
        let (clock, driver) = driver(exec);
        let keeper = FakeKeeper::new(vec![Ok(Duration::from_secs(1_800))]);
        let mut driver = driver.with_token_keeper(keeper.clone());

        // Antes do prazo não renova, mas o prazo é reportado a quem chama.
        let round = driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 0);
        assert!(
            round.wake_at.is_some(),
            "quem chama precisa saber quando voltar"
        );

        clock.advance(REFRESH_FIRST);
        let round = driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 1);
        assert!(round.effects.contains(&Effect::TokenRefreshed));

        // Reagendou: não martela o login a cada rodada.
        driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 1);
    }

    #[tokio::test]
    async fn retryable_refresh_failure_backs_off_and_recovers() {
        let exec = FakeExec::replying(vec![]);
        let (clock, driver) = driver(exec);
        let keeper = FakeKeeper::new(vec![
            Err(RefreshError::retryable("rede")),
            Err(RefreshError::retryable("rede")),
            Ok(Duration::from_secs(1_800)),
        ]);
        let mut driver = driver.with_token_keeper(keeper.clone());

        clock.advance(REFRESH_FIRST);
        let round = driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 1);
        assert!(round.effects.contains(&Effect::TokenRefreshFailed {
            retry_in: REFRESH_BACKOFF_MIN
        }));

        // Antes do backoff, não tenta.
        driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 1);

        // Depois do backoff, tenta de novo — e o próximo intervalo dobrou.
        clock.advance(REFRESH_BACKOFF_MIN);
        let round = driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 2);
        assert!(round.effects.contains(&Effect::TokenRefreshFailed {
            retry_in: REFRESH_BACKOFF_MIN * 2
        }));

        // Metade do novo backoff: ainda não.
        clock.advance(REFRESH_BACKOFF_MIN);
        driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 2);

        // Completou: renova e o backoff volta ao mínimo.
        clock.advance(REFRESH_BACKOFF_MIN);
        let round = driver.drive(Event::WakeUp).await.expect("acorda");
        assert_eq!(keeper.calls(), 3);
        assert!(round.effects.contains(&Effect::TokenRefreshed));
    }

    #[tokio::test]
    async fn refresh_without_silent_renewal_needs_the_owner() {
        let exec = FakeExec::replying(vec![]);
        let (clock, driver) = driver(exec);
        let keeper = FakeKeeper::new(vec![Err(RefreshError::needs_login("RT vencido"))]);
        let mut driver = driver.with_token_keeper(keeper.clone());

        clock.advance(REFRESH_FIRST);
        let round = driver.drive(Event::WakeUp).await.expect("acorda");

        assert!(round.effects.contains(&Effect::SessionExpired));
        assert!(
            round
                .capabilities
                .iter()
                .all(|line| line.contains("exige novo login")),
            "sem renovação silenciosa, tudo depende do dono: {:?}",
            round.capabilities
        );
        assert_eq!(driver.capabilities().generation(), SessionGeneration(2));

        // Não fica tentando sozinho depois de exigir login.
        let round = driver.drive(Event::WakeUp).await.expect("acorda");
        assert!(round.effects.is_empty());
        assert_eq!(keeper.calls(), 1);
    }

    // ---- seam de middleware ----

    struct FakeTranslator {
        fail_send: bool,
        fail_receive_on: Option<&'static str>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeTranslator {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                fail_send: false,
                fail_receive_on: None,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn received(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait::async_trait]
    impl Middleware for FakeTranslator {
        fn model(&self) -> &str {
            "fake@1"
        }

        async fn pre_send(&self, outgoing: Outgoing) -> Result<Outgoing, MiddlewareError> {
            if self.fail_send {
                return Err(MiddlewareError("tradutor indisponível".into()));
            }
            Ok(Outgoing {
                text: format!("[en] {}", outgoing.text),
                ..outgoing
            })
        }

        async fn post_receive(&self, incoming: &Incoming) -> Result<Vec<Derived>, MiddlewareError> {
            self.calls
                .lock()
                .expect("calls")
                .push(incoming.message_id.clone());
            if self.fail_receive_on == Some(incoming.message_id.as_str()) {
                return Err(MiddlewareError("falhou".into()));
            }
            Ok(vec![Derived {
                kind: "translation".into(),
                lang: Some("pt".into()),
                body: format!("[pt] {}", incoming.body),
            }])
        }
    }

    const TWO_MESSAGES: &str = r#"{"messages":[
        {"id":"m1","content":"hello","composetime":"2024-01-01T00:00:00Z"},
        {"id":"m2","content":"world","composetime":"2024-01-01T00:00:05Z"}
    ]}"#;

    #[tokio::test]
    async fn pre_send_text_is_what_goes_out_and_is_not_recomputed() {
        let exec = FakeExec::replying(vec![ok(201, r#"{"OriginalArrivalTime":42}"#)]);
        let (_clock, driver) = driver(exec.clone());
        let mut driver = driver.with_middleware(FakeTranslator::new());

        let round = driver.send_message(&conv(), "oi").await.expect("envia");

        assert!(round
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::Sent { .. })));
        let body = exec.ops.lock().expect("ops")[0]
            .request
            .body
            .clone()
            .expect("corpo");
        assert!(
            String::from_utf8(body).unwrap().contains("[en] oi"),
            "saiu o texto transformado"
        );
    }

    #[tokio::test]
    async fn failing_pre_send_sends_nothing_and_registers_nothing() {
        let exec = FakeExec::replying(vec![]);
        let (_clock, driver) = driver(exec.clone());
        let middleware = Arc::new(FakeTranslator {
            fail_send: true,
            fail_receive_on: None,
            calls: Mutex::new(Vec::new()),
        });
        let mut driver = driver.with_middleware(middleware);

        let error = driver
            .send_message(&conv(), "oi")
            .await
            .expect_err("recusado");

        assert!(matches!(error, DriverError::Middleware(_)));
        assert_eq!(
            exec.calls(),
            0,
            "nada saiu em silêncio com o texto original"
        );
        assert!(driver.uncertain_sends().is_empty());
    }

    #[tokio::test]
    async fn post_receive_stores_derivatives_and_keeps_the_original() {
        let exec = FakeExec::replying(vec![ok(200, TWO_MESSAGES)]);
        let (_clock, driver) = driver(exec);
        let mut driver = driver.with_middleware(FakeTranslator::new());

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("busca");

        let derived = round
            .effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Derived { .. }))
            .count();
        assert_eq!(derived, 2);
        let page = driver
            .store()
            .page_messages("19:abc@thread.v2", None, 10)
            .expect("cache");
        assert_eq!(
            page[1].body, "hello",
            "o fato não foi trocado pela tradução"
        );
        let derivatives = driver.store().derivatives_for("m1").expect("derivados");
        assert_eq!(derivatives.len(), 1);
        assert_eq!(derivatives[0].body, "[pt] hello");
        assert_eq!(derivatives[0].model, "fake@1");
        assert!(!derivatives[0].stale);
    }

    #[tokio::test]
    async fn already_derived_messages_are_not_reprocessed() {
        let exec = FakeExec::replying(vec![ok(200, TWO_MESSAGES), ok(200, TWO_MESSAGES)]);
        let (_clock, driver) = driver(exec);
        let middleware = FakeTranslator::new();
        let mut driver = driver.with_middleware(middleware.clone());

        for _ in 0..2 {
            driver
                .drive(Event::Invalidated {
                    conversation: conv(),
                })
                .await
                .expect("busca");
        }

        assert_eq!(
            middleware.received().len(),
            2,
            "só a primeira busca traduziu"
        );
        assert_eq!(driver.store().derivatives_for("m1").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn edited_message_is_derived_again_for_the_new_revision() {
        let edited = TWO_MESSAGES.replace("hello", "hello there");
        let exec = FakeExec::replying(vec![ok(200, TWO_MESSAGES), ok(200, &edited)]);
        let (_clock, driver) = driver(exec);
        let middleware = FakeTranslator::new();
        let mut driver = driver.with_middleware(middleware.clone());

        for _ in 0..2 {
            driver
                .drive(Event::Invalidated {
                    conversation: conv(),
                })
                .await
                .expect("busca");
        }

        let list = driver.store().derivatives_for("m1").unwrap();
        assert_eq!(list.len(), 2, "o velho fica de histórico");
        assert!(list[0].body.contains("hello there") && !list[0].stale);
        assert!(list[1].stale, "o derivado da revisão antiga ficou obsoleto");
        assert_eq!(
            middleware.received(),
            vec!["m1", "m2", "m1"],
            "m2 não mudou e não foi reprocessada"
        );
    }

    #[tokio::test]
    async fn post_receive_failure_is_isolated_to_that_message() {
        let exec = FakeExec::replying(vec![ok(200, TWO_MESSAGES)]);
        let (_clock, driver) = driver(exec);
        let middleware = Arc::new(FakeTranslator {
            fail_send: false,
            fail_receive_on: Some("m1"),
            calls: Mutex::new(Vec::new()),
        });
        let mut driver = driver.with_middleware(middleware);

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("a busca não falha por causa do middleware");

        assert!(round.effects.contains(&Effect::MiddlewareFailed {
            message_id: "m1".into()
        }));
        assert!(round.effects.contains(&Effect::Fetched {
            conversation: conv(),
            messages: 2
        }));
        assert!(driver.store().derivatives_for("m1").unwrap().is_empty());
        assert_eq!(driver.store().derivatives_for("m2").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn without_middleware_nothing_changes() {
        let exec = FakeExec::replying(vec![ok(200, TWO_MESSAGES)]);
        let (_clock, mut driver) = driver(exec);

        let round = driver
            .drive(Event::Invalidated {
                conversation: conv(),
            })
            .await
            .expect("busca");

        assert!(!round
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::Derived { .. })));
        assert!(driver.store().derivatives_for("m1").unwrap().is_empty());
    }
}
