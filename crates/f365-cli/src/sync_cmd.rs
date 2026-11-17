//! `f365 sync` — primeira composição real do core: cache local + motor + executor + tempo real
//! num laço só. Fase 2 (#16).
//!
//! É a camada de **composição**: nada de decisão mora aqui. O motor decide (`f365-sync`), o driver
//! executa (`f365-driver`), o cache guarda (`f365-storage`) e este arquivo só liga as peças com o
//! executor autenticado.
//!
//! O que ele **não** faz: abrir WebView. A renovação silenciosa fica no `TokenBroker` (perfil
//! persistente da WebView, janela escondida — ela só aparece se o Microsoft pedir login), e quem
//! roda é o dono (`AGENTS.md`).

use anyhow::Context as _;
use f365_auth::executor::SessionExecutor;
use f365_auth::tokens::{TokenBroker, TokenError, TokenResource};
use f365_auth::transport::RustTransport;
use f365_auth::{AuthenticatedExecutor, Clock, SystemClock};
use f365_driver::{Driver, Effect, Notifier, Realtime, RefreshError, Round, TokenKeeper};
use f365_storage::Store;
use f365_sync::{ConversationId, Event};
use f365_teams::{RealtimeEvent, TeamsClient};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Options {
    /// Banco do cache. Padrão: `%LOCALAPPDATA%\fast365\f365-cache.db`.
    pub db: Option<PathBuf>,
    pub top: usize,
    /// Só atualiza o cache de conversas e sai (verificação rápida, sem laço).
    pub once: bool,
}

pub async fn sync(options: Options) -> anyhow::Result<()> {
    let db = db_path(options.db.as_ref())?;
    let store = Store::open(&db).with_context(|| format!("abrindo {}", db.display()))?;
    println!(
        "Cache local: {} (schema v{})\n",
        db.display(),
        store.schema_version()?
    );

    // Um broker só: o executor e o renovador compartilham a mesma sessão.
    let broker = Arc::new(crate::token_cmd::broker()?);
    let exec: Arc<dyn AuthenticatedExecutor> = Arc::new(SessionExecutor::new(
        broker.clone(),
        Arc::new(RustTransport::new()?),
        None,
        Arc::new(SystemClock),
    ));

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = Arc::new(Listener::new(exec.clone(), events_tx));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let mut driver = Driver::new(exec, store, clock.clone())
        .with_top(options.top)
        .with_notifier(Arc::new(ConsoleNotifier))
        .with_realtime(listener.clone())
        .with_token_keeper(Arc::new(BrokerKeeper { broker }));

    let mut last_caps: Vec<String> = Vec::new();
    let round = driver.refresh_conversations().await?;
    print_round(&round, &mut last_caps);

    if options.once {
        return Ok(());
    }

    let mut round = driver.drive(Event::ConnectionRestored).await?;
    print_round(&round, &mut last_caps);
    println!("Tempo real e cache ativos. Ctrl+C encerra.\n");

    let mut wake = round.wake_at;
    loop {
        tokio::select! {
            biased;
            () = sleep_until(clock.clone(), wake) => {
                wake = None;
                round = driver.drive(Event::WakeUp).await?;
            }
            event = events_rx.recv() => {
                let Some(event) = event else { break };
                round = driver.drive(event).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nEncerrado.");
                break;
            }
        }
        wake = next_deadline(wake, round.wake_at);
        print_round(&round, &mut last_caps);
    }
    Ok(())
}

fn next_deadline(previous: Option<Instant>, current: Option<Instant>) -> Option<Instant> {
    match (previous, current) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Expectativa de `WakeUp`: dorme até o prazo pedido (ou para sempre, se não há prazo).
async fn sleep_until(clock: Arc<dyn Clock>, at: Option<Instant>) {
    let Some(at) = at else {
        std::future::pending::<()>().await;
        return;
    };
    let wait = at.saturating_duration_since(clock.now());
    if !wait.is_zero() {
        clock.sleep(wait).await;
    }
}

fn print_round(round: &Round, last_caps: &mut Vec<String>) {
    for effect in &round.effects {
        match effect {
            Effect::ConversationsRefreshed(count) => println!("{count} conversa(s) no cache"),
            Effect::Fetched {
                conversation,
                messages,
            } => println!("  {} mensagem(ns) em {}", messages, conversation.0),
            Effect::SendAttempted { client_id, .. } => println!("  enviando {client_id:?}"),
            Effect::Sent { client_id, .. } => println!("  enviado {client_id:?}"),
            Effect::Uncertain { client_id, .. } => {
                println!("  INCERTO {client_id:?} — confira no Teams antes de reenviar")
            }
            Effect::ReconnectAttempt => println!("  reabrindo o tempo real"),
            Effect::TokenRefreshed => println!("  token renovado em silêncio"),
            Effect::TokenRefreshFailed { retry_in } => {
                println!(
                    "  renovação falhou; nova tentativa em {}s",
                    retry_in.as_secs()
                )
            }
            Effect::SessionExpired => {
                println!("  SESSÃO EXPIRADA — rode `f365 auth` (o dono é quem faz login)")
            }
            // Estado do cache e aviso de conversa publicada já aparecem no resumo abaixo.
            Effect::Persisted(_) | Effect::Published(_) => {}
        }
    }
    // Degradação parcial explícita só quando muda, para não inundar o terminal (§2.5).
    if round.capabilities != *last_caps && !round.capabilities.is_empty() {
        *last_caps = round.capabilities.clone();
        println!("  capacidades: {}", round.capabilities.join(" · "));
    }
}

fn db_path(override_path: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.clone());
    }
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA não definido")?;
    let dir = std::path::Path::new(&local).join("fast365");
    std::fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;
    Ok(dir.join("f365-cache.db"))
}

/// Traduz mudanças de canal e invalidações; sinais de progresso não são dados.
fn event_from_realtime(event: &RealtimeEvent) -> Option<Event> {
    match event {
        RealtimeEvent::Changed { conversation } => {
            conversation.as_ref().map(|id| Event::Invalidated {
                conversation: ConversationId(id.clone()),
            })
        }
        RealtimeEvent::Ready => Some(Event::ConnectionRestored),
        RealtimeEvent::Reconnecting { .. } => Some(Event::RealtimeLost),
        _ => None,
    }
}

/// Ponte entre o laço do Trouter (streaming) e o motor (eventos discretos).
struct Listener {
    exec: Arc<dyn AuthenticatedExecutor>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    connected: Arc<AtomicBool>,
    spawned: Arc<AtomicBool>,
}

impl Listener {
    fn new(
        exec: Arc<dyn AuthenticatedExecutor>,
        events: tokio::sync::mpsc::UnboundedSender<Event>,
    ) -> Self {
        Self {
            exec,
            events,
            connected: Arc::new(AtomicBool::new(false)),
            spawned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Sobe o laço do Trouter uma vez; ele mesmo repete/resubscreve a cada queda.
    fn spawn(&self, ready: tokio::sync::oneshot::Sender<()>) {
        let exec = self.exec.clone();
        let events = self.events.clone();
        let connected = self.connected.clone();
        let spawned = self.spawned.clone();
        tokio::spawn(async move {
            let client = TeamsClient::new(exec);
            let clock = SystemClock;
            let mut ready = Some(ready);
            // O callback toma o flag; o laço guarda uma cópia para marcar a queda no fim.
            let during = connected.clone();
            let mut on_event = move |event: RealtimeEvent| {
                match &event {
                    RealtimeEvent::Ready => {
                        during.store(true, Ordering::SeqCst);
                        if let Some(tx) = ready.take() {
                            let _ = tx.send(());
                        }
                    }
                    RealtimeEvent::Reconnecting { .. } => during.store(false, Ordering::SeqCst),
                    _ => {}
                }
                if let Some(event) = event_from_realtime(&event) {
                    let _ = events.send(event);
                }
            };
            let _ = client.listen(&clock, &mut on_event).await;
            connected.store(false, Ordering::SeqCst);
            spawned.store(false, Ordering::SeqCst);
        });
    }
}

#[async_trait::async_trait]
impl Realtime for Listener {
    async fn reconnect(&self) -> Result<(), String> {
        if !self.spawned.swap(true, Ordering::SeqCst) {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            self.spawn(ready_tx);
            return match tokio::time::timeout(Duration::from_secs(30), ready_rx).await {
                Ok(Ok(())) => Ok(()),
                _ => Err("tempo real não ficou pronto em 30 s".into()),
            };
        }
        // O laço do provider já repete sozinho: aqui só reportamos se o canal está de pé.
        if self.connected.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("tempo real ainda reconectando".into())
        }
    }
}

/// Renovação silenciosa pelo `TokenBroker` (que já faz o re-pareamento escondido quando o RT de
/// SPA vence — janela só aparece se o Microsoft pedir login).
struct BrokerKeeper {
    broker: Arc<TokenBroker>,
}

#[async_trait::async_trait]
impl TokenKeeper for BrokerKeeper {
    async fn refresh(&self) -> Result<Duration, RefreshError> {
        match self.broker.describe(TokenResource::Teams).await {
            Ok(info) => {
                let left = info
                    .expires_at
                    .duration_since(SystemClock.utc_now())
                    .unwrap_or_default();
                // Renova na metade da validade restante, com piso de 1 min.
                Ok((left / 2).max(Duration::from_secs(60)))
            }
            Err(error) => Err(match error {
                TokenError::Transport(_) => RefreshError::retryable("falha de rede ao renovar"),
                TokenError::Http { status, .. } if status >= 500 || status == 429 => {
                    RefreshError::retryable(format!("token endpoint respondeu HTTP {status}"))
                }
                other => RefreshError::needs_login(other.to_string()),
            }),
        }
    }
}

struct ConsoleNotifier;

impl Notifier for ConsoleNotifier {
    fn published(&self, conversation: &ConversationId) {
        println!("  mudança em {}", conversation.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_loss_does_not_block_rest_send() {
        let mut machine = f365_sync::SyncMachine::new();
        let now = Instant::now();
        let event = event_from_realtime(&RealtimeEvent::Reconnecting {
            attempt: 1,
            after: Duration::from_secs(2),
        })
        .unwrap();
        machine.handle(now, event);
        let decisions = machine.handle(
            now,
            Event::SendRequested {
                conversation: ConversationId("test".into()),
                client_id: f365_sync::ClientMessageId("1".into()),
            },
        );
        assert!(decisions
            .iter()
            .any(|d| matches!(d, f365_sync::Decision::AttemptSend { .. })));
    }

    #[test]
    fn only_changed_becomes_a_machine_event() {
        assert_eq!(
            event_from_realtime(&RealtimeEvent::Changed {
                conversation: Some("19:abc@thread.v2".into())
            }),
            Some(Event::Invalidated {
                conversation: ConversationId("19:abc@thread.v2".into())
            })
        );
        // Sem id legível não há o que invalidar.
        assert!(event_from_realtime(&RealtimeEvent::Changed { conversation: None }).is_none());
        assert_eq!(
            event_from_realtime(&RealtimeEvent::Ready),
            Some(Event::ConnectionRestored)
        );
        assert!(event_from_realtime(&RealtimeEvent::Step("1/4")).is_none());
        assert!(event_from_realtime(&RealtimeEvent::Signal("trouter.connected".into())).is_none());
        assert_eq!(
            event_from_realtime(&RealtimeEvent::Reconnecting {
                attempt: 1,
                after: Duration::from_secs(2)
            }),
            Some(Event::RealtimeLost)
        );
    }

    #[tokio::test]
    async fn listener_retries_after_initial_failure() {
        use f365_auth::transport::HttpResponse;
        use f365_auth::{AuthOp, ExecError};
        use std::sync::atomic::AtomicUsize;
        struct Failing(AtomicUsize);
        #[async_trait::async_trait]
        impl AuthenticatedExecutor for Failing {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(HttpResponse {
                    status: 503,
                    headers: vec![],
                    body: vec![],
                })
            }
        }
        let exec = Arc::new(Failing(AtomicUsize::new(0)));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = Listener::new(exec.clone(), tx);
        assert!(listener.reconnect().await.is_err());
        let first = exec.0.load(Ordering::SeqCst);
        assert!(first > 0);
        assert!(listener.reconnect().await.is_err());
        assert!(exec.0.load(Ordering::SeqCst) > first);
    }

    #[test]
    fn db_path_honours_the_override() {
        let path = PathBuf::from(r"C:\temp\cache.db");
        assert_eq!(db_path(Some(&path)).expect("caminho"), path);
    }

    #[test]
    fn earlier_pending_deadline_survives_later_event() {
        let earlier = Instant::now();
        let later = earlier + Duration::from_secs(5);
        assert_eq!(next_deadline(Some(earlier), Some(later)), Some(earlier));
        assert_eq!(next_deadline(Some(earlier), None), Some(earlier));
    }

    #[tokio::test]
    async fn continuous_events_do_not_postpone_absolute_wake() {
        let manual = f365_auth::ManualClock::default();
        let clock: Arc<dyn Clock> = Arc::new(manual.clone());
        let deadline = clock.now() + Duration::from_millis(100);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for _ in 0..20 {
            tx.send(()).unwrap();
        }
        let mut events = 0;
        loop {
            tokio::select! {
                biased;
                () = sleep_until(clock.clone(), Some(deadline)) => break,
                Some(()) = rx.recv() => { events += 1; manual.advance(Duration::from_millis(10)); }
            }
        }
        assert_eq!(events, 10);
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn expired_deadline_fires_without_restarting_floor() {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let past = clock.now() - Duration::from_millis(1);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), sleep_until(clock, Some(past)))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn wake_without_deadline_never_fires_on_its_own() {
        // Sem prazo, `sleep_until` fica pendente: quem chama acorda por evento ou Ctrl+C.
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let outcome =
            tokio::time::timeout(Duration::from_millis(50), sleep_until(clock, None)).await;
        assert!(outcome.is_err(), "não deveria acordar sozinho");
    }
}
