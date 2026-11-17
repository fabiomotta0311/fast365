//! `f365 gate` (Issue #13) — roda os checkpoints mecanizáveis da matriz Go/No-Go
//! (`docs/ARQUITETURA.md` §2.4) e grava um **registro sanitizado** da execução, para que as duas
//! rodadas independentes do Go sejam auditáveis em vez de viverem num comentário de issue.
//!
//! O relatório **não** declara Go: F exige uma segunda execução ≥1 h depois, A (a janela da WebView
//! fechando sozinha) e E (a mensagem no Teams oficial) pedem conferência humana. O arquivo diz o que
//! foi medido, com o que foi medido — nada de pintar verde.
//!
//! Nunca entram aqui token, cookie, URL assinada ou conteúdo de mensagem (`AGENTS.md`). Ids de
//! conversa entram: não são credenciais e são o que correlaciona D/E entre rodadas.

use anyhow::Context as _;
use f365_auth::executor::SessionExecutor;
use f365_auth::tokens::{TokenBroker, TokenResource};
use f365_auth::transport::RustTransport;
use f365_auth::vault::Vault;
use f365_auth::{Clock, ExecError, SystemClock};
use f365_teams::{ConversationKind, RealtimeEvent, TeamsClient, TeamsError};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Options {
    pub chat: Option<String>,
    pub wait_d: Duration,
    pub skip_d: bool,
    pub text: Option<String>,
    pub out: PathBuf,
    pub commit: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Fail,
    /// Não dá para executar nesta invocação (`--skip-d`, sem `--chat`, sem feature `webview`).
    Skipped,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Skipped => "skipped",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Status::Pass => "✅ pass",
            Status::Fail => "❌ falhou",
            Status::Skipped => "⏭ não executado",
        }
    }
}

struct Check {
    id: &'static str,
    name: &'static str,
    status: Status,
    /// Evidência medida, sem credencial e sem conteúdo.
    detail: String,
    ms: u128,
}

/// Ingredientes de durabilidade (F) que **uma** execução consegue medir — o resto é o intervalo.
struct Durability {
    silent_repairs: usize,
    rt_age: Option<Duration>,
    token_expires_in: Option<Duration>,
}

impl Durability {
    fn describe(&self) -> String {
        format!(
            "reparos silenciosos: {} · RT resgatado há: {} · token `teams` expira em: {}",
            self.silent_repairs,
            self.rt_age.map(human).unwrap_or_else(|| "nunca".into()),
            self.token_expires_in
                .map(human)
                .unwrap_or_else(|| "desconhecido".into()),
        )
    }
}

struct Report {
    utc: SystemTime,
    version: &'static str,
    webview: bool,
    commit: Option<String>,
    checks: Vec<Check>,
    durability: Durability,
}

impl Report {
    fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }

    fn summary(&self) -> String {
        let pass = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Pass)
            .count();
        format!(
            "{} de {} checkpoint(s) medidos passaram · {}\nF (durabilidade) não se prova numa execução: repita com ≥1 h de intervalo.",
            pass,
            self.checks.len(),
            self.durability.describe()
        )
    }

    fn to_json(&self) -> serde_json::Value {
        json!({
            "utc": rfc3339(self.utc),
            "versao": self.version,
            "webview": self.webview,
            "commit": self.commit,
            "checks": self.checks.iter().map(|c| json!({
                "id": c.id,
                "nome": c.name,
                "status": c.status.label(),
                "evidencia": c.detail,
                "ms": c.ms,
            })).collect::<Vec<_>>(),
            "durabilidade": {
                "reparos_silenciosos": self.durability.silent_repairs,
                "rt_resgatado_ha_s": self.durability.rt_age.map(|d| d.as_secs()),
                "token_expira_em_s": self.durability.token_expires_in.map(|d| d.as_secs()),
            },
        })
    }

    fn to_markdown(&self) -> String {
        let mut md = String::from("# Gate da Fase 0 — registro de execução (`f365 gate`)\n\n");
        md.push_str(&format!("- **Quando (UTC):** {}\n", rfc3339(self.utc)));
        md.push_str(&format!(
            "- **Binário:** f365 {} · feature `webview`: {}\n",
            self.version,
            if self.webview { "sim" } else { "não" }
        ));
        md.push_str(&format!(
            "- **Commit:** {}\n",
            self.commit
                .as_deref()
                .unwrap_or("não informado (`--commit`)")
        ));
        md.push_str(
            "- **Decisão:** não é tomada por esta ferramenta. §2.4 exige duas execuções \
             independentes e F depois de ≥1 h.\n\n",
        );

        md.push_str("| # | Checkpoint | Resultado | Evidência medida | Tempo |\n");
        md.push_str("|---|---|---|---|---|\n");
        for c in &self.checks {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} ms |\n",
                c.id,
                c.name,
                c.status.glyph(),
                escape_cell(&c.detail),
                c.ms
            ));
        }

        md.push_str("\n## F — durabilidade: o que esta rodada mediu\n\n");
        md.push_str("Esta seção **não prova** o checkpoint F.\n\n");
        md.push_str(&format!("- {}\n", self.durability.describe()));
        md.push_str(
            "- Prova de F = repetir `f365 gate` depois de ≥1 h e conferir que B–E voltam a passar \
             sem `f365 auth` manual (repare no contador de reparos silenciosos).\n",
        );

        md.push_str("\n## O que este relatório não prova\n\n");
        md.push_str(
            "- **A** — a janela da WebView fechando sozinha: só o dono vê, com `f365 auth` \
             (esta ferramenta nunca abre WebView).\n",
        );
        md.push_str(
            "- **D** — que o evento veio de outro cliente: quem dispara a mensagem é você, na janela.\n",
        );
        md.push_str(
            "- **E** — que a mensagem apareceu no Teams oficial: confira no Teams e cite o \
             `messageId` acima.\n",
        );
        md.push_str(
            "- Nenhum token, cookie, URL assinada ou conteúdo de mensagem é registrado. Ids de \
             conversa aparecem porque são o que correlaciona as rodadas — não são credenciais.\n",
        );
        md
    }

    fn write(&self, dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dir).with_context(|| format!("criando {}", dir.display()))?;
        let stem = format!("gate-{}", rfc3339(self.utc).replace([':', '.'], "-"));
        let json_path = dir.join(format!("{stem}.json"));
        let md_path = dir.join(format!("{stem}.md"));
        let json = serde_json::to_string_pretty(&self.to_json()).context("serializando o JSON")?;
        std::fs::write(&json_path, json)
            .with_context(|| format!("escrevendo {}", json_path.display()))?;
        std::fs::write(&md_path, self.to_markdown())
            .with_context(|| format!("escrevendo {}", md_path.display()))?;
        Ok(vec![md_path, json_path])
    }
}

pub async fn run(opts: Options) -> anyhow::Result<bool> {
    let clock = SystemClock;
    let mut checks = Vec::with_capacity(5);

    let started = Instant::now();
    let (status, detail, rt_age) = check_session();
    checks.push(Check {
        id: "A",
        name: "Sessão no keyring (RT + cookies)",
        status,
        detail,
        ms: started.elapsed().as_millis(),
    });

    let repairs = Arc::new(AtomicUsize::new(0));
    let broker = build_broker(&repairs)?;

    let started = Instant::now();
    let (status, detail, token_expires_in) = match broker.describe(TokenResource::Teams).await {
        Ok(info) => {
            let left = info.expires_at.duration_since(clock.utc_now()).ok();
            let aud = info.claims.audience.clone();
            let status = if aud.is_some() {
                Status::Pass
            } else {
                Status::Fail
            };
            let detail = format!(
                "aud `{}`, expira em {}, RT {}",
                aud.as_deref().unwrap_or("ausente"),
                left.map(human).unwrap_or_else(|| "já expirado".into()),
                if info.rotated_refresh_token {
                    "rotacionado"
                } else {
                    "estável"
                }
            );
            (status, detail, left)
        }
        Err(e) => (
            Status::Fail,
            format!("refresh silencioso falhou: {e}"),
            None,
        ),
    };
    checks.push(Check {
        id: "B",
        name: "Troca de tokens sem WebView (aud de chat)",
        status,
        detail,
        ms: started.elapsed().as_millis(),
    });

    let client = TeamsClient::new(Arc::new(SessionExecutor::new(
        Arc::new(broker),
        Arc::new(RustTransport::new()?),
        None,
        Arc::new(SystemClock),
    )));

    let started = Instant::now();
    let (status, detail) = match client.list_conversations(5).await {
        Ok(listing) => {
            let list = listing.conversations;
            let chats = list
                .iter()
                .filter(|c| matches!(c.kind, ConversationKind::Chat { .. }))
                .count();
            let status = if list.is_empty() {
                Status::Fail
            } else {
                Status::Pass
            };
            (
                status,
                format!(
                    "{} conversa(s): {chats} chat(s), {} canal(is) — ids omitidos",
                    list.len(),
                    list.len() - chats
                ),
            )
        }
        Err(e) => (Status::Fail, format!("chatsvcagg: {e}")),
    };
    checks.push(Check {
        id: "C",
        name: "REST de listagem (chatsvcagg)",
        status,
        detail,
        ms: started.elapsed().as_millis(),
    });

    let started = Instant::now();
    let (status, detail) = if opts.skip_d {
        (Status::Skipped, "pulado com `--skip-d`".to_string())
    } else {
        println!(
            "\nCheckpoint D: mande uma mensagem de OUTRO cliente agora — aguardando até {}s…",
            opts.wait_d.as_secs()
        );
        check_realtime(&client, opts.wait_d).await
    };
    checks.push(Check {
        id: "D",
        name: "Tempo real (Trouter)",
        status,
        detail,
        ms: started.elapsed().as_millis(),
    });

    let started = Instant::now();
    let (status, detail) = match opts.chat.as_deref() {
        None => (
            Status::Skipped,
            "sem `--chat <id>`; a id sai de `f365 chats`".to_string(),
        ),
        Some(chat) => {
            let text = opts
                .text
                .clone()
                .unwrap_or_else(|| format!("ping f365 gate {}", rfc3339(clock.utc_now())));
            let client_message_id = f365_teams::new_client_message_id();
            match client.send_text(chat, &text, &client_message_id).await {
                Ok(receipt) => (
                    Status::Pass,
                    format!(
                        "messageId {} (clientmessageid {client_message_id})",
                        receipt
                            .server_message_id
                            .as_deref()
                            .unwrap_or("não informado pelo servidor")
                    ),
                ),
                Err(TeamsError::Exec(ExecError::Transport(_) | ExecError::TimedOut(_))) => (
                    Status::Fail,
                    format!(
                        "RESULTADO DESCONHECIDO (clientmessageid {client_message_id}) — confira no \
                         Teams antes de reenviar (SEND-RECONCILE-001)"
                    ),
                ),
                Err(e) => (Status::Fail, format!("envio: {e}")),
            }
        }
    };
    checks.push(Check {
        id: "E",
        name: "Envio pelo chat regional",
        status,
        detail,
        ms: started.elapsed().as_millis(),
    });

    let report = Report {
        utc: clock.utc_now(),
        version: env!("CARGO_PKG_VERSION"),
        webview: cfg!(feature = "webview"),
        commit: opts.commit.clone(),
        checks,
        durability: Durability {
            silent_repairs: repairs.load(Ordering::SeqCst),
            rt_age,
            token_expires_in,
        },
    };

    let written = report.write(&opts.out)?;
    println!("\n{}\n", report.summary());
    for path in &written {
        println!("  → {}", path.display());
    }
    Ok(!report.failed())
}

/// A **não** abre WebView: esta ferramenta só lê o que o dono já capturou. A parte de A que ela
/// não cobre (a janela fechando sozinha) aparece no relatório como não provada.
fn check_session() -> (Status, String, Option<Duration>) {
    let clock = SystemClock;
    match Vault::keyring().active_account() {
        Ok(Some(account)) => {
            let age = clock
                .utc_now()
                .duration_since(account.captured_at)
                .unwrap_or_default();
            let rt_age = account
                .last_refresh
                .and_then(|r| clock.utc_now().duration_since(r).ok());
            let status = if account.cookie_count > 0 {
                Status::Pass
            } else {
                Status::Fail
            };
            let detail = format!(
                "cliente {}, login há {}, RT resgatado há {}, {} cookie(s) de sessão (UPN omitido)",
                account.client_id,
                human(age),
                rt_age.map(human).unwrap_or_else(|| "nunca".into()),
                account.cookie_count
            );
            (status, detail, rt_age)
        }
        Ok(None) => (
            Status::Fail,
            "nenhuma sessão no keyring — o dono precisa rodar `f365 auth`".into(),
            None,
        ),
        Err(e) => (Status::Fail, format!("keyring ilegível: {e}"), None),
    }
}

async fn check_realtime(client: &TeamsClient, wait: Duration) -> (Status, String) {
    let clock = SystemClock;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut on_event = move |event: RealtimeEvent| {
        let _ = tx.send(event);
    };
    let mut listen = Box::pin(client.listen(&clock, &mut on_event));
    let mut deadline = Box::pin(tokio::time::sleep(wait));
    let started = Instant::now();
    let mut ready_at: Option<Instant> = None;
    let mut reconnects = 0u32;

    loop {
        tokio::select! {
            _ = &mut deadline => {
                let extra = match reconnects {
                    0 => String::new(),
                    n => format!(", com {n} reconexão(ões)"),
                };
                return (
                    Status::Fail,
                    match ready_at {
                        Some(_) => format!("conectou, mas nenhum evento em {}s{extra}", wait.as_secs()),
                        None => format!("não conectou nem entregou evento em {}s", wait.as_secs()),
                    },
                );
            }
            result = &mut listen => {
                return (
                    Status::Fail,
                    match result {
                        Ok(()) => "o tempo real encerrou antes de qualquer evento".to_string(),
                        Err(e) => format!("tempo real falhou: {e}"),
                    },
                );
            }
            event = rx.recv() => match event {
                None => return (Status::Fail, "canal do tempo real fechou".to_string()),
                // O primeiro `Pronto` é a régua da latência; um segundo (pós-reconexão) não a refaz.
                Some(RealtimeEvent::Ready) => {
                    ready_at.get_or_insert_with(Instant::now);
                }
                Some(RealtimeEvent::Changed { conversation }) => {
                    let since = ready_at.unwrap_or(started).elapsed();
                    let extra = match reconnects {
                        0 => String::new(),
                        n => format!(", após {n} reconexão(ões)"),
                    };
                    return (
                        Status::Pass,
                        format!(
                            "evento {:.1}s após `Pronto`{extra} (conversa {})",
                            since.as_secs_f32(),
                            conversation.as_deref().unwrap_or("não identificada")
                        ),
                    );
                }
                // Queda com recuperação automática **não** reprova D (é o comportamento esperado no
                // #50/#51); ela só precisa terminar em evento dentro da janela.
                Some(RealtimeEvent::Reconnecting { .. }) => reconnects += 1,
                // `Step` (progresso) e `Signal` (sinal do Trouter): seguem, não são o checkpoint.
                Some(_) => {}
            },
        }
    }
}

fn build_broker(repairs: &Arc<AtomicUsize>) -> anyhow::Result<TokenBroker> {
    let broker = crate::token_cmd::base_broker()?;
    // Contador: F só é verdade se o refresh silencioso acontecer **sem** `auth` manual.
    #[cfg(feature = "webview")]
    let broker = broker.with_repair(Arc::new(CountingRepair {
        inner: crate::session_cmd::SilentRepair,
        repairs: repairs.clone(),
    }));
    #[cfg(not(feature = "webview"))]
    let _ = repairs;
    Ok(broker)
}

#[cfg(feature = "webview")]
struct CountingRepair {
    inner: crate::session_cmd::SilentRepair,
    repairs: Arc<AtomicUsize>,
}

#[cfg(feature = "webview")]
#[async_trait::async_trait]
impl f365_auth::tokens::SessionRepair for CountingRepair {
    async fn repair(&self) -> Result<(), String> {
        self.repairs.fetch_add(1, Ordering::SeqCst);
        self.inner.repair().await
    }
}

fn human(d: Duration) -> String {
    crate::session_cmd::human(d)
}

fn escape_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '|' => out.push_str("\\|"),
            '\n' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// `RFC 3339` em UTC a partir de um `SystemTime` — sem dependência nova (só aritmética civil).
fn rfc3339(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rest = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60,
        since.subsec_millis()
    )
}

/// Dias desde 1970-01-01 → (ano, mês, dia) no calendário gregoriano proléptico.
/// Algoritmo de Howard Hinnant (`civil_from_days`), aritmética inteira pura.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(id: &'static str, status: Status) -> Check {
        Check {
            id,
            name: "checkpoint",
            status,
            detail: "evidência medida".into(),
            ms: 12,
        }
    }

    fn report(checks: Vec<Check>) -> Report {
        Report {
            utc: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            version: "0.0.0",
            webview: true,
            commit: Some("abc1234".into()),
            checks,
            durability: Durability {
                silent_repairs: 1,
                rt_age: Some(Duration::from_secs(7_380)),
                token_expires_in: Some(Duration::from_secs(2_520)),
            },
        }
    }

    #[test]
    fn civil_dates_match_known_epoch_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(1_096), (1973, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn rfc3339_is_utc_with_millis() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_millis(1_700_000_000_042)),
            "2023-11-14T22:13:20.042Z"
        );
    }

    #[test]
    fn a_failing_check_fails_the_run_but_a_skipped_one_does_not() {
        let mut r = report(vec![
            check("A", Status::Pass),
            check("B", Status::Pass),
            check("D", Status::Skipped),
        ]);
        assert!(!r.failed(), "pular um checkpoint não inventa falha");
        r.checks.push(check("C", Status::Fail));
        assert!(r.failed());
    }

    #[test]
    fn markdown_shows_every_check_and_never_claims_f() {
        let md = report(vec![
            check("A", Status::Pass),
            check("D", Status::Skipped),
            check("E", Status::Fail),
        ])
        .to_markdown();

        for id in ["| A |", "| D |", "| E |"] {
            assert!(md.contains(id), "faltou {id}");
        }
        assert!(md.contains("não executado"));
        assert!(md.contains("❌ falhou"));
        assert!(md.contains("**não prova**"));
        assert!(md.contains("Prova de F = repetir"));
    }

    #[test]
    fn json_schema_is_the_documented_one() {
        let value = report(vec![check("A", Status::Pass)]).to_json();
        let mut keys: Vec<String> = value.as_object().expect("objeto").keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "checks",
                "commit",
                "durabilidade",
                "utc",
                "versao",
                "webview"
            ]
        );
    }

    #[test]
    fn report_never_carries_credentials() {
        let r = report(vec![check("B", Status::Pass)]);
        let md = r.to_markdown().to_lowercase();
        let js = r.to_json().to_string().to_lowercase();
        for banned in [
            "access_token",
            "refresh_token",
            "skypetoken",
            "bearer",
            "cookie=",
        ] {
            assert!(!md.contains(banned), "markdown vazou {banned}");
            assert!(!js.contains(banned), "json vazou {banned}");
        }
    }

    #[test]
    fn table_cells_do_not_break_the_table() {
        assert_eq!(escape_cell("a|b\nc"), "a\\|b c");
    }

    /// Prova o artefato em si (e que o nome do arquivo é seguro no Windows: sem `:`) sem tocar em
    /// keyring, rede ou WebView.
    #[test]
    fn writes_markdown_and_json_side_by_side() {
        let dir = std::env::temp_dir().join(format!("f365-gate-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let written = report(vec![check("A", Status::Pass), check("E", Status::Skipped)])
            .write(&dir)
            .expect("grava os dois relatórios");

        assert_eq!(written.len(), 2);
        let md = std::fs::read_to_string(&written[0]).expect("markdown");
        let js = std::fs::read_to_string(&written[1]).expect("json");
        assert!(md.starts_with("# Gate da Fase 0"));
        assert!(md.contains("| A |") && md.contains("| E |"));
        assert!(js.contains("\"checks\"") && js.contains("\"durabilidade\""));

        let name = written[0]
            .file_name()
            .and_then(|n| n.to_str())
            .expect("nome do arquivo");
        assert_eq!(name, "gate-2023-11-14T22-13-20-000Z.md");

        std::fs::remove_dir_all(&dir).expect("limpa");
    }
}
