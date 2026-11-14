//! `f365 listen` (Checkpoint D): mostra cada passo da conexão ao Trouter e, depois, uma linha por
//! conversa alterada — nunca o conteúdo (evento = invalidação). Ctrl+C encerra.

use f365_auth::{Clock, SystemClock};
use f365_teams::{RealtimeEvent, TeamsClient};
use std::sync::Arc;

pub async fn listen() -> anyhow::Result<()> {
    let client = TeamsClient::new(Arc::new(crate::chats_cmd::executor()?));
    let clock = SystemClock;
    let start = clock.now();
    let mut on_event = move |e: RealtimeEvent| {
        let t = start.elapsed().as_secs_f32();
        match e {
            RealtimeEvent::Step(s) => println!("  {s}…"),
            RealtimeEvent::Ready => println!(
                "\nPronto (+{t:.1}s). Mande uma mensagem de OUTRO dispositivo/conta e veja a linha aparecer. Ctrl+C sai.\n"
            ),
            RealtimeEvent::Changed { conversation } => println!(
                "[+{t:.1}s] mudança em {}",
                conversation.as_deref().unwrap_or("(conversa não identificada)")
            ),
            RealtimeEvent::Signal(name) => println!("[+{t:.1}s] sinal do Trouter: {name}"),
            RealtimeEvent::Reconnecting { attempt, after } => {
                println!("[+{t:.1}s] conexão caiu — tentativa {attempt} em {}s", after.as_secs())
            }
        }
    };

    println!("Conectando ao tempo real (Trouter)…");
    tokio::select! {
        r = client.listen(&clock, &mut on_event) => r.map_err(Into::into),
        _ = tokio::signal::ctrl_c() => {
            println!("\nEncerrado.");
            Ok(())
        }
    }
}
