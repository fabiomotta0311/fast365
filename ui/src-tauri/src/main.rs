//! Shell Tauri do Fast365 (Fase 3, #19).
//!
//! Casca fina de propósito: cada comando é um pedido ao **ator do core**, que é o único dono do
//! runtime. O ator espera, ao mesmo tempo, por (a) acontecimento do tempo real, (b) prazo do motor
//! e (c) comando do frontend — e só executa I/O fora do `select!`, então nenhum pedido cancela uma
//! operação em voo.
//!
//! A UI nunca faz rede: ela lê o cache e chama comandos. Quem empurra novidades é o evento
//! `f365://round` (rodada do motor) e `f365://realtime` (estado do canal).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use f365_auth::executor::SessionExecutor;
use f365_auth::tokens::TokenBroker;
use f365_auth::transport::RustTransport;
use f365_auth::vault::Vault;
use f365_auth::{AuthenticatedExecutor, Clock, SystemClock};
use f365_driver::Driver;
use f365_ipc::{ConversationDto, MessageDto, RoundDto, Shell};
use f365_storage::Store;
use f365_ui::{channel, App, Inbound, PumpHandle, RealtimeLink};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, oneshot};

/// Teto da fila de pedidos: o frontend é síncrono por gesto; 64 é folga de sobra.
const REQUEST_QUEUE: usize = 64;

type Reply<T> = oneshot::Sender<Result<T, String>>;

/// Pedido do frontend ao dono do core.
enum Request {
    Conversations {
        limit: usize,
        reply: Reply<Vec<ConversationDto>>,
    },
    Messages {
        conversation_id: String,
        before_ms: Option<i64>,
        limit: usize,
        reply: Reply<Vec<MessageDto>>,
    },
    Search {
        query: String,
        limit: usize,
        reply: Reply<Vec<MessageDto>>,
    },
    Capabilities {
        reply: Reply<Vec<String>>,
    },
    Refresh {
        reply: Reply<RoundDto>,
    },
    Send {
        conversation_id: String,
        text: String,
        reply: Reply<RoundDto>,
    },
    /// Abrir uma conversa: busca o histórico no provider (o cache é só o primeiro frame).
    Load {
        conversation_id: String,
        reply: Reply<RoundDto>,
    },
}

/// Alça que os comandos usam para falar com o ator.
struct CoreHandle {
    tx: mpsc::Sender<Request>,
}

impl CoreHandle {
    async fn ask<T>(&self, build: impl FnOnce(Reply<T>) -> Request) -> Result<T, String> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(build(reply))
            .await
            .map_err(|_| "core encerrado".to_string())?;
        answer.await.map_err(|_| "core encerrado".to_string())?
    }
}

#[derive(Clone, Serialize)]
struct RealtimePayload {
    connected: bool,
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            use tauri::Manager as _;

            let (tx, rx) = mpsc::channel(REQUEST_QUEUE);
            app.manage(CoreHandle { tx });
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move { run_core(handle, rx).await });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            conversations,
            messages,
            search,
            capabilities,
            refresh,
            send,
            load
        ])
        .run(tauri::generate_context!())
        .expect("erro ao iniciar o Fast365");
}

/// Ator do core: dono único do runtime.
async fn run_core(app: AppHandle, mut requests: mpsc::Receiver<Request>) {
    let (pump, mut inbox) = channel();

    let mut core = match build_core(pump) {
        Ok(core) => core,
        // Falhar ao montar o core **não** fecha a janela: ela abre para dizer o que fazer.
        Err(why) => return serve_failing(&mut requests, why).await,
    };

    // Busca fresca na abertura: sem isso a janela abriria só com o cache, e o nome de um 1:1 só
    // apareceria ao clicar na conversa. Falhar aqui não é fatal — a UI mostra as capacidades.
    if let Ok(round) = core.refresh_conversations().await {
        emit_round(&app, &round);
    }

    // Sobe o tempo real (o `RealtimeLink` inicia o laço do Trouter e alimenta a `pump`).
    if let Ok(round) = core.connection_restored().await {
        emit_round(&app, &round);
    }

    loop {
        let wake = core.wake_at();
        tokio::select! {
            inbound = inbox.recv() => {
                let Some(inbound) = inbound else { break };
                match inbound {
                    Inbound::Realtime { connected } => {
                        let _ = app.emit("f365://realtime", RealtimePayload { connected });
                    }
                    inbound => {
                        // Fora do `select!`: aplicar pode fazer I/O e não pode ser cancelado.
                        if let Ok(round) = core.apply(inbound).await {
                            emit_round(&app, &round);
                        }
                    }
                }
            }
            request = requests.recv() => {
                let Some(request) = request else { break };
                serve(&mut core, request).await;
            }
            () = sleep_until(wake) => {
                if let Ok(round) = core.wake().await {
                    emit_round(&app, &round);
                }
            }
        }
    }
}

async fn serve(core: &mut App, request: Request) {
    match request {
        Request::Conversations { limit, reply } => {
            let _ = reply.send(core.conversations(limit).map_err(|error| error.to_string()));
        }
        Request::Messages {
            conversation_id,
            before_ms,
            limit,
            reply,
        } => {
            let _ = reply.send(
                core.messages(&conversation_id, before_ms, limit)
                    .map_err(|error| error.to_string()),
            );
        }
        Request::Search {
            query,
            limit,
            reply,
        } => {
            let _ = reply.send(
                core.search(&query, limit)
                    .map_err(|error| error.to_string()),
            );
        }
        Request::Capabilities { reply } => {
            let _ = reply.send(Ok(core.capabilities()));
        }
        Request::Refresh { reply } => {
            let _ = reply.send(
                core.refresh_conversations()
                    .await
                    .map_err(|error| error.to_string()),
            );
        }
        Request::Send {
            conversation_id,
            text,
            reply,
        } => {
            let _ = reply.send(
                core.send(&conversation_id, &text)
                    .await
                    .map_err(|error| error.to_string()),
            );
        }
        Request::Load {
            conversation_id,
            reply,
        } => {
            // Invalidação é o que o motor entende por "busque esta conversa".
            let inbound = Inbound::Invalidated {
                conversation: conversation_id,
            };
            let _ = reply.send(core.apply(inbound).await.map_err(|error| error.to_string()));
        }
    }
}

/// Modo degradado: o core não subiu, então todo pedido recebe o motivo.
async fn serve_failing(requests: &mut mpsc::Receiver<Request>, why: String) {
    while let Some(request) = requests.recv().await {
        match request {
            Request::Conversations { reply, .. } => {
                let _ = reply.send(Err(why.clone()));
            }
            Request::Messages { reply, .. } => {
                let _ = reply.send(Err(why.clone()));
            }
            Request::Search { reply, .. } => {
                let _ = reply.send(Err(why.clone()));
            }
            Request::Capabilities { reply } => {
                let _ = reply.send(Err(why.clone()));
            }
            Request::Refresh { reply } => {
                let _ = reply.send(Err(why.clone()));
            }
            Request::Send { reply, .. } => {
                let _ = reply.send(Err(why.clone()));
            }
            Request::Load { reply, .. } => {
                let _ = reply.send(Err(why.clone()));
            }
        }
    }
}

/// Monta o core: cache, executor autenticado, motor (com tempo real) e runtime.
///
/// O broker vai **sem** re-pareamento automático nesta casca (a feature `webview` não é compilada
/// aqui): quando o RT de SPA vence, a capacidade vira `NeedsAuth` e a UI pede `f365 auth` ao dono.
fn build_core(pump: PumpHandle) -> Result<App, String> {
    let db = db_path()?;
    let store = Store::open(&db).map_err(|error| format!("cache: {error}"))?;

    let transport = Arc::new(RustTransport::new().map_err(|error| format!("transporte: {error}"))?);
    let broker = Arc::new(TokenBroker::new(
        Vault::keyring(),
        transport.clone(),
        Arc::new(SystemClock),
    ));
    let exec: Arc<dyn AuthenticatedExecutor> = Arc::new(SessionExecutor::new(
        broker,
        transport,
        None,
        Arc::new(SystemClock),
    ));

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let realtime = Arc::new(RealtimeLink::new(exec.clone(), pump));
    let driver = Driver::new(exec, store, clock.clone())
        .with_top(50)
        .with_realtime(realtime);
    // Nome dos 1:1 já no primeiro frame: usa o cache, sem rede.
    let _ = driver.repair_titles();
    let shell = Shell::new(driver, clock.clone());
    Ok(App::new(shell, clock))
}

fn db_path() -> Result<PathBuf, String> {
    let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA não definido")?;
    let dir = std::path::Path::new(&local).join("fast365");
    std::fs::create_dir_all(&dir).map_err(|error| format!("criando {}: {error}", dir.display()))?;
    Ok(dir.join("f365-cache.db"))
}

fn emit_round(app: &AppHandle, round: &RoundDto) {
    let _ = app.emit("f365://round", round);
}

/// Dorme até o prazo (ou para sempre, se não há prazo). Piso curto evita laço quente.
async fn sleep_until(at: Option<Instant>) {
    let Some(at) = at else {
        std::future::pending::<()>().await;
        return;
    };
    let wait = at
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(50));
    tokio::time::sleep(wait).await
}

// ---- comandos (casca fina: um pedido ao ator) ----

#[tauri::command]
async fn conversations(
    core: tauri::State<'_, CoreHandle>,
    limit: usize,
) -> Result<Vec<ConversationDto>, String> {
    core.ask(|reply| Request::Conversations { limit, reply }).await
}

#[tauri::command]
async fn messages(
    core: tauri::State<'_, CoreHandle>,
    conversation_id: String,
    before_ms: Option<i64>,
    limit: usize,
) -> Result<Vec<MessageDto>, String> {
    core.ask(|reply| Request::Messages {
        conversation_id,
        before_ms,
        limit,
        reply,
    })
    .await
}

#[tauri::command]
async fn search(
    core: tauri::State<'_, CoreHandle>,
    query: String,
    limit: usize,
) -> Result<Vec<MessageDto>, String> {
    core.ask(|reply| Request::Search {
        query,
        limit,
        reply,
    })
    .await
}

#[tauri::command]
async fn capabilities(core: tauri::State<'_, CoreHandle>) -> Result<Vec<String>, String> {
    core.ask(|reply| Request::Capabilities { reply }).await
}

#[tauri::command]
async fn refresh(core: tauri::State<'_, CoreHandle>) -> Result<RoundDto, String> {
    core.ask(|reply| Request::Refresh { reply }).await
}

#[tauri::command]
async fn send(
    core: tauri::State<'_, CoreHandle>,
    conversation_id: String,
    text: String,
) -> Result<RoundDto, String> {
    core.ask(|reply| Request::Send {
        conversation_id,
        text,
        reply,
    })
    .await
}

#[tauri::command]
async fn load(
    core: tauri::State<'_, CoreHandle>,
    conversation_id: String,
) -> Result<RoundDto, String> {
    core.ask(|reply| Request::Load {
        conversation_id,
        reply,
    })
    .await
}
