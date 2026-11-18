//! `f365-ui` — runtime do app de UI (Fase 3, #19). **Sem toolkit.**
//!
//! Este é o processo que possui o core: monta a fachada do `f365-ipc`, recebe os acontecimentos do
//! tempo real por um canal e os converte em rodadas do motor. O frontend (Tauri + React, num
//! workspace separado) só chama os comandos daqui — nada de I/O de rede na UI.
//!
//! Por que o shell Tauri não mora neste crate: `tauri` arrasta GTK/WebKit e derrubaria o job Linux
//! headless. Separar o runtime do toolkit mantém a Fase 3 buildável e testável em qualquer runner.
//!
//! O laço de `TeamsClient::listen` **não** está aqui de propósito: quem o roda é a casca (Tauri ou
//! CLI) e entrega os eventos por [`PumpHandle`]. Assim o runtime é testável sem provider nenhum.

use f365_auth::Clock;
use f365_ipc::{
    ConversationDto, DerivativeDto, EventDto, IpcError, MessageDto, OlderDto, RoundDto, Shell,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

pub mod realtime;
pub use realtime::RealtimeLink;

/// Acontecimento que entra no runtime vindo do tempo real (ou de um teste).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// O canal avisou que esta conversa mudou. Invalidação, não dado.
    Invalidated { conversation: String },
    /// O canal de tempo real caiu (`false`) ou voltou (`true`).
    Realtime { connected: bool },
}

/// O que o frontend recebe para re-renderizar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// Uma rodada do motor aconteceu (busca, envio, renovação, estado por capacidade).
    Round(RoundDto),
    /// O canal de tempo real mudou de estado.
    Realtime { connected: bool },
}

/// O runtime já foi encerrado: não há para quem entregar o acontecimento.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("runtime encerrado")]
pub struct RuntimeClosed;

/// Alça para a casca (Tauri/CLI) entregar acontecimentos ao runtime.
#[derive(Debug, Clone)]
pub struct PumpHandle(UnboundedSender<Inbound>);

impl PumpHandle {
    /// Entrega um acontecimento. Falha só se o runtime já foi descartado.
    pub fn send(&self, inbound: Inbound) -> Result<(), RuntimeClosed> {
        self.0.send(inbound).map_err(|_| RuntimeClosed)
    }

    pub fn invalidate(&self, conversation: impl Into<String>) -> Result<(), RuntimeClosed> {
        self.send(Inbound::Invalidated {
            conversation: conversation.into(),
        })
    }

    pub fn realtime(&self, connected: bool) -> Result<(), RuntimeClosed> {
        self.send(Inbound::Realtime { connected })
    }
}

/// Lado receptor do canal. Fica com **quem possui o runtime** (a casca ou um teste).
#[derive(Debug)]
pub struct Inbox(UnboundedReceiver<Inbound>);

impl Inbox {
    /// Espera o próximo acontecimento (cancel-safe: nada se perde ao cancelar).
    pub async fn recv(&mut self) -> Option<Inbound> {
        self.0.recv().await
    }

    /// Versão sem espera, para teste.
    pub fn try_recv(&mut self) -> Option<Inbound> {
        self.0.try_recv().ok()
    }
}

/// Canal entre quem produz acontecimentos (tempo real, testes) e o runtime.
///
/// Separado do [`App`] de propósito: o [`RealtimeLink`] precisa da alça **antes** de o `Driver`
/// existir, e o `Inbox` alimenta o runtime depois.
pub fn channel() -> (PumpHandle, Inbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    (PumpHandle(tx), Inbox(rx))
}

/// Runtime do app: uma fachada única para o frontend.
///
/// Não possui o canal de entrada de propósito: quem orquestra (a casca) precisa esperar por um
/// acontecimento **e** por um comando do frontend ao mesmo tempo, e isso exige emprestar o canal e
/// o runtime separadamente.
pub struct App {
    shell: Shell,
    clock: Arc<dyn Clock>,
    wake_at: Option<Instant>,
}

impl App {
    pub fn new(shell: Shell, clock: Arc<dyn Clock>) -> Self {
        Self {
            shell,
            clock,
            wake_at: None,
        }
    }

    // ---- leitura (cache, sem rede) ----

    pub fn conversations(&self, limit: usize) -> Result<Vec<ConversationDto>, IpcError> {
        self.shell.conversations(limit)
    }

    pub fn messages(
        &self,
        conversation_id: &str,
        before_ms: Option<i64>,
        limit: usize,
    ) -> Result<Vec<MessageDto>, IpcError> {
        self.shell.messages(conversation_id, before_ms, limit)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MessageDto>, IpcError> {
        self.shell.search(query, limit)
    }

    pub fn capabilities(&self) -> Vec<String> {
        self.shell.capabilities()
    }

    pub fn me(&self) -> Option<String> {
        self.shell.me()
    }

    pub fn agenda(&self, from_ms: i64, to_ms: i64) -> Result<Vec<EventDto>, IpcError> {
        self.shell.agenda(from_ms, to_ms)
    }

    pub fn join_url(&self, event_id: &str) -> Result<String, IpcError> {
        self.shell.join_url(event_id)
    }

    pub fn draft(&self, conversation_id: &str) -> Result<Option<String>, IpcError> {
        self.shell.draft(conversation_id)
    }

    pub fn set_draft(&self, conversation_id: &str, text: &str) -> Result<(), IpcError> {
        self.shell.set_draft(conversation_id, text)
    }

    pub fn derivatives(&self, message_id: &str) -> Result<Vec<DerivativeDto>, IpcError> {
        self.shell.derivatives(message_id)
    }

    pub fn approve_derivative(&self, derivative_id: i64) -> Result<String, IpcError> {
        self.shell.approve_derivative(derivative_id)
    }

    // ---- comandos ----

    pub async fn refresh_conversations(&mut self) -> Result<RoundDto, IpcError> {
        let round = self.shell.refresh_conversations().await?;
        self.note(&round);
        Ok(round)
    }

    pub async fn send(&mut self, conversation_id: &str, text: &str) -> Result<RoundDto, IpcError> {
        let round = self.shell.send(conversation_id, text).await?;
        self.note(&round);
        Ok(round)
    }

    /// "Carregar mais histórico" da conversa.
    pub async fn older(&mut self, conversation_id: &str) -> Result<OlderDto, IpcError> {
        let older = self.shell.load_older(conversation_id).await?;
        self.note(&older.round);
        Ok(older)
    }

    pub async fn connection_restored(&mut self) -> Result<RoundDto, IpcError> {
        let round = self.shell.connection_restored().await?;
        self.note(&round);
        Ok(round)
    }

    /// Faz o motor processar prazos vencidos (reconciliação de envio, renovação de token).
    ///
    /// A casca pode chamar isto periodicamente enquanto o empurrão do tempo real não existe.
    pub async fn wake(&mut self) -> Result<RoundDto, IpcError> {
        let round = self.shell.wake().await?;
        self.note(&round);
        Ok(round)
    }

    /// Espera o próximo acontecimento **sem** tocar no motor.
    ///
    /// É o que a casca usa num `select!`: `recv` é cancel-safe, então esperar por um comando do
    /// frontend ao mesmo tempo não aborta nenhuma operação em voo.
    pub fn wake_at(&self) -> Option<Instant> {
        self.wake_at
    }

    /// Aplica um acontecimento no motor e devolve a rodada. Pode fazer I/O — chame fora de
    /// `select!`.
    pub async fn apply(&mut self, inbound: Inbound) -> Result<RoundDto, IpcError> {
        match inbound {
            Inbound::Invalidated { conversation } => {
                let round = self.shell.invalidated(&conversation).await?;
                self.note(&round);
                Ok(round)
            }
            // Estado do canal não mexe no motor: quem desenha isso é o frontend.
            Inbound::Realtime { .. } => Ok(RoundDto::default()),
        }
    }

    /// Próximo acontecimento para o frontend: um evento do tempo real **ou** o prazo que o motor
    /// pediu (reconciliação/renovação). `None` = o runtime foi encerrado.
    ///
    /// Conveniência para consumidor único. A casca usa `inbox.recv()` + [`apply`](Self::apply) +
    /// [`wake`](Self::wake) num `select!`, para não prender o motor enquanto espera.
    pub async fn next_event(&mut self, inbox: &mut Inbox) -> Option<UiEvent> {
        let wake = self.wake_at;
        tokio::select! {
            inbound = inbox.recv() => {
                match inbound? {
                    Inbound::Realtime { connected } => Some(UiEvent::Realtime { connected }),
                    inbound @ Inbound::Invalidated { .. } => {
                        Some(UiEvent::Round(self.apply(inbound).await.ok()?))
                    }
                }
            }
            () = sleep_until(self.clock.clone(), wake) => {
                Some(UiEvent::Round(self.wake().await.ok()?))
            }
        }
    }

    /// Guarda o prazo pedido pelo motor, para o `next_event` acordar sozinho.
    fn note(&mut self, round: &RoundDto) {
        self.wake_at = round
            .wake_in_ms
            .map(|ms| self.clock.now() + Duration::from_millis(ms));
    }
}

/// Dorme até o prazo (ou para sempre, se não há prazo). Piso curto evita laço quente.
async fn sleep_until(clock: Arc<dyn Clock>, at: Option<Instant>) {
    let Some(at) = at else {
        std::future::pending::<()>().await;
        return;
    };
    let wait = at
        .saturating_duration_since(clock.now())
        .max(Duration::from_millis(50));
    clock.sleep(wait).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use f365_auth::transport::HttpResponse;
    use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError, ManualClock};
    use f365_driver::Driver;
    use f365_storage::Store;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const CHATS: &str = r#"{"chats":[{"id":"19:a@thread.v2","title":"Time"}]}"#;
    const HISTORY: &str =
        r#"{"messages":[{"id":"m1","content":"oi","composetime":"2024-01-01T00:00:00Z"}]}"#;

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

    fn app(replies: Vec<Result<HttpResponse, ExecError>>) -> (ManualClock, App, PumpHandle, Inbox) {
        let clock = ManualClock::default();
        let exec = FakeExec::replying(replies);
        let store = Store::open_in_memory().expect("banco");
        let clock_arc: Arc<dyn Clock> = Arc::new(clock.clone());
        let driver = Driver::new(exec, store, clock_arc.clone());
        let shell = Shell::new(driver, clock_arc.clone());
        let (pump, inbox) = channel();
        (clock, App::new(shell, clock_arc), pump, inbox)
    }

    #[tokio::test]
    async fn invalidation_from_the_pump_drives_the_machine() {
        let (_clock, mut app, pump, mut inbox) = app(vec![ok(200, CHATS), ok(200, HISTORY)]);
        app.refresh_conversations().await.expect("conversas");

        pump.invalidate("19:a@thread.v2").expect("entrega");
        let event = app.next_event(&mut inbox).await.expect("evento");

        match event {
            UiEvent::Round(round) => {
                assert!(round
                    .effects
                    .iter()
                    .any(|effect| format!("{effect:?}").contains("Fetched")));
            }
            other => panic!("esperava rodada, veio {other:?}"),
        }
        // O conteúdo da invalidação já está no cache, pronto para o próximo frame.
        let messages = app.messages("19:a@thread.v2", None, 10).expect("histórico");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "m1");
    }

    #[tokio::test]
    async fn apply_reports_the_round_and_the_next_wake() {
        let (_clock, mut app, _pump, _inbox) = app(vec![ok(200, CHATS), ok(200, HISTORY)]);
        app.refresh_conversations().await.expect("conversas");

        let round = app
            .apply(Inbound::Invalidated {
                conversation: "19:a@thread.v2".into(),
            })
            .await
            .expect("aplica");

        assert!(format!("{:?}", round.effects).contains("Fetched"));
        // Estado do canal não mexe no motor.
        let noop = app
            .apply(Inbound::Realtime { connected: true })
            .await
            .expect("aplica");
        assert!(noop.effects.is_empty());
    }

    #[tokio::test]
    async fn realtime_state_is_surfaced_without_touching_the_machine() {
        let (_clock, mut app, pump, mut inbox) = app(vec![]);

        pump.realtime(false).expect("entrega");
        assert_eq!(
            app.next_event(&mut inbox).await.expect("evento"),
            UiEvent::Realtime { connected: false }
        );

        pump.realtime(true).expect("entrega");
        assert_eq!(
            app.next_event(&mut inbox).await.expect("evento"),
            UiEvent::Realtime { connected: true }
        );
    }

    #[tokio::test]
    async fn closing_the_pump_ends_the_event_loop() {
        let (_clock, mut app, pump, mut inbox) = app(vec![]);
        drop(pump);
        // Sem alça e sem prazo pendente, o laço termina em vez de girar.
        let closed = tokio::time::timeout(Duration::from_secs(1), app.next_event(&mut inbox)).await;
        assert!(
            matches!(closed, Ok(None)),
            "esperava encerramento, veio {closed:?}"
        );
    }

    #[tokio::test]
    async fn first_frame_comes_from_the_cache() {
        let (_clock, mut app, _pump, _inbox) = app(vec![ok(200, CHATS)]);
        app.refresh_conversations().await.expect("conversas");

        let list = app.conversations(10).expect("lista");

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Time");
    }
}
