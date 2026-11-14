//! `f365` — binário de validação da Fase 0 (gate Go/No-Go).
//! Ver `docs/ARQUITETURA.md` §2.3 e §2.4.
//!
//! Subcomandos mínimos que provam o gate:
//!   f365 auth    → captura de sessão (A)
//!   f365 chats   → troca de tokens + GET chatsvcagg (B, C)
//!   f365 listen  → Trouter WS (D)
//!   f365 send    → POST chatsvcagg (E)

mod chats_cmd;
mod listen_cmd;
mod send_cmd;
mod session_cmd;
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
    Auth,
    /// Lista conversas/canais (troca de tokens + GET chatsvcagg).
    Chats {
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
    /// Conecta no Trouter (WS) e imprime eventos de novas mensagens.
    Listen,
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
        Command::Auth => session_cmd::auth()?,
        Command::Chats { top } => chats_cmd::chats(top).await?,
        Command::Listen => listen_cmd::listen().await?,
        Command::Send { chat_id, text } => send_cmd::send(&chat_id, &text).await?,
        Command::Token { resource } => token_cmd::token(resource).await?,
        Command::Whoami { offline } => session_cmd::whoami(offline).await?,
    }
    Ok(())
}
