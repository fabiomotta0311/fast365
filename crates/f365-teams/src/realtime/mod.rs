//! Capacidade: tempo real pelo Trouter (Checkpoint D, `docs/ARQUITETURA.md` §2.3/§3.5).
//!
//! **Evento = invalidação, não dado:** de cada notificação sai só *qual* conversa mudou; o conteúdo
//! é buscado depois via REST. O texto das mensagens nunca é lido aqui.
//!
//! Fluxo (contratos em `trouter`): dados de conexão → handshake socket.io v1 → WebSocket →
//! confirmação de conexão → registro do endpoint → laço (heartbeat, keep-alive, resposta a cada
//! mensagem). As chamadas HTTP passam pelo executor (skype token não sai do `f365-auth`); o socket
//! abre do Rust só com os parâmetros assinados do Trouter. Queda depois de conectado → reconexão
//! com backoff; falha antes da primeira conexão → erro imediato citando o contrato.

mod socketio;
mod trouter;
#[cfg(test)]
mod wiremock_e2e;

use crate::{TeamsClient, TeamsError};
use f365_auth::transport::HttpResponse;
use f365_auth::Clock;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::Value;
use socketio::Frame;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// Prazo para o socket confirmar a conexão (`1::`) depois de aberto.
const CONNECT_CONFIRM: Duration = Duration::from_secs(15);
/// Prazo total de DNS, TCP, TLS e upgrade WebSocket.
const WS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(30);
/// Prazo para cada escrita, incluindo flush; após vencê-lo o socket é descartado.
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Keep-alive quando o handshake não informa o heartbeat do servidor.
const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(30);
#[cfg(test)]
const MAX_CONSECUTIVE_FAILURES: u32 = 8;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_INCOMING: usize = 1024 * 1024;
const MAX_ID: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeEvent {
    /// Progresso da conexão (para diagnóstico do Checkpoint D).
    Step(&'static str),
    /// Socket conectado e endpoint registrado: a partir daqui chegam notificações.
    Ready,
    /// Algo mudou numa conversa (`None` = notificação sem id legível). Buscar via REST.
    Changed { conversation: Option<String> },
    /// Evento nomeado do Trouter que não é de conversa (ex.: `trouter.connected`).
    Signal(String),
    /// Sessão caiu; nova tentativa depois de `after`.
    Reconnecting { attempt: u32, after: Duration },
}

type OnEvent<'a> = &'a mut (dyn FnMut(RealtimeEvent) + Send);

impl TeamsClient {
    /// Escuta o tempo real até falhar de vez. Só retorna com erro.
    pub async fn listen(&self, clock: &dyn Clock, on_event: OnEvent<'_>) -> Result<(), TeamsError> {
        let mut failures = 0u32;
        let mut ever_ready = false;
        loop {
            let mut session_ready = false;
            let mut forward = |event| {
                if event == RealtimeEvent::Ready {
                    session_ready = true;
                }
                on_event(event);
            };
            let ended_ok = match self.listen_once(clock, &mut forward, &mut ever_ready).await {
                Ok(()) => true,
                Err(e) if !ever_ready => return Err(e),
                Err(_) => false,
            };
            failures = next_failure_count(failures, ended_ok, session_ready);
            let after = MAX_BACKOFF.min(Duration::from_secs(1 << failures.min(6)));
            on_event(RealtimeEvent::Reconnecting {
                attempt: failures,
                after,
            });
            clock.sleep(after).await;
        }
    }

    async fn listen_once(
        &self,
        clock: &dyn Clock,
        on_event: OnEvent<'_>,
        ever_ready: &mut bool,
    ) -> Result<(), TeamsError> {
        let epid = uuid::Uuid::new_v4().to_string();

        on_event(RealtimeEvent::Step("1/4 dados de conexão do Trouter"));
        let resp = self.exec.execute(trouter::connect_op(&epid)).await?;
        ensure_2xx("Trouter (conexão)", &resp)?;
        tracing::debug!(campos = ?trouter::shape(&resp.body), "Trouter: formato da resposta de conexão");
        let parse_start = Instant::now();
        let parsed = trouter::parse_connect(&resp.body);
        if let Some(recorder) = &self.recorder {
            recorder.trouter_protocol(false, parsed.is_ok(), parse_start);
        }
        let mut info = parsed?;
        let ms = clock
            .utc_now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        info.con_num = Some(format!("{ms}_1"));

        on_event(RealtimeEvent::Step("2/4 handshake socket.io"));
        let resp = self
            .exec
            .execute(trouter::handshake_op(&info, &epid))
            .await?;
        ensure_2xx("Trouter (handshake)", &resp)?;
        let parse_start = Instant::now();
        let parsed = std::str::from_utf8(&resp.body)
            .ok()
            .and_then(socketio::parse_handshake)
            .filter(|h| h.transports.iter().any(|t| t == "websocket"))
            .ok_or(TeamsError::Malformed(
                "Trouter: handshake socket.io inesperado (TROUTER-HANDSHAKE-001)",
            ));
        if let Some(recorder) = &self.recorder {
            recorder.trouter_protocol(true, parsed.is_ok(), parse_start);
        }
        let hs = parsed?;

        on_event(RealtimeEvent::Step("3/4 WebSocket"));
        let ws_url = trouter::ws_url(&info, &hs.session_id, &epid);
        #[cfg(test)]
        let ws_url = self.realtime_test_url.clone().unwrap_or(ws_url);
        let ws_start = Instant::now();
        let (mut ws, _) = tokio::select! {
            result = tokio_tungstenite::connect_async_with_config(ws_url, Some(ws_config()), false) => result.map_err(|e| {
                if let Some(recorder) = &self.recorder { recorder.websocket_failure(crate::diagnostics::WebSocketFailure::Upgrade, ws_start); }
                TeamsError::Realtime(format!("WebSocket recusado: {} (TROUTER-WS-001)", ws_error_kind(&e)))
            })?,
            () = clock.sleep(WS_UPGRADE_TIMEOUT) => {
                if let Some(recorder) = &self.recorder { recorder.websocket_failure(crate::diagnostics::WebSocketFailure::UpgradeTimeout, ws_start); }
                return Err(TeamsError::Realtime("tempo esgotado no upgrade WebSocket (TROUTER-WS-001)".into()));
            },
        };
        await_connect(&mut ws, clock).await.inspect_err(|_| {
            if let Some(recorder) = &self.recorder {
                recorder.websocket_failure(
                    crate::diagnostics::WebSocketFailure::Confirmation,
                    ws_start,
                );
            }
        })?;

        on_event(RealtimeEvent::Step("4/4 registro do endpoint"));
        let resp = self
            .exec
            .execute(trouter::registrar_op(&info, &epid))
            .await?;
        ensure_2xx("Trouter (registrador, TROUTER-REGISTRAR-001)", &resp)?;

        *ever_ready = true;
        on_event(RealtimeEvent::Ready);
        run_session(&mut ws, clock, hs.heartbeat, on_event)
            .await
            .inspect_err(|_| {
                if let Some(recorder) = &self.recorder {
                    recorder
                        .websocket_failure(crate::diagnostics::WebSocketFailure::Session, ws_start);
                }
            })
    }
}

fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_INCOMING))
        .max_frame_size(Some(MAX_INCOMING))
}

fn next_failure_count(previous: u32, ended_ok: bool, session_ready: bool) -> u32 {
    if ended_ok || session_ready {
        1
    } else {
        previous.saturating_add(1)
    }
}

fn ensure_2xx(service: &'static str, resp: &HttpResponse) -> Result<(), TeamsError> {
    if (200..300).contains(&resp.status) {
        return Ok(());
    }
    tracing::debug!(service, status = resp.status, "Trouter recusou");
    Err(TeamsError::Http {
        service,
        status: resp.status,
    })
}

/// Tipo do erro do WebSocket, sem URL (a query carrega os parâmetros assinados).
fn ws_error_kind(e: &WsError) -> String {
    match e {
        WsError::Http(resp) => format!("HTTP {}", resp.status().as_u16()),
        WsError::Tls(_) => "TLS".into(),
        WsError::Io(io) => format!("E/S ({:?})", io.kind()),
        WsError::Url(_) => "URL".into(),
        WsError::Protocol(p) => format!("protocolo ({p})"),
        other => format!("{:?}", std::mem::discriminant(other)),
    }
}

async fn await_connect<S>(ws: &mut S, clock: &dyn Clock) -> Result<(), TeamsError>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    let wait = async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Text(t)) if t.len() > MAX_INCOMING => {
                    return Err(TeamsError::Realtime(
                        "entrada do socket excede o limite".into(),
                    ))
                }
                Ok(Message::Text(t)) => match socketio::parse(t.as_str()) {
                    Some(Frame::Connect) => return Ok(()),
                    Some(Frame::Error { .. } | Frame::Disconnect) => break,
                    _ => {}
                },
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        Err(TeamsError::Realtime(
            "socket fechou sem confirmar a conexão (TROUTER-WS-001)".into(),
        ))
    };
    tokio::select! {
        r = wait => r,
        () = clock.sleep(CONNECT_CONFIRM) => Err(TeamsError::Realtime("socket não confirmou a conexão a tempo (TROUTER-WS-001)".into())),
    }
}

fn id_within_limit(id: &Value) -> bool {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            if self.0 > MAX_ID {
                return Err(std::io::Error::other("limit"));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), id).is_ok()
}

fn silence_error(clock: &dyn Clock, last_received: Instant) -> TeamsError {
    TeamsError::Realtime(format!(
        "sem frames do servidor há {}s — conexão considerada morta",
        clock
            .now()
            .saturating_duration_since(last_received)
            .as_secs()
    ))
}

/// Laço de uma sessão conectada. `Ok(())` = servidor encerrou normalmente.
async fn run_session<S>(
    ws: &mut S,
    clock: &dyn Clock,
    heartbeat: Option<Duration>,
    on_event: OnEvent<'_>,
) -> Result<(), TeamsError>
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    // Keep-alive com instante ABSOLUTO: frames frequentes não podem adiá-lo indefinidamente.
    let every = heartbeat
        .map_or(DEFAULT_KEEPALIVE, |h| {
            h.saturating_sub(Duration::from_secs(5))
        })
        .max(Duration::from_secs(5));
    let invalid_heartbeat =
        || TeamsError::Realtime("intervalo de heartbeat fora do intervalo representável".into());
    let mut next_keepalive: Instant = clock
        .now()
        .checked_add(every)
        .ok_or_else(invalid_heartbeat)?;
    let silence = heartbeat
        .filter(|h| !h.is_zero())
        .map_or(Duration::from_secs(75), |h| h.saturating_mul(2));
    let mut last_received = clock.now();
    let (mut sink, mut stream) = ws.split();
    let (outgoing, mut outgoing_rx) = tokio::sync::mpsc::channel::<Message>(128);
    let pending = std::sync::atomic::AtomicUsize::new(0);
    let pending_bytes = std::sync::atomic::AtomicUsize::new(0);
    let enqueue = |message: Message| {
        let size = message.len();
        if pending_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_add(size)
            > 256 * 1024
        {
            return Err(TeamsError::Realtime(
                "orçamento de escrita do socket esgotado".into(),
            ));
        }
        outgoing.try_send(message).map_err(|_| {
            TeamsError::Realtime("fila de escrita do socket cheia ou encerrada".into())
        })?;
        pending.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pending_bytes.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    };
    let session_end = || {
        if pending.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            Ok(())
        } else {
            Err(TeamsError::Realtime(
                "socket encerrou com mensagens de escrita pendentes".into(),
            ))
        }
    };
    let writer = async {
        while let Some(message) = outgoing_rx.recv().await {
            let size = message.len();
            tokio::select! {
                biased;
                () = clock.sleep(WS_WRITE_TIMEOUT) => return Err(TeamsError::Realtime("tempo esgotado na escrita do socket".into())),
                result = sink.send(message) => result.map_err(|e| {
                    TeamsError::Realtime(format!("falha ao escrever no socket: {}", ws_error_kind(&e)))
                })?,
            }
            pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            pending_bytes.fetch_sub(size, std::sync::atomic::Ordering::Relaxed);
        }
        Ok::<(), TeamsError>(())
    };
    tokio::pin!(writer);
    loop {
        let now = clock.now();
        if now.saturating_duration_since(last_received) >= silence {
            return Err(silence_error(clock, last_received));
        }
        if now >= next_keepalive {
            enqueue(Message::text(socketio::encode(&Frame::Heartbeat)))?;
            next_keepalive = now.checked_add(every).ok_or_else(invalid_heartbeat)?;
        }
        let wait = next_keepalive
            .saturating_duration_since(now)
            .min(silence.saturating_sub(now.saturating_duration_since(last_received)));
        let incoming = tokio::select! {
            biased;
            result = &mut writer => return result,
            m = stream.next() => Some(m),
            () = clock.sleep(wait) => None,
        };
        let Some(incoming) = incoming else {
            continue;
        };
        if matches!(incoming, Some(Ok(_))) {
            last_received = clock.now();
        }
        let text = match incoming {
            None | Some(Ok(Message::Close(_))) => return session_end(),
            Some(Err(e)) => {
                return Err(TeamsError::Realtime(format!(
                    "socket caiu: {}",
                    ws_error_kind(&e)
                )))
            }
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(_)) => continue,
        };
        if text.len() > MAX_INCOMING {
            return Err(TeamsError::Realtime(
                "entrada do socket excede o limite".into(),
            ));
        }
        match socketio::parse(text.as_str()) {
            Some(Frame::Heartbeat) => {
                enqueue(Message::text(socketio::encode(&Frame::Heartbeat)))?;
            }
            Some(Frame::Message { data, .. }) => {
                let Ok(msg) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if let Some(id) = msg.get("id") {
                    if !id_within_limit(id) {
                        return Err(TeamsError::Realtime("id excede o limite".into()));
                    }
                }
                if let Some(reply) = trouter::response_for(&msg) {
                    enqueue(Message::text(reply))?;
                }
                on_event(RealtimeEvent::Changed {
                    conversation: trouter::conversation_of(&msg),
                });
            }
            Some(Frame::Event { data, .. }) => {
                if let Some(name) = trouter::event_name(&data) {
                    on_event(RealtimeEvent::Signal(name));
                }
            }
            Some(Frame::Disconnect) => return session_end(),
            Some(Frame::Error { .. }) => {
                return Err(TeamsError::Realtime(
                    "o Trouter enviou erro e encerrou".into(),
                ))
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use f365_auth::ManualClock;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, connect_async};

    #[tokio::test]
    async fn websocket_rejects_oversized_frame_before_session_parsing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            ws.send(Message::text("x".repeat(MAX_INCOMING + 1)))
                .await
                .unwrap();
        });
        let (mut ws, _) =
            tokio_tungstenite::connect_async_with_config(url, Some(ws_config()), false)
                .await
                .unwrap();
        let result = run_session(&mut ws, &ManualClock::default(), None, &mut |_| {}).await;
        assert!(matches!(result, Err(TeamsError::Realtime(s)) if s.contains("socket caiu")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_websocket_upgrade_times_out() {
        use async_trait::async_trait;
        use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Exec(AtomicUsize);
        #[async_trait]
        impl AuthenticatedExecutor for Exec {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                let body = match self.0.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        r#"{"socketio":"https://test.trouter.teams.microsoft.com/","surl":"synthetic","connectparams":{}}"#
                    }
                    1 => "sid:40:60:websocket",
                    _ => panic!("registration must not happen"),
                };
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: body.as_bytes().to_vec(),
                })
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TeamsClient::new(Arc::new(Exec(AtomicUsize::new(0))));
        client.realtime_test_url = Some(format!(
            "ws://{}?secret=synthetic",
            listener.local_addr().unwrap()
        ));
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let task = tokio::spawn(async move { client.listen(&ticker, &mut |_| {}).await });
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::task::yield_now().await;
        clock.advance(Duration::from_secs(29));
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "upgrade expired before 30s");
        clock.advance(Duration::from_secs(1));
        let result = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(result.is_ok(), "upgrade stalled without a deadline");
        let error = result.unwrap().unwrap().unwrap_err().to_string();
        assert!(error.contains("tempo esgotado no upgrade"), "{error}");
        assert!(!error.contains("synthetic"));
    }

    #[tokio::test]
    async fn upgrade_timeout_after_ready_retries_and_restores_ready() {
        use async_trait::async_trait;
        use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Exec(AtomicUsize);
        #[async_trait]
        impl AuthenticatedExecutor for Exec {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                let call = self.0.fetch_add(1, Ordering::SeqCst);
                let body = match call {
                    0 | 3 | 5 => {
                        r#"{"socketio":"https://test.trouter.teams.microsoft.com/","surl":"synthetic","connectparams":{}}"#
                    }
                    1 | 4 | 6 => "sid:40:60:websocket",
                    2 | 7 => "{}",
                    _ => panic!("unexpected HTTP call {call}"),
                };
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: body.as_bytes().to_vec(),
                })
            }
        }
        async fn event(
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<RealtimeEvent>,
        ) -> RealtimeEvent {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap()
        }
        async fn until(
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<RealtimeEvent>,
            expected: RealtimeEvent,
        ) {
            loop {
                if event(rx).await == expected {
                    break;
                }
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let exec = Arc::new(Exec(AtomicUsize::new(0)));
        let mut client = TeamsClient::new(exec.clone());
        client.realtime_test_url = Some(format!("ws://{}", listener.local_addr().unwrap()));
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            client
                .listen(&ticker, &mut |e| {
                    let _ = tx.send(e);
                })
                .await
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let mut first = accept_async(tcp).await.unwrap();
        first.send(Message::text("1::")).await.unwrap();
        until(&mut rx, RealtimeEvent::Ready).await;
        first.send(Message::text("7:::error")).await.unwrap();
        until(
            &mut rx,
            RealtimeEvent::Reconnecting {
                attempt: 1,
                after: Duration::from_secs(2),
            },
        )
        .await;
        tokio::task::yield_now().await;
        clock.advance(Duration::from_secs(2));
        let (stalled, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        tokio::task::yield_now().await;
        clock.advance(Duration::from_secs(29));
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        while let Ok(e) = rx.try_recv() {
            assert!(!matches!(e, RealtimeEvent::Reconnecting { .. }));
        }
        clock.advance(Duration::from_secs(1));
        until(
            &mut rx,
            RealtimeEvent::Reconnecting {
                attempt: 2,
                after: Duration::from_secs(4),
            },
        )
        .await;
        drop(stalled);
        tokio::task::yield_now().await;
        clock.advance(Duration::from_secs(4));
        let (tcp, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut recovered = accept_async(tcp).await.unwrap();
        recovered.send(Message::text("1::")).await.unwrap();
        until(&mut rx, RealtimeEvent::Ready).await;
        assert_eq!(exec.0.load(Ordering::SeqCst), 8);
        task.abort();
        let _ = task.await;
        assert_eq!(clock.pending_sleepers(), 0);
    }

    #[tokio::test]
    async fn first_failure_before_ready_is_returned_immediately() {
        use async_trait::async_trait;
        use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError};
        struct Failing;
        #[async_trait]
        impl AuthenticatedExecutor for Failing {
            async fn execute(&self, _: AuthOp) -> Result<HttpResponse, ExecError> {
                Ok(HttpResponse {
                    status: 503,
                    headers: vec![],
                    body: vec![],
                })
            }
        }
        let client = TeamsClient::new(Arc::new(Failing));
        let mut events = Vec::new();
        let result = client
            .listen(&ManualClock::default(), &mut |e| events.push(e))
            .await;
        assert!(matches!(result, Err(TeamsError::Http { status: 503, .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, RealtimeEvent::Reconnecting { .. })));
    }

    #[tokio::test]
    async fn listen_retries_ready_error_sessions_beyond_failure_limit() {
        use async_trait::async_trait;
        use f365_auth::{AuthOp, AuthenticatedExecutor, ExecError};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::SystemTime;

        struct ScriptedExec(AtomicUsize);
        #[async_trait]
        impl AuthenticatedExecutor for ScriptedExec {
            async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
                let call = self.0.fetch_add(1, Ordering::SeqCst);
                // Two ready sessions with failures in between and more than eight afterward.
                let (status, body) = match call {
                    0 | 5 => (
                        200,
                        r#"{"socketio":"https://test.trouter.teams.microsoft.com/","surl":"synthetic","connectparams":{}}"#,
                    ),
                    1 | 6 => (200, "sid:40:60:websocket"),
                    2 | 7 => (200, "{}"),
                    3 | 4 | 8..=16 => (503, ""),
                    _ => std::future::pending().await,
                };
                assert_eq!(op.resource, f365_auth::Resource::Trouter);
                Ok(HttpResponse {
                    status,
                    headers: vec![],
                    body: body.as_bytes().to_vec(),
                })
            }
        }
        #[derive(Default)]
        struct RetryClock(Mutex<Vec<Duration>>, AtomicBool);
        #[async_trait]
        impl Clock for RetryClock {
            fn now(&self) -> Instant {
                Instant::now()
            }
            fn utc_now(&self) -> SystemTime {
                SystemTime::UNIX_EPOCH
            }
            async fn sleep(&self, duration: Duration) {
                // Confirmation and keep-alive are driven by the local socket instead.
                if self.1.swap(false, Ordering::SeqCst) {
                    self.0.lock().unwrap().push(duration);
                } else {
                    std::future::pending::<()>().await;
                }
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(tcp).await.unwrap();
                ws.send(Message::text("1::")).await.unwrap();
                ws.send(Message::text("7:::error")).await.unwrap();
            }
        });
        let exec = Arc::new(ScriptedExec(AtomicUsize::new(0)));
        let mut client = TeamsClient::new(exec.clone());
        client.realtime_test_url = Some(url);
        let clock = RetryClock::default();
        let mut events = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client.listen(&clock, &mut |event| {
                if matches!(event, RealtimeEvent::Reconnecting { .. }) {
                    clock.1.store(true, Ordering::SeqCst);
                }
                events.push(event)
            }),
        )
        .await;
        server.await.unwrap();
        assert!(result.is_err(), "listen must continue after nine failures");
        assert_eq!(exec.0.load(Ordering::SeqCst), 18);
        assert_eq!(
            events
                .iter()
                .filter(|e| **e == RealtimeEvent::Ready)
                .count(),
            2
        );
        let retries: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                RealtimeEvent::Reconnecting { attempt, after } => Some((*attempt, after.as_secs())),
                _ => None,
            })
            .collect();
        assert_eq!(
            retries,
            [
                (1, 2),
                (2, 4),
                (3, 8),
                (1, 2),
                (2, 4),
                (3, 8),
                (4, 16),
                (5, 32),
                (6, 60),
                (7, 60),
                (8, 60),
                (9, 60),
                (10, 60)
            ]
        );
        assert_eq!(
            *clock.0.lock().unwrap(),
            retries
                .iter()
                .map(|(_, secs)| Duration::from_secs(*secs))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn retry_count_resets_after_ready_even_when_session_errors() {
        let mut count = 0;
        for _ in 0..3 {
            count = next_failure_count(count, false, false);
        }
        assert_eq!(count, 3);
        count = next_failure_count(count, false, true);
        assert_eq!(count, 1);
        count = next_failure_count(count, false, false);
        assert_eq!(count, 2);
        count = next_failure_count(count, true, true);
        assert_eq!(count, 1);
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            count = next_failure_count(count, false, false);
        }
        assert!(count > MAX_CONSECUTIVE_FAILURES);
        assert_eq!(next_failure_count(u32::MAX, false, false), u32::MAX);
    }

    #[test]
    fn http_error_log_excludes_body() {
        use std::io::Write;
        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let output = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || Writer(output.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let resp = HttpResponse {
                status: 400,
                headers: vec![],
                body: br#"{"sig":"SYNTHETIC_SECRET","unknown":"PRIVATE_TEXT"}"#.to_vec(),
            };
            assert!(ensure_2xx("test", &resp).is_err());
        });
        let log = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(log.contains("400"), "{log}");
        assert!(
            !log.contains("SYNTHETIC_SECRET") && !log.contains("PRIVATE_TEXT"),
            "{log}"
        );
    }

    /// Servidor WebSocket local que segue um roteiro e devolve o que o cliente escreveu.
    async fn scripted_server(
        script: Vec<&'static str>,
        expect_replies: usize,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            let mut got = Vec::new();
            for frame in script {
                ws.send(Message::text(frame)).await.unwrap();
            }
            while got.len() < expect_replies {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => got.push(t.as_str().to_owned()),
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            let _ = ws.close(None).await;
            got
        });
        (url, task)
    }

    #[tokio::test]
    async fn session_answers_heartbeat_and_messages_and_reports_invalidation_only() {
        let (url, server) = scripted_server(
            vec![
                "2::",
                r#"3:::{"id":77,"method":"POST","url":"/v4/f/x/messaging","body":"{\"resourceLink\":\"https://amer.ng.msg.teams.microsoft.com/v1/users/ME/conversations/19:abc@thread.v2/messages/1\",\"content\":\"texto privado\"}"}"#,
                r#"5:1+::{"name":"trouter.connected","args":[]}"#,
            ],
            2,
        )
        .await;
        let (mut ws, _) = connect_async(url).await.unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut on_event = move |e: RealtimeEvent| sink.lock().unwrap().push(e);
        let clock = ManualClock::default();

        run_session(
            &mut ws,
            &clock,
            Some(Duration::from_secs(40)),
            &mut on_event,
        )
        .await
        .unwrap();

        let replies = server.await.unwrap();
        assert_eq!(replies[0], "2::", "heartbeat respondido");
        assert_eq!(
            replies[1], r#"3:::{"body":"","headers":{},"id":77,"status":200}"#,
            "toda mensagem é respondida"
        );
        let events = events.lock().unwrap();
        assert!(events.contains(&RealtimeEvent::Changed {
            conversation: Some("19:abc@thread.v2".into())
        }));
        assert!(events.contains(&RealtimeEvent::Signal("trouter.connected".into())));
        assert!(
            !format!("{events:?}").contains("texto privado"),
            "conteúdo nunca sai da sessão"
        );
    }

    #[tokio::test]
    async fn heartbeat_extends_silence_deadline_but_sent_frames_do_not() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (send_heartbeat, receive_heartbeat) = tokio::sync::oneshot::channel::<()>();
        let (ack, acked) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            receive_heartbeat.await.unwrap();
            ws.send(Message::text("2::")).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if msg == Message::text("2::") {
                    let _ = ack.send(());
                    break;
                }
            }
            std::future::pending::<()>().await;
        });
        let (mut ws, _) = connect_async(url).await.unwrap();
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let mut session = tokio::spawn(async move {
            run_session(&mut ws, &ticker, Some(Duration::from_secs(40)), &mut |_| {}).await
        });
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }
        clock.advance(Duration::from_secs(30));
        send_heartbeat.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), acked)
            .await
            .unwrap()
            .unwrap();
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }
        clock.advance(Duration::from_secs(50));
        tokio::task::yield_now().await;
        assert!(!session.is_finished());
        clock.advance(Duration::from_secs(30));
        let err = tokio::time::timeout(Duration::from_secs(2), &mut session)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("sem frames do servidor há 80s"));
        server.abort();
    }

    #[tokio::test]
    async fn missing_or_zero_heartbeat_uses_default_silence_deadline() {
        for heartbeat in [None, Some(Duration::ZERO)] {
            let (url, server) = scripted_server(vec![], 100).await;
            let (mut ws, _) = connect_async(url).await.unwrap();
            let clock = ManualClock::default();
            let ticker = clock.clone();
            let mut session =
                tokio::spawn(
                    async move { run_session(&mut ws, &ticker, heartbeat, &mut |_| {}).await },
                );
            while clock.pending_sleepers() == 0 {
                tokio::task::yield_now().await;
            }
            clock.advance(Duration::from_secs(75));
            let err = tokio::time::timeout(Duration::from_secs(2), &mut session)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(err.to_string().contains("sem frames do servidor há 75s"));
            server.abort();
        }
    }

    #[tokio::test]
    async fn replies_are_written_in_receive_order() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket {
            input: tokio::sync::mpsc::UnboundedReceiver<Message>,
            output: tokio::sync::mpsc::UnboundedSender<Message>,
        }
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                self.input.poll_recv(cx).map(|x| x.map(Ok))
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Ready(Ok(()))
            }
            fn start_send(self: Pin<&mut Self>, msg: Message) -> Result<(), WsError> {
                self.output.send(msg).unwrap();
                Ok(())
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Ready(Ok(()))
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Ready(Ok(()))
            }
        }
        let (tx, input) = tokio::sync::mpsc::unbounded_channel();
        let (output, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let session = tokio::spawn(async move {
            run_session(
                &mut Socket { input, output },
                &ManualClock::default(),
                None,
                &mut |_| {},
            )
            .await
        });
        for id in ["first", "second", "third"] {
            tx.send(Message::text(format!("3:::{{\"id\":\"{id}\"}}")))
                .unwrap();
        }
        for id in ["first", "second", "third"] {
            let reply = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(reply
                .to_text()
                .unwrap()
                .contains(&format!("\"id\":\"{id}\"")));
        }
        drop(tx);
        assert!(session.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn blocked_writer_enforces_aggregate_bytes_including_inflight() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket(usize);
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                self.0 += 1;
                if self.0 > 16 {
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(Message::text(format!(
                    "3:::{{\"id\":\"{}\"}}",
                    "x".repeat(32 * 1024)
                )))))
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                unreachable!()
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        let mut socket = Socket(0);
        let result = run_session(&mut socket, &ManualClock::default(), None, &mut |_| {}).await;
        assert!(matches!(result, Err(TeamsError::Realtime(s)) if s.contains("orçamento")));
        assert_eq!(socket.0, 8, "the in-flight message must consume the budget");
    }

    #[tokio::test]
    async fn blocked_writer_reports_queue_saturation() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket;
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Ready(Some(Ok(Message::text("2::"))))
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                unreachable!()
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        let result = run_session(&mut Socket, &ManualClock::default(), None, &mut |_| {}).await;
        assert!(matches!(result, Err(TeamsError::Realtime(s)) if s.contains("fila de escrita")));
    }

    #[tokio::test]
    async fn session_end_reports_unwritten_reply() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket(bool);
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.0 {
                    Poll::Ready(None)
                } else {
                    self.0 = true;
                    Poll::Ready(Some(Ok(Message::text(r#"3:::{"id":"synthetic"}"#))))
                }
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                unreachable!()
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        let result = run_session(
            &mut Socket(false),
            &ManualClock::default(),
            None,
            &mut |_| {},
        )
        .await;
        assert!(
            matches!(result, Err(TeamsError::Realtime(_))),
            "session ended with an unwritten reply"
        );
    }

    #[tokio::test]
    async fn oversized_reply_is_rejected_instead_of_buffered() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket(Option<Message>);
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                Poll::Ready(self.0.take().map(Ok))
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                unreachable!()
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        let input = format!("5:::{{\"name\":\"{}\"}}", "x".repeat(MAX_INCOMING));
        let result = run_session(
            &mut Socket(Some(Message::text(input))),
            &ManualClock::default(),
            None,
            &mut |_| {},
        )
        .await;
        assert!(matches!(result, Err(TeamsError::Realtime(s)) if s.contains("entrada do socket")));
        for id in [
            format!("\"{}\"", "x".repeat(70_000)),
            format!("[\"{}\"]", "x".repeat(70_000)),
        ] {
            let input = format!("3:::{{\"id\":{id}}}");
            let result = run_session(
                &mut Socket(Some(Message::text(input))),
                &ManualClock::default(),
                None,
                &mut |_| {},
            )
            .await;
            assert!(matches!(result, Err(TeamsError::Realtime(s)) if s.contains("id excede")));
        }
    }

    #[tokio::test]
    async fn pending_flush_after_start_send_times_out() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket {
            rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
            flush: Option<tokio::sync::oneshot::Sender<()>>,
        }
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                self.rx.poll_recv(cx).map(|x| x.map(Ok))
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Ready(Ok(()))
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                Ok(())
            }
            fn poll_flush(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), WsError>> {
                if let Some(tx) = self.flush.take() {
                    let _ = tx.send(());
                }
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (notify, wait) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut socket = Socket {
                rx,
                flush: Some(notify),
            };
            run_session(
                &mut socket,
                &ticker,
                Some(Duration::from_secs(1000)),
                &mut |_| {},
            )
            .await
        });
        tx.send(Message::text("2::")).unwrap();
        wait.await.unwrap();
        clock.advance(Duration::from_secs(59));
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "must not expire before 60s");
        clock.advance(Duration::from_secs(1));
        let error = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(error.contains("tempo esgotado na escrita"), "{error}");
        assert_eq!(clock.pending_sleepers(), 0);
    }

    #[tokio::test]
    async fn stalled_write_times_out_despite_active_incoming_frames() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket {
            rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
            blocked: Option<tokio::sync::oneshot::Sender<()>>,
        }
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                self.rx.poll_recv(cx).map(|m| m.map(Ok))
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), WsError>> {
                if let Some(tx) = self.blocked.take() {
                    let _ = tx.send(());
                }
                Poll::Pending
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                unreachable!()
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (blocked, blocked_rx) = tokio::sync::oneshot::channel();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let mut session = tokio::spawn(async move {
            run_session(
                &mut Socket {
                    rx,
                    blocked: Some(blocked),
                },
                &ticker,
                Some(Duration::from_secs(40)),
                &mut |e| {
                    let _ = events.send(e);
                },
            )
            .await
        });
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }
        clock.advance(Duration::from_secs(35));
        blocked_rx.await.unwrap();
        for _ in 0..20 {
            let _ = tx.send(Message::text(
                r#"5:::{"name":"trouter.connected","args":[]}"#,
            ));
            tokio::select! {
                result = &mut session => {
                    assert!(matches!(result.unwrap(), Err(TeamsError::Realtime(s)) if s.contains("escrita") && s.contains("tempo")));
                    return;
                }
                event = received.recv() => {
                    if event.is_none() {
                        assert!(matches!(session.await.unwrap(), Err(TeamsError::Realtime(s)) if s.contains("escrita") && s.contains("tempo")));
                        return;
                    }
                }
            }
            clock.advance(Duration::from_secs(30));
        }
        session.abort();
        panic!("write stalled for 600 seconds despite active frames");
    }

    #[tokio::test]
    async fn reads_server_frames_while_keepalive_write_is_blocked() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Socket {
            rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
            blocked: Option<tokio::sync::oneshot::Sender<()>>,
            block_flush: bool,
        }
        impl Stream for Socket {
            type Item = Result<Message, WsError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                self.rx.poll_recv(cx).map(|m| m.map(Ok))
            }
        }
        impl Sink<Message> for Socket {
            type Error = WsError;
            fn poll_ready(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), WsError>> {
                if self.block_flush {
                    return Poll::Ready(Ok(()));
                }
                if let Some(tx) = self.blocked.take() {
                    let _ = tx.send(());
                }
                Poll::Pending
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                Ok(())
            }
            fn poll_flush(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), WsError>> {
                if let Some(tx) = self.blocked.take() {
                    let _ = tx.send(());
                }
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        for block_flush in [false, true] {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (blocked, blocked_rx) = tokio::sync::oneshot::channel();
            let clock = ManualClock::default();
            let ticker = clock.clone();
            let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
            let session = tokio::spawn(async move {
                run_session(
                    &mut Socket {
                        rx,
                        blocked: Some(blocked),
                        block_flush,
                    },
                    &ticker,
                    Some(Duration::from_secs(40)),
                    &mut |event| {
                        let _ = events.send(event);
                    },
                )
                .await
            });
            while clock.pending_sleepers() == 0 {
                tokio::task::yield_now().await;
            }
            clock.advance(Duration::from_secs(35));
            tokio::time::timeout(Duration::from_secs(2), blocked_rx)
                .await
                .unwrap()
                .unwrap();
            clock.advance(Duration::from_secs(10));
            tx.send(Message::text(
                r#"5:::{"name":"trouter.connected","args":[]}"#,
            ))
            .unwrap();
            tokio::time::timeout(Duration::from_millis(250), received.recv())
                .await
                .expect("must read events during blocked writes")
                .unwrap();
            clock.advance(Duration::from_secs(35));
            tokio::task::yield_now().await;
            assert!(
                !session.is_finished(),
                "frame at 45s must extend deadline past 80s"
            );
            session.abort();
            assert!(session.await.unwrap_err().is_cancelled());
            assert_eq!(clock.pending_sleepers(), 0);
            assert!(tx.is_closed(), "cancel must release the reader and writer");
        }
    }

    #[tokio::test]
    async fn continuously_ready_frames_cannot_starve_keepalive() {
        use std::{
            pin::Pin,
            sync::atomic::{AtomicUsize, Ordering},
            task::{Context, Poll},
        };
        struct Busy(Arc<AtomicUsize>);
        impl Stream for Busy {
            type Item = Result<Message, WsError>;
            fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                if self.0.load(Ordering::SeqCst) > 0 {
                    Poll::Ready(Some(Ok(Message::Close(None))))
                } else {
                    Poll::Ready(Some(Ok(Message::text(
                        r#"5:::{"name":"trouter.connected","args":[]}"#,
                    ))))
                }
            }
        }
        impl Sink<Message> for Busy {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Ready(Ok(()))
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Ready(Ok(()))
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Ready(Ok(()))
            }
        }
        let clock = ManualClock::default();
        let writes = Arc::new(AtomicUsize::new(0));
        let mut events = 0;
        run_session(
            &mut Busy(writes.clone()),
            &clock,
            Some(Duration::from_secs(40)),
            &mut |_| {
                events += 1;
                if events == 1 {
                    clock.advance(Duration::from_secs(35));
                }
                assert!(events < 100, "keepalive starved by ready stream");
            },
        )
        .await
        .unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn blocked_keepalive_write_cannot_hide_silence() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        struct Blocked;
        impl Stream for Blocked {
            type Item = Result<Message, WsError>;
            fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Pending
            }
        }
        impl Sink<Message> for Blocked {
            type Error = WsError;
            fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
                Ok(())
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
            fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
                Poll::Pending
            }
        }
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let mut session = tokio::spawn(async move {
            run_session(
                &mut Blocked,
                &ticker,
                Some(Duration::from_secs(40)),
                &mut |_| {},
            )
            .await
        });
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }
        clock.advance(Duration::from_secs(35));
        tokio::task::yield_now().await;
        clock.advance(Duration::from_secs(45));
        let result = tokio::time::timeout(Duration::from_millis(250), &mut session).await;
        assert!(result
            .expect("blocked write must respect silence deadline")
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("sem frames do servidor"));
    }

    #[tokio::test]
    async fn unrepresentable_heartbeat_is_rejected_without_panicking() {
        let (url, server) = scripted_server(vec![], 100).await;
        let (mut ws, _) = connect_async(url).await.unwrap();
        let result = run_session(
            &mut ws,
            &ManualClock::default(),
            Some(Duration::from_secs(u64::MAX)),
            &mut |_| {},
        )
        .await;
        server.abort();
        assert!(matches!(result, Err(TeamsError::Realtime(_))));
    }

    #[tokio::test]
    async fn silent_server_times_out_at_deadline() {
        let (url, server) = scripted_server(vec![], 100).await;
        let (mut ws, _) = connect_async(url).await.unwrap();
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let mut session = tokio::spawn(async move {
            run_session(&mut ws, &ticker, Some(Duration::from_secs(40)), &mut |_| {}).await
        });
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }
        clock.advance(Duration::from_secs(80));
        let result = tokio::time::timeout(Duration::from_secs(1), &mut session).await;
        server.abort();
        let err = result
            .expect("deadline must terminate session")
            .unwrap()
            .unwrap_err();
        assert!(
            err.to_string().contains("sem frames do servidor há 80s"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn frequent_server_frames_do_not_postpone_keepalive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            while rx.recv().await.is_some() {
                ws.send(Message::text(
                    r#"5:::{"name":"trouter.connected","args":[]}"#,
                ))
                .await
                .unwrap();
            }
            let msg = ws.next().await.unwrap().unwrap();
            reply_tx.send(msg).unwrap();
        });
        let (mut ws, _) = connect_async(url).await.unwrap();
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let session = tokio::spawn(async move {
            run_session(
                &mut ws,
                &ticker,
                Some(Duration::from_secs(40)),
                &mut |event| {
                    let _ = events_tx.send(event);
                },
            )
            .await
        });
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }
        for _ in 0..3 {
            clock.advance(Duration::from_secs(10));
            tx.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
                .await
                .unwrap()
                .unwrap();
        }
        drop(tx);
        clock.advance(Duration::from_secs(5));
        let reply = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("keepalive must fire at original 35s deadline")
            .unwrap();
        assert_eq!(reply, Message::text("2::"));
        server.await.unwrap();
        session.abort();
    }

    #[tokio::test]
    async fn keepalive_fires_on_an_absolute_schedule() {
        let (url, server) = scripted_server(vec![], 1).await;
        let (mut ws, _) = connect_async(url).await.unwrap();
        let clock = ManualClock::default();
        let ticker = clock.clone();
        let session = tokio::spawn(async move {
            let mut ignore = |_e: RealtimeEvent| {};
            run_session(&mut ws, &ticker, Some(Duration::from_secs(40)), &mut ignore).await
        });
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }
        clock.advance(Duration::from_secs(35)); // heartbeat 40 s − 5 s de folga
        let replies = server.await.unwrap();
        assert_eq!(replies, ["2::"]);
        session.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connect_confirmation_and_its_deadline() {
        let (url, _server) = scripted_server(vec!["1::"], 0).await;
        let (mut ws, _) = connect_async(url).await.unwrap();
        await_connect(&mut ws, &ManualClock::default())
            .await
            .unwrap();

        let (url, _server) = scripted_server(vec!["7:::x"], 0).await;
        let (mut ws, _) = connect_async(url).await.unwrap();
        let err = await_connect(&mut ws, &ManualClock::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("TROUTER-WS-001"), "{err}");
    }
}
