//! `f365 chats` (Checkpoint C): lista conversas pelo executor. Credencial e rota ficam no
//! `f365-auth`; aqui só chega a lista (id + título).

use f365_auth::executor::SessionExecutor;
use f365_auth::transport::RustTransport;
use f365_auth::SystemClock;
use f365_teams::{ConversationKind, TeamsClient};
use std::sync::Arc;

pub async fn chats(top: usize) -> anyhow::Result<()> {
    let exec = SessionExecutor::new(
        Arc::new(crate::token_cmd::broker()?),
        Arc::new(RustTransport::new()?),
        None, // rota InPage ainda não existe (ver #9)
        Arc::new(SystemClock),
    );
    let conversations = TeamsClient::new(Arc::new(exec))
        .list_conversations(top)
        .await?;

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
