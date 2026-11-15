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
/// Keep-alive quando o handshake não informa o heartbeat do servidor.
const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(30);
#[cfg(test)]
const MAX_CONSECUTIVE_FAILURES: u32 = 8;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

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
        let mut info = trouter::parse_connect(&resp.body)?;
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
        let hs = std::str::from_utf8(&resp.body)
            .ok()
            .and_then(socketio::parse_handshake)
            .filter(|h| h.transports.iter().any(|t| t == "websocket"))
            .ok_or(TeamsError::Malformed(
                "Trouter: handshake socket.io inesperado (TROUTER-HANDSHAKE-001)",
            ))?;

        on_event(RealtimeEvent::Step("3/4 WebSocket"));
        let ws_url = trouter::ws_url(&info, &hs.session_id, &epid);
        #[cfg(test)]
        let ws_url = self.realtime_test_url.clone().unwrap_or(ws_url);
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| {
                TeamsError::Realtime(format!(
                    "WebSocket recusado: {} (TROUTER-WS-001)",
                    ws_error_kind(&e)
                ))
            })?;
        await_connect(&mut ws, clock).await?;

        on_event(RealtimeEvent::Step("4/4 registro do endpoint"));
        let resp = self
            .exec
            .execute(trouter::registrar_op(&info, &epid))
            .await?;
        ensure_2xx("Trouter (registrador, TROUTER-REGISTRAR-001)", &resp)?;

        *ever_ready = true;
        on_event(RealtimeEvent::Ready);
        run_session(&mut ws, clock, hs.heartbeat, on_event).await
    }
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
    let mut next_keepalive: Instant = clock.now() + every;
    let silence = heartbeat
        .filter(|h| !h.is_zero())
        .map_or(Duration::from_secs(75), |h| h.saturating_mul(2));
    let mut last_received = clock.now();
    let send_err = |e: WsError| {
        TeamsError::Realtime(format!(
            "falha ao escrever no socket: {}",
            ws_error_kind(&e)
        ))
    };

    loop {
        let now = clock.now();
        if now.saturating_duration_since(last_received) >= silence {
            return Err(TeamsError::Realtime(format!(
                "sem frames do servidor há {}s — conexão considerada morta",
                now.saturating_duration_since(last_received).as_secs()
            )));
        }
        let wait = next_keepalive
            .saturating_duration_since(now)
            .min(silence.saturating_sub(now.saturating_duration_since(last_received)));
        // Decide primeiro, age depois: a leitura empresta o socket até o `select!` terminar.
        let incoming = tokio::select! {
            m = ws.next() => Some(m),
            () = clock.sleep(wait) => None,
        };
        let Some(incoming) = incoming else {
            if clock.now().saturating_duration_since(last_received) >= silence {
                continue;
            }
            ws.send(Message::text(socketio::encode(&Frame::Heartbeat)))
                .await
                .map_err(send_err)?;
            next_keepalive = clock.now() + every;
            continue;
        };
        if matches!(incoming, Some(Ok(_))) {
            last_received = clock.now();
        }
        let text = match incoming {
            None | Some(Ok(Message::Close(_))) => return Ok(()),
            Some(Err(e)) => {
                return Err(TeamsError::Realtime(format!(
                    "socket caiu: {}",
                    ws_error_kind(&e)
                )))
            }
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(_)) => continue,
        };
        match socketio::parse(text.as_str()) {
            Some(Frame::Heartbeat) => {
                ws.send(Message::text(socketio::encode(&Frame::Heartbeat)))
                    .await
                    .map_err(send_err)?;
            }
            Some(Frame::Message { data, .. }) => {
                let Ok(msg) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if let Some(reply) = trouter::response_for(&msg) {
                    ws.send(Message::text(reply)).await.map_err(send_err)?;
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
            Some(Frame::Disconnect) => return Ok(()),
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
