//! `f365` — binário de validação da Fase 0 (gate Go/No-Go).
//! Ver `docs/ARQUITETURA.md` §2.3 e §2.4.
//!
//! Subcomandos mínimos que provam o gate:
//!   f365 auth    → captura de sessão (A)
//!   f365 chats   → troca de tokens + GET chatsvcagg (B, C)
//!   f365 listen  → Trouter WS (D)
//!   f365 send    → POST chatsvcagg (E)

mod session_cmd;

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
    /// Envia uma mensagem de teste (POST chatsvcagg).
    Send { chat_id: String, text: String },
    /// Imprime o access token atual + claims decodificados (debug).
    Token {
        #[arg(long, default_value = "teams")]
        resource: String,
    },
    /// Valida a sessão: UPN, tenant, região descoberta.
    Whoami,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Auth => session_cmd::auth()?,
        Command::Chats { top: _ } => todo!("Fase 0-B/C: troca de tokens + GET chatsvcagg (§2.3)"),
        Command::Listen => todo!("Fase 0-C: Trouter WS, evento = invalidação (§2.3)"),
        Command::Send { .. } => todo!("Fase 0-D: POST chatsvcagg (§2.3)"),
        Command::Token { .. } => todo!("debug: imprime access token + claims (§2.3)"),
        Command::Whoami => session_cmd::whoami()?,
    }
    Ok(())
}
