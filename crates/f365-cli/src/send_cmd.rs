//! `f365 send` (Checkpoint E): envia texto puro pelo executor. O `clientmessageid` é gerado e
//! mostrado ANTES do envio — se o resultado ficar incerto, é com ele que se confere no Teams.

use f365_auth::ExecError;
use f365_teams::{new_client_message_id, TeamsClient, TeamsError};
use std::sync::Arc;

pub async fn send(chat_id: &str, text: &str) -> anyhow::Result<()> {
    let client_message_id = new_client_message_id();
    println!("clientmessageid: {client_message_id}");

    let client = TeamsClient::new(Arc::new(crate::chats_cmd::executor()?));
    match client.send_text(chat_id, text, &client_message_id).await {
        Ok(receipt) => {
            println!(
                "Enviada. messageId: {}",
                receipt
                    .server_message_id
                    .as_deref()
                    .unwrap_or("(o servidor não informou)")
            );
            Ok(())
        }
        // Rede/prazo: o servidor pode ter recebido. Nunca reenviar sem conferir (SEND-RECONCILE-001).
        Err(TeamsError::Exec(e @ (ExecError::Transport(_) | ExecError::TimedOut(_)))) => {
            anyhow::bail!(
                "RESULTADO DESCONHECIDO ({e}). A mensagem pode ter sido entregue: confira no Teams \
                 antes de reenviar (clientmessageid {client_message_id})."
            )
        }
        Err(e) => Err(e.into()),
    }
}
