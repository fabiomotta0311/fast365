//! Captura da sessão pela WebView (feature `webview`, `docs/ARQUITETURA.md` §2.1).
//!
//! Abre o Teams Web numa janela própria e deixa **a própria página** conduzir o login MSAL
//! (client_id, redirect, PKCE exatamente como o cliente oficial — reproduzir, não adivinhar). O
//! usuário digita senha/MFA; nada é automatizado. A cada segundo lemos as entradas MSAL do storage
//! da página; quando há a conta ativa + refresh token dela, coletamos os cookies `ESTSAUTH*` e a
//! janela fecha.
//!
//! Só vira sessão o que foi lido numa origem confiável do Teams (`AUTH-ORIGIN-001`) e no documento
//! atual: cada navegação de topo incrementa uma geração, e sondas de documentos anteriores são
//! descartadas. O login pode passar por outros hosts (Entra, federação), mas nenhum deles entrega
//! sessão.
//!
//! O perfil do WebView2 fica num diretório dedicado e persistente: é o que permite, no futuro,
//! parquear a página (transporte in-page) e re-logar em silêncio (`AUTH-RT-SPA-001`).

use crate::probe::{accept_probe, BlockerTimer, Schedule, Wake};
use crate::secret::Secret;
use crate::session::{self, Blocker, CapturedSession, MsalProbe, SessionCookie};
use crate::vault::{StoredAccount, Vault, VaultError};
use crate::Clock;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};
use zeroize::{Zeroize, Zeroizing};

/// `AUTH-ENTRY-001` (VERIFIED, Checkpoint A 2026-11-13): abrir o Teams Web dispara o authorize MSAL
/// do cliente oficial (client_id web `5e3ce6c0-…`).
pub const TEAMS_WEB_URL: &str = "https://teams.cloud.microsoft/";
const COOKIE_URL: &str = "https://login.microsoftonline.com/";
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// Bloqueio (cache criptografado, conta ambígua) visto por esse tempo seguido vira erro.
const BLOCKER_GRACE: Duration = Duration::from_secs(20);

/// Lê só entradas com cara de MSAL (credencial, conta, conta ativa ou cache criptografado) — nada
/// de conteúdo — e a origem do documento que as produziu.
const PROBE_JS: &str = r#"(() => {
  const out = [];
  for (const store of [window.localStorage, window.sessionStorage]) {
    try {
      for (let i = 0; i < store.length; i++) {
        const k = store.key(i), v = store.getItem(k);
        if (!v || v.length >= 65536) continue;
        if (k.includes('active-account') || v.includes('"credentialType"') || v.includes('"authorityType"')
            || (v.includes('"nonce"') && v.includes('"data"'))) out.push([k, v]);
      }
    } catch (e) {}
  }
  return { origin: window.location.origin, entries: out };
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
    #[error(
        "há mais de uma conta no perfil da WebView e a página não indicou qual está ativa. Escolha a \
         conta no Teams Web e rode `f365 auth` de novo"
    )]
    AmbiguousAccount,
    #[error("WebView: {0}")]
    WebView(String),
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
pub fn pair(
    opts: &CaptureOptions,
    clock: &dyn Clock,
    vault: &Vault,
) -> Result<StoredAccount, PairError> {
    let session = capture_session(opts, clock)?;
    vault.save(&session, clock.utc_now())?;
    vault
        .active_account()?
        .ok_or(PairError::Vault(VaultError::Corrupt("conta recém-gravada")))
}

/// Resultado de uma sonda: a geração do documento em que foi PEDIDA + o JSON que voltou.
struct ProbeResult {
    generation: u64,
    json: Zeroizing<String>,
}

#[derive(Deserialize)]
struct ProbePayload {
    origin: String,
    entries: Vec<[String; 2]>,
}

impl Drop for ProbePayload {
    fn drop(&mut self) {
        self.entries.zeroize(); // contém o RT em claro
    }
}

/// Bloqueia até capturar a sessão, o usuário fechar a janela ou estourar o prazo.
fn capture_session(
    opts: &CaptureOptions,
    clock: &dyn Clock,
) -> Result<CapturedSession, CaptureError> {
    let web = |e: &dyn std::fmt::Display| CaptureError::WebView(e.to_string());
    std::fs::create_dir_all(&opts.profile_dir).map_err(|e| web(&e))?;

    let mut event_loop = EventLoopBuilder::<ProbeResult>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Fast365 — entre na sua conta Microsoft")
        .with_inner_size(LogicalSize::new(520.0, 760.0))
        .build(&event_loop)
        .map_err(|e| web(&e))?;

    // Toda navegação de topo invalida as sondas em voo do documento anterior.
    let generation = Arc::new(AtomicU64::new(0));
    let mut context = WebContext::new(Some(opts.profile_dir.clone()));
    let webview = WebViewBuilder::with_web_context(&mut context)
        .with_url(&opts.start_url)
        .with_navigation_handler({
            let generation = Arc::clone(&generation);
            move |_url| {
                generation.fetch_add(1, Ordering::SeqCst);
                true // login/federação podem passar por qualquer host; só a origem decide a sessão
            }
        })
        .build(&window)
        .map_err(|e| web(&e))?;

    let mut schedule = Schedule::new(clock.now(), opts.timeout, PROBE_EVERY);
    let mut blockers = BlockerTimer::default();
    let mut outcome: Option<Result<CapturedSession, CaptureError>> = None;

    event_loop.run_return(|event, _, control_flow| {
        let mut finish = |r: Result<CapturedSession, CaptureError>, cf: &mut ControlFlow| {
            outcome = Some(r);
            *cf = ControlFlow::Exit;
        };

        match event {
            Event::UserEvent(probe) => {
                let current = generation.load(Ordering::SeqCst);
                let Ok(payload) = serde_json::from_str::<ProbePayload>(&probe.json) else {
                    return;
                };
                let url = webview.url().unwrap_or_default();
                if !accept_probe(probe.generation, current, &payload.origin, &url) {
                    return;
                }
                let entries = payload
                    .entries
                    .iter()
                    .map(|[k, v]| (k.as_str(), v.as_str()));
                match session::read_msal_cache(entries) {
                    MsalProbe::Found(account, refresh_token) => {
                        let session = webview
                            .cookies_for_url(COOKIE_URL)
                            .map_err(|e| web(&e))
                            .map(|all| CapturedSession {
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
                            });
                        return finish(session, control_flow);
                    }
                    MsalProbe::Pending(blocker) => {
                        match blockers.observe(blocker, clock.now(), BLOCKER_GRACE) {
                            Some(Blocker::Encrypted) => {
                                return finish(Err(CaptureError::EncryptedCache), control_flow)
                            }
                            Some(Blocker::AmbiguousAccount) => {
                                return finish(Err(CaptureError::AmbiguousAccount), control_flow)
                            }
                            None => {}
                        }
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                return finish(Err(CaptureError::Cancelled), control_flow);
            }
            _ => {}
        }

        // Todo despertar, qualquer que seja o motivo, confere prazo e sonda contra instantes fixos.
        match schedule.on_wake(clock.now()) {
            Some(Wake::TimedOut) => {
                return finish(Err(CaptureError::TimedOut(opts.timeout)), control_flow)
            }
            Some(Wake::Probe) => {
                let requested_in = generation.load(Ordering::SeqCst);
                let proxy = proxy.clone();
                let _ = webview.evaluate_script_with_callback(PROBE_JS, move |json| {
                    let _ = proxy.send_event(ProbeResult {
                        generation: requested_in,
                        json: Zeroizing::new(json),
                    });
                });
            }
            None => {}
        }
        *control_flow = ControlFlow::WaitUntil(schedule.wait_until());
    });

    drop(webview);
    drop(window); // "WebView fecha sozinha" (Checkpoint A)
    outcome.unwrap_or(Err(CaptureError::Cancelled))
}
