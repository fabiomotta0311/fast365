//! `f365` — binário de validação da Fase 0 (gate Go/No-Go).
//! Ver `docs/ARQUITETURA.md` §2.3 e §2.4.
//!
//! Subcomandos mínimos que provam o gate:
//!   f365 auth    → captura de sessão (A)
//!   f365 chats   → troca de tokens + GET chatsvcagg (B, C)
//!   f365 listen  → Trouter WS (D)
//!   f365 send    → POST chatsvcagg (E)
//!
//! `f365 gate` roda os checkpoints mecanizáveis e grava o registro sanitizado da matriz (§2.4), para
//! as duas rodadas do Go serem auditáveis (Issue #13).
//!
//! `f365 sync` é a composição da Fase 2: cache local + motor + tempo real num laço só (#16).

mod chats_cmd;
mod gate_cmd;
mod history_cmd;
mod listen_cmd;
mod logout_cmd;
mod send_cmd;
mod session_cmd;
mod sync_cmd;
mod token_cmd;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "f365",
    version,
    about = "Fast365 — cliente nativo M365 (PoC Fase 0)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Abre WebView, captura a sessão e grava o refresh token no keyring.
    Auth {
        /// Começa com a janela escondida (re-login pelos cookies do perfil); ela só aparece se o
        /// Microsoft pedir login. É o que o re-pareamento automático usa.
        #[arg(long)]
        silent: bool,
    },
    /// Apaga a sessão local (keyring + perfil da WebView) para trocar de conta.
    ///
    /// Só o dono, num terminal normal. O cache de mensagens fica, a menos que `--cache`.
    Logout {
        /// Apaga também o cache local de mensagens (`f365-cache.db`).
        #[arg(long)]
        cache: bool,
        /// Não pergunta antes de apagar (necessário fora de um terminal).
        #[arg(long)]
        yes: bool,
    },
    /// Lista conversas/canais (troca de tokens + GET chatsvcagg).
    Chats {
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Diagnóstico de forma: imprime só os nomes de campo do primeiro chat (sem valores).
        #[arg(long)]
        fields: bool,
    },
    /// Roda os checkpoints A–E da matriz §2.4 e grava o registro sanitizado da execução (Issue #13).
    ///
    /// Não abre WebView e não declara Go: F exige repetir depois de ≥1 h (ver `docs/GATE-FASE0.md`).
    Gate {
        /// Conversa do Checkpoint E (a id sai de `f365 chats`); sem ela, E fica como não executado.
        #[arg(long)]
        chat: Option<String>,
        /// Segundos de espera por um evento no Checkpoint D.
        #[arg(long, default_value_t = 120)]
        wait_d: u64,
        /// Pula o Checkpoint D (não há outro cliente para disparar a mensagem agora).
        #[arg(long)]
        skip_d: bool,
        /// Texto do Checkpoint E (padrão: `ping f365 gate <utc>`).
        #[arg(long)]
        text: Option<String>,
        /// Diretório dos relatórios (`.gitignore` já cobre `gate-reports/`).
        #[arg(long, default_value = "gate-reports")]
        out: std::path::PathBuf,
        /// Commit citado no relatório — `--commit $(git rev-parse --short HEAD)`.
        #[arg(long)]
        commit: Option<String>,
    },
    /// Mantém cache local + tempo real ativos, renovando a sessão em silêncio (Fase 2).
    ///
    /// Não abre WebView: quem roda é o dono. Ctrl+C encerra.
    Sync {
        /// Banco do cache (padrão: `%LOCALAPPDATA%\fast365\f365-cache.db`).
        #[arg(long)]
        db: Option<std::path::PathBuf>,
        /// Quantas mensagens buscar por conversa.
        #[arg(long, default_value_t = 30)]
        top: usize,
        /// Só atualiza o cache de conversas e sai.
        #[arg(long)]
        once: bool,
    },
    /// Conecta no Trouter (WS) e imprime eventos de novas mensagens.
    Listen {
        #[arg(long)]
        fetch: bool,
        #[arg(long, requires = "fetch")]
        show_content: bool,
    },
    /// Consulta mensagens recentes no serviço regional.
    History {
        chat_id: String,
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long)]
        show_content: bool,
    },
    /// Envia texto puro para uma conversa (chat regional; id vem de `f365 chats`). Checkpoint E.
    Send { chat_id: String, text: String },
    /// Renova em silêncio (sem WebView) e imprime os claims do token — nunca o token (Checkpoint B).
    Token {
        #[arg(long, value_enum, default_value = "teams")]
        resource: token_cmd::Resource,
    },
    /// Valida a sessão: UPN, tenant e região descoberta (online).
    Whoami {
        /// Não consulta a rede: só o que está no keyring.
        #[arg(long)]
        offline: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Auth { silent } => session_cmd::auth(silent)?,
        Command::Logout { cache, yes } => logout_cmd::logout(cache, yes)?,
        Command::Chats { top, fields } => chats_cmd::chats(top, fields).await?,
        Command::Gate {
            chat,
            wait_d,
            skip_d,
            text,
            out,
            commit,
        } => {
            let opts = gate_cmd::Options {
                chat,
                wait_d: std::time::Duration::from_secs(wait_d),
                skip_d,
                text,
                out,
                commit,
            };
            if !gate_cmd::run(opts).await? {
                // Saída não-zero deixa o runner (ou o CI futuro) reprovar a rodada sem parsear texto.
                std::process::exit(1);
            }
        }
        Command::Sync { db, top, once } => {
            sync_cmd::sync(sync_cmd::Options { db, top, once }).await?
        }
        Command::Listen {
            fetch,
            show_content,
        } => listen_cmd::listen(fetch, show_content).await?,
        Command::History {
            chat_id,
            top,
            show_content,
        } => history_cmd::history(&chat_id, top, show_content).await?,
        Command::Send { chat_id, text } => send_cmd::send(&chat_id, &text).await?,
        Command::Token { resource } => token_cmd::token(resource).await?,
        Command::Whoami { offline } => session_cmd::whoami(offline).await?,
    }
    Ok(())
}
