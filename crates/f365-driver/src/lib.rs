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
use std::time::{Instant, UNIX_EPOCH};

/// Quantas transições o driver aceita executar num único `drive` (guarda contra laço infinito).
const MAX_STEPS: usize = 64;

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
            TeamsError::Http { status, .. } if *status >= 500 => OpFailure::Retryable,
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

/// Quem é avisado quando uma conversa muda (UI, IPC, log).
pub trait Notifier: Send + Sync {
    fn published(&self, conversation: &ConversationId);
}

pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn published(&self, _conversation: &ConversationId) {}
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
}

impl Driver {
    pub fn new(exec: Arc<dyn AuthenticatedExecutor>, store: Store, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
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
            clock,
        }
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
        let client_id = ClientMessageId(f365_teams::new_client_message_id());
        self.pending_text
            .insert(client_id.clone(), text.to_string());
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
        match self.client.list_conversations(self.top).await {
            Ok(list) => {
                let now_ms = self.now_ms();
                for conversation in &list {
                    self.store
                        .upsert_conversation(&stored_conversation(conversation, now_ms))?;
                }
                self.capabilities
                    .observe_ok(now, Capability::ListConversations);
                round
                    .effects
                    .push(Effect::ConversationsRefreshed(list.len()));
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
        let mut queue = vec![event];
        let mut steps = 0;
        let uncertain_before: Vec<ClientMessageId> = self
            .uncertain_sends()
            .into_iter()
            .map(|(_, client_id)| client_id)
            .collect();

        while let Some(event) = queue.pop() {
            steps += 1;
            if steps > MAX_STEPS {
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
                        let outcome = self.fetch(&conversation, &mut round).await?;
                        queue.push(Event::OperationFinished { op, outcome });
                    }
                    Decision::AttemptSend {
                        op,
                        conversation,
                        client_id,
                    } => {
                        let outcome = self.send(&conversation, &client_id, &mut round).await;
                        queue.push(Event::OperationFinished { op, outcome });
                    }
                    Decision::Reconnect { op } => {
                        let outcome = self.reconnect(&mut round).await;
                        queue.push(Event::OperationFinished { op, outcome });
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
    ) -> Result<Outcome, DriverError> {
        let now = self.clock.now();
        match self.client.history(&conversation.0, self.top).await {
            Ok(messages) => {
                let now_ms = self.now_ms();
                self.store.ensure_conversation(&conversation.0, now_ms)?;
                let batch: Vec<StoredMessage> = messages
                    .iter()
                    .map(|message| stored_message(conversation, message))
                    .collect();
                // Atômico e idempotente: repetir uma busca interrompida não inventa mensagem.
                self.store.upsert_messages(&batch)?;
                self.capabilities.observe_ok(now, Capability::ReadHistory);
                round.effects.push(Effect::Fetched {
                    conversation: conversation.clone(),
                    messages: batch.len(),
                });
                Ok(Outcome::Succeeded)
            }
            Err(error) => {
                self.on_failure(now, Capability::ReadHistory, &error);
                Ok(OpFailure::classify(&error).outcome())
            }
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
                let failure = OpFailure::classify(&error);
                // Resposta perdida: o motor vai reconciliar, não reenviar.
                failure.outcome()
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

    fn now_ms(&self) -> i64 {
        self.clock
            .utc_now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0)
    }
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
        // `HISTORY-SHAPE-001` ainda não traz o campo: a reconciliação depende de
        // `Driver::note_observed` (ver o doc do crate).
        client_message_id: None,
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
}
