//! `f365 listen` (Checkpoint D): mostra cada passo da conexão ao Trouter e, depois, uma linha por
//! conversa alterada — nunca o conteúdo (evento = invalidação). Ctrl+C encerra.

use f365_auth::{Clock, SystemClock};
use f365_teams::{RealtimeEvent, TeamsClient};
use std::sync::Arc;

pub async fn listen(fetch: bool, show_content: bool) -> anyhow::Result<()> {
    let client = TeamsClient::new(Arc::new(crate::chats_cmd::executor()?));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
    let clock = SystemClock;
    let start = clock.now();
    let mut on_event = move |e: RealtimeEvent| {
        let t = start.elapsed().as_secs_f32();
        match e {
            RealtimeEvent::Step(s) => println!("  {s}…"),
            RealtimeEvent::Ready => println!(
                "\nPronto (+{t:.1}s). Mande uma mensagem de OUTRO dispositivo/conta e veja a linha aparecer. Ctrl+C sai.\n"
            ),
            RealtimeEvent::Changed { conversation } => {
                if fetch {
                    if let Some(id) = conversation.as_ref() {
                        if tx.try_send(id.clone()).is_err() { eprintln!("Aviso: fila de histórico cheia; evento descartado."); }
                    } else {
                        println!("[+{t:.1}s] mudança em (conversa não identificada)");
                    }
                } else {
                    println!("[+{t:.1}s] mudança em {}", conversation.as_deref().unwrap_or("(conversa não identificada)"));
                }
            },
            RealtimeEvent::Signal(name) => println!("[+{t:.1}s] sinal do Trouter: {name}"),
            RealtimeEvent::Reconnecting { attempt, after } => {
                println!("[+{t:.1}s] conexão caiu — tentativa {attempt} em {}s", after.as_secs())
            }
        }
    };

    println!("Conectando ao tempo real (Trouter)…");
    let worker = async {
        while let Some(id) = rx.recv().await {
            match client.history(&id, 1).await {
                Ok(messages) => {
                    for message in messages {
                        crate::history_cmd::display(&message, show_content);
                    }
                }
                Err(_) => eprintln!("Aviso: não foi possível consultar histórico da conversa."),
            }
        }
    };
    tokio::select! {
        r = client.listen(&clock, &mut on_event) => r.map_err(Into::into),
        _ = worker => Ok(()),
        _ = tokio::signal::ctrl_c() => {
            println!("\nEncerrado.");
            Ok(())
        }
    }
}
