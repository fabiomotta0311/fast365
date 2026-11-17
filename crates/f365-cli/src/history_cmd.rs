use f365_teams::{HistoryMessage, TeamsClient};
use std::sync::Arc;

fn format_message(m: &HistoryMessage, show_content: bool) -> String {
    let mut line = format!(
        "{} | {} | {} | {}",
        m.id,
        m.time.as_deref().unwrap_or("?"),
        m.sender.as_deref().unwrap_or("?"),
        m.message_type.as_deref().unwrap_or("?")
    );
    if show_content {
        line.push_str(&format!(" | {}", m.content.as_deref().unwrap_or("")));
    }
    let mut safe = String::new();
    for c in line.chars() {
        if c.is_control() {
            safe.extend(c.escape_default());
        } else {
            safe.push(c);
        }
    }
    safe
}

pub fn display(m: &HistoryMessage, show_content: bool) {
    println!("{}", format_message(m, show_content));
}

pub async fn history(chat_id: &str, top: usize, show_content: bool) -> anyhow::Result<()> {
    let client = TeamsClient::new(Arc::new(crate::chats_cmd::executor()?));
    for m in client.history(chat_id, top).await? {
        display(&m, show_content);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_fields_are_safe_for_terminal_with_and_without_content() {
        let m = HistoryMessage {
            client_message_id: None,
            id: "id\x1b[31m".into(),
            time: Some("t\n".into()),
            sender: Some("s\r".into()),
            message_type: Some("k\x07".into()),
            content: Some("private\x1b[2J".into()),
        };
        for show in [false, true] {
            let line = format_message(&m, show);
            assert!(!line.chars().any(char::is_control), "{line:?}");
            assert_eq!(line.contains("private"), show);
        }
    }
}
