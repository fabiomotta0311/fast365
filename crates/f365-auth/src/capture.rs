//! Captura da sessão pela WebView (feature `webview`, `docs/ARQUITETURA.md` §2.1).
//!
//! Abre o Teams Web numa janela própria e deixa **a própria página** conduzir o login MSAL
//! (client_id, redirect, PKCE exatamente como o cliente oficial — reproduzir, não adivinhar). O
//! usuário digita senha/MFA; nada é automatizado. A cada segundo lemos as entradas MSAL do storage
//! da página; quando há conta + refresh token, coletamos os cookies `ESTSAUTH*` e a janela fecha.
//!
//! O perfil do WebView2 fica num diretório dedicado e persistente: é o que permite, no futuro,
//! parquear a página (transporte in-page) e re-logar em silêncio (`AUTH-RT-SPA-001`).

use crate::session::{self, CapturedSession, MsalProbe, SessionCookie};
use crate::vault::{SecretStore, StoredAccount, Vault, VaultError};
use crate::{Clock, Secret};
use std::path::PathBuf;
use std::time::Duration;
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};

/// `AUTH-ENTRY-001` (UNVERIFIED): abrir o Teams Web dispara o authorize MSAL do cliente oficial.
pub const TEAMS_WEB_URL: &str = "https://teams.cloud.microsoft/";
const COOKIE_URL: &str = "https://login.microsoftonline.com/";
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// Cache criptografado visto por esse tempo seguido = MSAL v4 (não é só a página carregando).
const ENCRYPTED_GRACE: Duration = Duration::from_secs(20);

/// Lê só entradas com cara de MSAL (credencial, conta ou cache criptografado) — nada de conteúdo.
const PROBE_JS: &str = r#"(() => {
  const out = [];
  for (const store of [window.localStorage, window.sessionStorage]) {
    try {
      for (let i = 0; i < store.length; i++) {
        const k = store.key(i), v = store.getItem(k);
        if (v && v.length < 65536 && (v.includes('"credentialType"') || v.includes('"authorityType"')
            || (v.includes('"nonce"') && v.includes('"data"')))) out.push([k, v]);
      }
    } catch (e) {}
  }
  return out;
})()"#;

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub start_url: String,
    /// Perfil dedicado do WebView2 (cookies/cache do login). Ex.: `%LOCALAPPDATA%\fast365\webview`.
    pub profile_dir: PathBuf,
    /// Tempo máximo para o usuário concluir login + MFA.
    pub timeout: Duration,
}

impl CaptureOptions {
    pub fn new(profile_dir: PathBuf) -> Self {
        Self {
            start_url: TEAMS_WEB_URL.into(),
            profile_dir,
            timeout: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("janela fechada antes de concluir o login")]
    Cancelled,
    #[error("login não concluído em {0:?}")]
    TimedOut(Duration),
    #[error(
        "o Teams Web guarda o cache MSAL criptografado (AUTH-MSAL-CACHE-002): o refresh token não é \
         legível por script. Registre na Issue #4 — a captura precisa de outra rota"
    )]
    EncryptedCache,
    #[error("WebView: {0}")]
    WebView(String),
}

enum Probe {
    Storage(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Vault(#[from] VaultError),
}

/// `f365 auth`: captura a sessão e grava no keyring. Devolve só dados não secretos da conta —
/// o refresh token e os cookies não saem de `f365-auth`.
/// Precisa rodar na thread principal (event loop da janela).
pub fn pair<S: SecretStore>(
    opts: &CaptureOptions,
    clock: &dyn Clock,
    vault: &Vault<S>,
) -> Result<StoredAccount, PairError> {
    let session = capture_session(opts, clock)?;
    vault.save(&session, clock.utc_now())?;
    vault
        .active_account()?
        .ok_or(PairError::Vault(VaultError::Corrupt("conta recém-gravada")))
}

/// Bloqueia até capturar a sessão, o usuário fechar a janela ou estourar o prazo.
fn capture_session(
    opts: &CaptureOptions,
    clock: &dyn Clock,
) -> Result<CapturedSession, CaptureError> {
    let web = |e: &dyn std::fmt::Display| CaptureError::WebView(e.to_string());
    std::fs::create_dir_all(&opts.profile_dir).map_err(|e| web(&e))?;

    let mut event_loop = EventLoopBuilder::<Probe>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Fast365 — entre na sua conta Microsoft")
        .with_inner_size(LogicalSize::new(520.0, 760.0))
        .build(&event_loop)
        .map_err(|e| web(&e))?;
    let mut context = WebContext::new(Some(opts.profile_dir.clone()));
    let webview = WebViewBuilder::with_web_context(&mut context)
        .with_url(&opts.start_url)
        .build(&window)
        .map_err(|e| web(&e))?;

    let deadline = clock.now() + opts.timeout;
    let mut encrypted_since = None;
    let mut outcome: Option<Result<CapturedSession, CaptureError>> = None;

    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(clock.now() + PROBE_EVERY);
        match event {
            Event::NewEvents(StartCause::Init | StartCause::ResumeTimeReached { .. }) => {
                if clock.now() >= deadline {
                    outcome = Some(Err(CaptureError::TimedOut(opts.timeout)));
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                let proxy = proxy.clone();
                let _ = webview.evaluate_script_with_callback(PROBE_JS, move |json| {
                    let _ = proxy.send_event(Probe::Storage(json));
                });
            }
            Event::UserEvent(Probe::Storage(json)) => {
                let entries: Vec<(String, String)> =
                    serde_json::from_str(&json).unwrap_or_default();
                match session::read_msal_cache(
                    entries.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                ) {
                    MsalProbe::Found(account, refresh_token) => {
                        let cookies = webview.cookies_for_url(COOKIE_URL).map_err(|e| web(&e));
                        outcome = Some(cookies.map(|all| {
                            CapturedSession {
                                account,
                                refresh_token,
                                cookies: all
                                    .into_iter()
                                    .filter(|c| session::is_session_cookie(c.name()))
                                    .map(|c| SessionCookie {
                                        name: c.name().to_owned(),
                                        value: Secret::new(c.value()),
                                        domain: c.domain().unwrap_or_default().to_owned(),
                                        path: c.path().unwrap_or("/").to_owned(),
                                    })
                                    .collect(),
                            }
                        }));
                        *control_flow = ControlFlow::Exit;
                    }
                    MsalProbe::Pending { encrypted: true } => {
                        let since = *encrypted_since.get_or_insert_with(|| clock.now());
                        if clock.now().duration_since(since) >= ENCRYPTED_GRACE {
                            outcome = Some(Err(CaptureError::EncryptedCache));
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                    MsalProbe::Pending { encrypted: false } => encrypted_since = None,
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                outcome = Some(Err(CaptureError::Cancelled));
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    drop(webview);
    drop(window); // "WebView fecha sozinha" (Checkpoint A)
    outcome.unwrap_or(Err(CaptureError::Cancelled))
}
