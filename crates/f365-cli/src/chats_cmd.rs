//! `f365 chats` (Checkpoint C): lista conversas pelo executor. Credencial e rota ficam no
//! `f365-auth`; aqui só chega a lista (id + título).

use f365_auth::executor::SessionExecutor;
use f365_auth::transport::RustTransport;
use f365_auth::SystemClock;
use f365_teams::{ConversationKind, TeamsClient};
use std::sync::Arc;

/// Executor da sessão salva: rota Rust; a InPage ainda não existe (ver #9).
pub fn executor() -> anyhow::Result<SessionExecutor> {
    Ok(SessionExecutor::new(
        Arc::new(crate::token_cmd::broker()?),
        Arc::new(RustTransport::new()?),
        None,
        Arc::new(SystemClock),
    ))
}

pub async fn chats(top: usize, fields: bool) -> anyhow::Result<()> {
    let client = TeamsClient::new(Arc::new(executor()?));

    // Diagnóstico de forma (TEAMS-CSA-SHAPE-001): nomes de campo, nunca valores.
    if fields {
        let names = f365_teams::chats::field_names(&client.list_raw().await?);
        if names.is_empty() {
            println!("Nenhum campo legível em `chats[]` (lista vazia ou resposta inesperada).");
            return Ok(());
        }
        println!("Campos do primeiro chat (sem valores):");
        for name in names {
            println!("  {name}");
        }
        return Ok(());
    }

    let conversations = client.list_conversations(top).await?.conversations;

    if conversations.is_empty() {
        println!("Nenhuma conversa retornada (HTTP 200, lista vazia).");
        return Ok(());
    }
    for (i, c) in conversations.iter().enumerate() {
        let kind = match &c.kind {
            ConversationKind::Chat { chat_type } => chat_type.as_deref().unwrap_or("chat"),
            ConversationKind::Channel { .. } => "canal",
        };
        println!("{:>3}. [{kind}] {}\n     {}", i + 1, c.title, c.id);
    }
    println!("\n{} conversa(s) via chatsvcagg.", conversations.len());
    Ok(())
}
