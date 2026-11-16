//! `f365 listen` (Checkpoint D): mostra cada passo da conexão ao Trouter e, depois, uma linha por
//! conversa alterada — nunca o conteúdo (evento = invalidação). Ctrl+C encerra.

use f365_auth::{Clock, SystemClock};
use f365_teams::HistoryMessage;
use f365_teams::{RealtimeEvent, TeamsClient};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, SyncSender},
    Arc,
};

fn safe(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}
#[derive(Clone)]
struct Output {
    tx: SyncSender<String>,
    dropped: Arc<AtomicUsize>,
    history_dropped: Arc<AtomicUsize>,
}
impl Output {
    fn send(&self, line: String) {
        if self.tx.try_send(line).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn drain_output(
    rx: mpsc::Receiver<String>,
    dropped: Arc<AtomicUsize>,
    history_dropped: Arc<AtomicUsize>,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    loop {
        let ended = match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(line) => {
                writeln!(out, "{line}")?;
                false
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => true,
        };
        let n = dropped.swap(0, Ordering::Relaxed);
        if n > 0 {
            writeln!(out, "Aviso: {n} linhas descartadas: saída congestionada.")?;
        }
        let n = history_dropped.swap(0, Ordering::Relaxed);
        if n > 0 {
            writeln!(
                out,
                "Aviso: {n} consultas de histórico descartadas: fila cheia, encerrada ou consulta cancelada."
            )?;
        }
        out.flush()?;
        if ended {
            return Ok(());
        }
    }
}
fn output() -> (Output, tokio::sync::oneshot::Receiver<std::io::Result<()>>) {
    let (tx, rx) = mpsc::sync_channel::<String>(32);
    let dropped = Arc::new(AtomicUsize::new(0));
    let lost = dropped.clone();
    let history_dropped = Arc::new(AtomicUsize::new(0));
    let lost_history = history_dropped.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    // Never join this thread: stdout can remain blocked forever.
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let result = drain_output(rx, lost, lost_history, &mut stdout.lock());
        let _ = done_tx.send(result);
    });
    (
        Output {
            tx,
            dropped,
            history_dropped,
        },
        done_rx,
    )
}
fn history_line(m: &HistoryMessage, show: bool) -> String {
    let mut line = format!(
        "{} | {} | {} | {}",
        safe(&m.id),
        safe(m.time.as_deref().unwrap_or("?")),
        safe(m.sender.as_deref().unwrap_or("?")),
        safe(m.message_type.as_deref().unwrap_or("?"))
    );
    if show {
        line.push_str(&format!(" | {}", safe(m.content.as_deref().unwrap_or(""))));
    }
    line
}

struct HistoryQueue {
    rx: tokio::sync::mpsc::Receiver<String>,
    active: bool,
    dropped: Arc<AtomicUsize>,
}
impl Drop for HistoryQueue {
    fn drop(&mut self) {
        self.rx.close();
        let lost = self.rx.len() + usize::from(self.active);
        self.dropped.fetch_add(lost, Ordering::Relaxed);
    }
}
fn history_worker<'a, F, Fut, E>(
    rx: tokio::sync::mpsc::Receiver<String>,
    out: &'a Output,
    show_content: bool,
    fetch: F,
) -> impl std::future::Future<Output = ()> + 'a
where
    F: Fn(String) -> Fut + 'a,
    Fut: std::future::Future<Output = Result<Vec<HistoryMessage>, E>>,
{
    let mut queue = HistoryQueue {
        rx,
        active: false,
        dropped: out.history_dropped.clone(),
    };
    async move {
        while let Some(id) = queue.rx.recv().await {
            queue.active = true;
            let conversation = safe(&id);
            match fetch(id).await {
                Ok(messages) => {
                    for message in messages.iter().take(1) {
                        out.send(format!(
                            "{conversation} | {}",
                            history_line(message, show_content)
                        ));
                    }
                }
                Err(_) => out.send(format!(
                    "Aviso: não foi possível consultar histórico da conversa {conversation}."
                )),
            }
            queue.active = false;
        }
    }
}

pub async fn listen(fetch: bool, show_content: bool) -> anyhow::Result<()> {
    let client = TeamsClient::new(Arc::new(crate::chats_cmd::executor()?));
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(32);
    let (out, mut output_done) = output();
    let event_out = out.clone();
    let clock = SystemClock;
    let start = clock.now();
    let mut on_event = move |e: RealtimeEvent| {
        let t = start.elapsed().as_secs_f32();
        match e {
            RealtimeEvent::Step(s) => event_out.send(format!("  {}…", safe(s))),
            RealtimeEvent::Ready => event_out.send(format!("Pronto (+{t:.1}s). Ctrl+C sai.")),
            RealtimeEvent::Changed { conversation } => {
                event_out.send(format!(
                    "[+{t:.1}s] mudança em {}",
                    safe(
                        conversation
                            .as_deref()
                            .unwrap_or("(conversa não identificada)")
                    )
                ));
                if fetch {
                    if let Some(id) = conversation {
                        if tx.try_send(id).is_err() {
                            event_out.history_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            RealtimeEvent::Signal(name) => {
                event_out.send(format!("[+{t:.1}s] sinal do Trouter: {}", safe(&name)))
            }
            RealtimeEvent::Reconnecting { attempt, after } => event_out.send(format!(
                "[+{t:.1}s] conexão caiu — tentativa {attempt} em {}s",
                after.as_secs()
            )),
        }
    };

    out.send("Conectando ao tempo real (Trouter)…".into());
    let history_client = &client;
    let worker = history_worker(rx, &out, show_content, |id| async move {
        history_client.history(&id, 1).await
    });
    let result = tokio::select! {
        r = client.listen(&clock, &mut on_event) => r.map_err(Into::into),
        _ = worker => Ok(()),
        r = &mut output_done => return Err(anyhow::anyhow!("saída encerrada: {:?}", r)),
        r = tokio::signal::ctrl_c() => match r {
            Ok(()) => { out.send("Encerrado.".into()); Ok(()) }
            Err(e) => Err(e.into()),
        }
    };
    drop(on_event);
    drop(out);
    match tokio::time::timeout(std::time::Duration::from_millis(500), output_done).await {
        Ok(Ok(Ok(()))) => result,
        Ok(Ok(Err(e))) => Err(e.into()),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(anyhow::anyhow!(
            "saída bloqueada; linhas pendentes podem ter sido perdidas"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_output() -> (Output, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::sync_channel(8);
        (
            Output {
                tx,
                dropped: Arc::new(AtomicUsize::new(0)),
                history_dropped: Arc::new(AtomicUsize::new(0)),
            },
            rx,
        )
    }
    #[tokio::test]
    async fn history_results_identify_sanitized_conversations() {
        let (out, lines) = test_output();
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send("chat\n1".into()).await.unwrap();
        tx.send("chat\x1b2".into()).await.unwrap();
        drop(tx);
        history_worker(rx, &out, false, |id| async move {
            if id == "chat\n1" {
                Ok(vec![HistoryMessage {
                    id: "msg".into(),
                    time: None,
                    sender: None,
                    message_type: None,
                    content: None,
                }])
            } else {
                Err(())
            }
        })
        .await;
        let lines = lines.try_iter().collect::<Vec<_>>();
        assert!(lines[0].contains("chat\\n1") && lines[0].contains("msg"));
        assert!(lines[1].contains("chat\\u{1b}2") && lines[1].contains("Aviso"));
        assert!(lines.iter().all(|line| !line.chars().any(char::is_control)));
    }
    #[tokio::test]
    async fn history_empty_and_error_have_distinct_output() {
        let (out, lines) = test_output();
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send("empty".into()).await.unwrap();
        tx.send("error".into()).await.unwrap();
        drop(tx);
        history_worker(rx, &out, false, |id| async move {
            if id == "empty" {
                Ok(Vec::new())
            } else {
                Err(())
            }
        })
        .await;
        assert_eq!(
            lines.try_iter().collect::<Vec<_>>(),
            vec!["Aviso: não foi possível consultar histórico da conversa error."]
        );
    }
    #[tokio::test]
    async fn pending_history_does_not_block_other_futures() {
        let (out, _) = test_output();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send("pending".into()).await.unwrap();
        let worker = history_worker(rx, &out, false, |_| async {
            std::future::pending::<Result<Vec<HistoryMessage>, ()>>().await
        });
        tokio::pin!(worker);
        tokio::select! {
            _ = &mut worker => panic!("history unexpectedly completed"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
    #[tokio::test]
    async fn shutdown_counts_inflight_and_queued_history() {
        let (out, _) = test_output();
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send("first".into()).await.unwrap();
        tx.send("second".into()).await.unwrap();
        let mut worker = Box::pin(history_worker(rx, &out, false, |_| async {
            std::future::pending::<Result<Vec<HistoryMessage>, ()>>().await
        }));
        tokio::select! {
            _ = &mut worker => panic!("unexpected completion"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
        drop(worker);
        assert_eq!(out.history_dropped.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn event_data_cannot_inject_terminal_controls() {
        assert_eq!(safe("abc\n\x1b"), "abc\\n\\u{1b}");
    }
    #[test]
    fn congested_output_never_waits() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let out = Output {
            tx,
            dropped: Arc::new(AtomicUsize::new(0)),
            history_dropped: Arc::new(AtomicUsize::new(0)),
        };
        for _ in 0..1000 {
            out.send("line".into());
        }
        assert_eq!(out.dropped.load(Ordering::Relaxed), 999);
    }
    #[test]
    fn final_overflow_is_reported_without_another_message() {
        let (tx, rx) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let history_dropped = Arc::new(AtomicUsize::new(0));
        let out = Output {
            tx,
            dropped: dropped.clone(),
            history_dropped: history_dropped.clone(),
        };
        out.send("first".into());
        out.send("lost".into());
        drop(out);
        let mut written = Vec::new();
        drain_output(rx, dropped, history_dropped, &mut written).unwrap();
        let text = String::from_utf8(written).unwrap();
        assert!(text.contains("first"));
        assert!(text.contains("1 linhas descartadas"));
    }
    #[test]
    fn history_losses_are_reported_even_when_output_is_full() {
        let (tx, rx) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let history_dropped = Arc::new(AtomicUsize::new(2));
        tx.try_send("queued".into()).unwrap();
        drop(tx);
        let mut written = Vec::new();
        drain_output(rx, dropped, history_dropped, &mut written).unwrap();
        assert!(String::from_utf8(written)
            .unwrap()
            .contains("2 consultas de histórico descartadas"));
    }
    #[test]
    fn output_write_errors_are_propagated() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.try_send("line".into()).unwrap();
        drop(tx);
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            drain_output(
                rx,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                &mut Broken
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }
    #[test]
    fn content_is_opt_in_and_fields_are_escaped() {
        let m = HistoryMessage {
            id: "id\n".into(),
            time: Some(
                "t
"
                .into(),
            ),
            sender: Some("s\x1b".into()),
            message_type: Some("k\x07".into()),
            content: Some("secret\n".into()),
        };
        for show in [false, true] {
            let line = history_line(&m, show);
            assert!(!line.chars().any(char::is_control));
            assert_eq!(line.contains("secret"), show);
        }
    }
}
