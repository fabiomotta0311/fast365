//! Shell Tauri do Fast365 (Fase 3, #19).
//!
//! Casca fina de propósito: cada comando delega ao runtime do `f365-ui`, que é quem possui o core.
//! Nada de rede aqui — a UI só lê o cache e pede comandos.
//!
//! O empurrão do tempo real (eventos do Trouter → frontend) ainda não está ligado nesta casca; por
//! ora o frontend chama `tick` num intervalo, que faz o motor processar prazos vencidos
//! (reconciliação/renovação). O laço do `listen` entra no próximo slice.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use f365_auth::executor::SessionExecutor;
use f365_auth::tokens::TokenBroker;
use f365_auth::transport::RustTransport;
use f365_auth::vault::Vault;
use f365_auth::{AuthenticatedExecutor, Clock, SystemClock};
use f365_driver::Driver;
use f365_ipc::{ConversationDto, MessageDto, RoundDto, Shell};
use f365_storage::Store;
use f365_ui::App;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// O core pode estar pronto ou ter falhado na inicialização (ex.: sem sessão pareada).
///
/// Falhar aqui **não** fecha a janela: a UI precisa abrir para dizer o que fazer.
enum Core {
    Ready(App),
    Failed(String),
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            use tauri::Manager;
            app.manage(Mutex::new(Core::Failed("iniciando…".into())));

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use tauri::Manager as _;
                let core = match build_core() {
                    Ok(app) => Core::Ready(app),
                    Err(why) => Core::Failed(why),
                };
                let state = handle.state::<Mutex<Core>>();
                *state.lock().await = core;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            conversations,
            messages,
            search,
            capabilities,
            refresh,
            send,
            tick
        ])
        .run(tauri::generate_context!())
        .expect("erro ao iniciar o Fast365");
}

/// Monta o core: cache, executor autenticado, motor e runtime.
///
/// O broker vai **sem** re-pareamento automático nesta casca (o `webview` não está compilado aqui):
/// quando o RT de SPA vence, a capacidade vira `NeedsAuth` e a UI pede `f365 auth` ao dono.
fn build_core() -> Result<App, String> {
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
    let driver = Driver::new(exec, store, clock.clone()).with_top(50);
    let shell = Shell::new(driver, clock.clone());
    let (app, _pump) = App::new(shell, clock);
    Ok(app)
}

fn db_path() -> Result<PathBuf, String> {
    let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA não definido")?;
    let dir = std::path::Path::new(&local).join("fast365");
    std::fs::create_dir_all(&dir).map_err(|error| format!("criando {}: {error}", dir.display()))?;
    Ok(dir.join("f365-cache.db"))
}

#[tauri::command]
async fn conversations(
    state: tauri::State<'_, Mutex<Core>>,
    limit: usize,
) -> Result<Vec<ConversationDto>, String> {
    let slot = state.lock().await;
    match &*slot {
        Core::Ready(app) => app.conversations(limit).map_err(|error| error.to_string()),
        Core::Failed(why) => Err(why.clone()),
    }
}

#[tauri::command]
async fn messages(
    state: tauri::State<'_, Mutex<Core>>,
    conversation_id: String,
    before_ms: Option<i64>,
    limit: usize,
) -> Result<Vec<MessageDto>, String> {
    let slot = state.lock().await;
    match &*slot {
        Core::Ready(app) => app
            .messages(&conversation_id, before_ms, limit)
            .map_err(|error| error.to_string()),
        Core::Failed(why) => Err(why.clone()),
    }
}

#[tauri::command]
async fn search(
    state: tauri::State<'_, Mutex<Core>>,
    query: String,
    limit: usize,
) -> Result<Vec<MessageDto>, String> {
    let slot = state.lock().await;
    match &*slot {
        Core::Ready(app) => app.search(&query, limit).map_err(|error| error.to_string()),
        Core::Failed(why) => Err(why.clone()),
    }
}

#[tauri::command]
async fn capabilities(state: tauri::State<'_, Mutex<Core>>) -> Result<Vec<String>, String> {
    let slot = state.lock().await;
    match &*slot {
        Core::Ready(app) => Ok(app.capabilities()),
        Core::Failed(why) => Err(why.clone()),
    }
}

#[tauri::command]
async fn refresh(state: tauri::State<'_, Mutex<Core>>) -> Result<RoundDto, String> {
    let mut slot = state.lock().await;
    match &mut *slot {
        Core::Ready(app) => app
            .refresh_conversations()
            .await
            .map_err(|error| error.to_string()),
        Core::Failed(why) => Err(why.clone()),
    }
}

#[tauri::command]
async fn send(
    state: tauri::State<'_, Mutex<Core>>,
    conversation_id: String,
    text: String,
) -> Result<RoundDto, String> {
    let mut slot = state.lock().await;
    match &mut *slot {
        Core::Ready(app) => app
            .send(&conversation_id, &text)
            .await
            .map_err(|error| error.to_string()),
        Core::Failed(why) => Err(why.clone()),
    }
}

/// Faz o motor processar prazos vencidos (reconciliação de envio, renovação de token).
///
/// Enquanto o empurrão do tempo real não está ligado, é o frontend que chama isto periodicamente.
#[tauri::command]
async fn tick(state: tauri::State<'_, Mutex<Core>>) -> Result<RoundDto, String> {
    let mut slot = state.lock().await;
    match &mut *slot {
        Core::Ready(app) => app.wake().await.map_err(|error| error.to_string()),
        Core::Failed(why) => Err(why.clone()),
    }
}
