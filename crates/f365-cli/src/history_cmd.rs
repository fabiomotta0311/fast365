use f365_teams::{HistoryMessage, TeamsClient};
use std::sync::Arc;

pub fn display(m: &HistoryMessage, show_content: bool) {
    print!(
        "{} | {} | {} | {}",
        m.id,
        m.time.as_deref().unwrap_or("?"),
        m.sender.as_deref().unwrap_or("?"),
        m.message_type.as_deref().unwrap_or("?")
    );
    if show_content {
        print!(" | {}", m.content.as_deref().unwrap_or(""));
    }
    println!();
}

pub async fn history(chat_id: &str, top: usize, show_content: bool) -> anyhow::Result<()> {
    let client = TeamsClient::new(Arc::new(crate::chats_cmd::executor()?));
    for m in client.history(chat_id, top).await? {
        display(&m, show_content);
    }
    Ok(())
}
