//! Ponte entre o laço **streaming** do Trouter e o runtime de eventos discretos (Fase 3).
//!
//! O provider entrega `RealtimeEvent`s a um callback síncrono; o runtime consome [`crate::Inbound`]
//! por canal. Esta é a única tradução entre os dois mundos: `Changed` vira invalidação, e
//! `Ready`/`Reconnecting` viram estado do canal. Nada de conteúdo atravessa — evento é invalidação.

use crate::PumpHandle;
use f365_auth::{AuthenticatedExecutor, Clock, SystemClock};
use f365_teams::{RealtimeEvent, TeamsClient};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Implementação de `f365_driver::Realtime` que sobe o laço do Trouter **uma vez** e alimenta o
/// [`PumpHandle`] do runtime.
///
/// O laço do provider já repete e resubscreve sozinho a cada queda; aqui a responsabilidade é só
/// reportar o estado ao runtime e dizer se o canal está de pé quando o motor pedir reconexão.
pub struct RealtimeLink {
    exec: Arc<dyn AuthenticatedExecutor>,
    pump: PumpHandle,
    connected: Arc<AtomicBool>,
    spawned: AtomicBool,
}

impl RealtimeLink {
    pub fn new(exec: Arc<dyn AuthenticatedExecutor>, pump: PumpHandle) -> Self {
        Self {
            exec,
            pump,
            connected: Arc::new(AtomicBool::new(false)),
            spawned: AtomicBool::new(false),
        }
    }

    /// Sobe o laço em background.
    ///
    /// Não espera o canal ficar pronto: o estado chega pelo [`PumpHandle`] (`Realtime { connected }`)
    /// e prender o motor até o handshake atrasaria os comandos da UI na abertura.
    fn spawn(&self) {
        let exec = self.exec.clone();
        let pump = self.pump.clone();
        let connected = self.connected.clone();
        tokio::spawn(async move {
            let client = TeamsClient::new(exec);
            let clock: SystemClock = SystemClock;
            // O callback toma alça+flag; o laço guarda cópias para marcar a queda no fim.
            let pump_for_events = pump.clone();
            let connected_for_events = connected.clone();
            let mut on_event = move |event: RealtimeEvent| {
                forward(&pump_for_events, &connected_for_events, &event);
            };
            let _ = client.listen(&clock as &dyn Clock, &mut on_event).await;
            connected.store(false, Ordering::SeqCst);
            let _ = pump.realtime(false);
        });
    }
}

#[async_trait::async_trait]
impl f365_driver::Realtime for RealtimeLink {
    async fn reconnect(&self) -> Result<(), String> {
        if !self.spawned.swap(true, Ordering::SeqCst) {
            self.spawn();
            return Ok(());
        }
        if self.connected.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("tempo real ainda reconectando".into())
        }
    }
}

/// Traduz um evento do provider em estado para o runtime. Função pura, testável sem socket.
fn forward(pump: &PumpHandle, connected: &AtomicBool, event: &RealtimeEvent) {
    match event {
        RealtimeEvent::Ready => {
            connected.store(true, Ordering::SeqCst);
            let _ = pump.realtime(true);
        }
        RealtimeEvent::Reconnecting { .. } => {
            connected.store(false, Ordering::SeqCst);
            let _ = pump.realtime(false);
        }
        // Invalidação, não dado: só o id da conversa atravessa.
        RealtimeEvent::Changed {
            conversation: Some(conversation),
        } => {
            let _ = pump.invalidate(conversation);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{channel, Inbound};
    use std::time::Duration;

    #[test]
    fn changed_becomes_an_invalidation_without_content() {
        let (pump, mut inbox) = channel();
        let connected = AtomicBool::new(false);

        forward(
            &pump,
            &connected,
            &RealtimeEvent::Changed {
                conversation: Some("19:a@thread.v2".into()),
            },
        );

        assert_eq!(
            inbox.try_recv(),
            Some(Inbound::Invalidated {
                conversation: "19:a@thread.v2".into()
            })
        );
    }

    #[test]
    fn ready_and_reconnecting_move_the_channel_state() {
        let (pump, mut inbox) = channel();
        let connected = AtomicBool::new(false);

        forward(&pump, &connected, &RealtimeEvent::Ready);
        assert!(connected.load(Ordering::SeqCst));
        assert_eq!(
            inbox.try_recv(),
            Some(Inbound::Realtime { connected: true })
        );

        forward(
            &pump,
            &connected,
            &RealtimeEvent::Reconnecting {
                attempt: 2,
                after: Duration::from_secs(4),
            },
        );
        assert!(!connected.load(Ordering::SeqCst));
        assert_eq!(
            inbox.try_recv(),
            Some(Inbound::Realtime { connected: false })
        );
    }

    #[test]
    fn progress_and_unnamed_changes_are_ignored() {
        let (pump, mut inbox) = channel();
        let connected = AtomicBool::new(false);

        for event in [
            RealtimeEvent::Step("2/4 handshake"),
            RealtimeEvent::Signal("trouter.connected".into()),
            RealtimeEvent::Changed { conversation: None },
        ] {
            forward(&pump, &connected, &event);
        }

        assert_eq!(inbox.try_recv(), None, "nada de conteúdo nem de progresso");
    }
}
