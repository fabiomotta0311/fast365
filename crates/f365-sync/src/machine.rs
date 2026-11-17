//! Transições do motor de sincronização (Fase 2, #15).
//!
//! FSM síncrona, sem I/O e sem relógio: recebe [`Event`] e devolve [`Decision`]. O tempo entra
//! sempre por parâmetro (`now`), nunca de `Instant::now` — `clippy.toml` do crate falha se alguém
//! tentar (o teste determinístico é a razão de existir deste módulo).
//!
//! Invariante de envio (`SEND-RECONCILE-001`, UNVERIFIED): resposta perdida **reconcilia** por
//! `clientmessageid`; sem evidência o estado final é [`SendState::Uncertain`], nunca "entregue".
//!
//! As capacidades do §2.5 têm estado próprio e não se derrubam: envio ([`SendState`]), histórico
//! ([`ConversationState`]) e tempo real ([`Realtime`]). Só a rede ([`Connectivity`]) é global.

use crate::{ClientMessageId, ConversationId, Decision, Event, OpId, Outcome};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// A rede está de pé? É o único estado global: sem ela nenhuma capacidade avança.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// Ainda não se sabe (início do processo): o motor tenta.
    Unknown,
    Up,
    Down,
}

/// Capacidade `realtime` (socket do Trouter). Independente de histórico e de envio (§2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Realtime {
    Unknown,
    Connected,
    /// Caiu e o motor está tentando reabrir.
    Reconnecting,
}

/// Capacidade `history` por conversa: o cache local está fresco?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationState {
    /// Algo mudou e o conteúdo local não é confiável: buscar via REST.
    Dirty,
    Fetching {
        op: OpId,
        /// Chegou outra invalidação durante a busca: buscar de novo ao terminar (coalescência).
        stale_again: bool,
    },
    Fresh,
}

/// Capacidade `send` por mensagem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendState {
    /// Aceito localmente, aguardando oportunidade (rede caída).
    Pending {
        conversation: ConversationId,
        attempts: u32,
    },
    /// Operação de envio em voo.
    InFlight {
        conversation: ConversationId,
        op: OpId,
        attempts: u32,
    },
    /// A resposta se perdeu: buscando evidência por `clientmessageid`. **Nunca** reenviar daqui.
    Reconciling {
        conversation: ConversationId,
        deadline: Instant,
    },
    /// Confirmado pelo servidor (resposta ou [`Event::MessageObserved`]).
    Delivered { conversation: ConversationId },
    /// Estado final sem prova: a UI precisa mostrar isso como incerto, não como entregue.
    Uncertain {
        conversation: ConversationId,
        reason: UncertainReason,
    },
}

impl SendState {
    pub fn conversation(&self) -> &ConversationId {
        match self {
            SendState::Pending { conversation, .. }
            | SendState::InFlight { conversation, .. }
            | SendState::Reconciling { conversation, .. }
            | SendState::Delivered { conversation }
            | SendState::Uncertain { conversation, .. } => conversation,
        }
    }

    /// Tentativas de envio já feitas (0 quando nunca saiu).
    pub fn attempts(&self) -> u32 {
        match self {
            SendState::Pending { attempts, .. } | SendState::InFlight { attempts, .. } => *attempts,
            SendState::Reconciling { .. }
            | SendState::Delivered { .. }
            | SendState::Uncertain { .. } => 1,
        }
    }
}

/// Por que um envio terminou sem prova de entrega.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UncertainReason {
    /// Falhou de forma retentável e as tentativas acabaram.
    RetriesExhausted,
    /// Falha definitiva do servidor/credencial.
    Refused,
    /// A resposta se perdeu e o prazo de reconciliação venceu sem evidência.
    NoEvidence,
}

/// Temporizações do motor. Injetadas (não lidas do relógio) para o teste ser determinístico.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Tentativas totais de um envio antes de desistir.
    pub send_attempts: u32,
    /// Prazo para achar a `clientmessageid` antes de marcar como incerto.
    pub reconcile_after: Duration,
    /// Espera antes de refazer uma busca que falhou.
    pub fetch_retry_after: Duration,
    /// Espera antes de tentar reabrir o tempo real de novo.
    pub reconnect_retry_after: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            send_attempts: 3,
            reconcile_after: Duration::from_secs(60),
            fetch_retry_after: Duration::from_secs(5),
            reconnect_retry_after: Duration::from_secs(2),
        }
    }
}

/// O que um [`OpId`] está fazendo (para interpretar o resultado que voltar).
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpKind {
    Send(ClientMessageId),
    Fetch(ConversationId),
    Reconnect,
}

/// Máquina de estados do motor.
#[derive(Debug)]
pub struct SyncMachine {
    config: Config,
    connectivity: Connectivity,
    realtime: Realtime,
    next_op: u64,
    inflight: BTreeMap<OpId, OpKind>,
    conversations: BTreeMap<ConversationId, ConversationState>,
    sends: BTreeMap<ClientMessageId, SendState>,
    wake: Option<Instant>,
}

impl Default for SyncMachine {
    fn default() -> Self {
        Self {
            config: Config::default(),
            connectivity: Connectivity::Unknown,
            realtime: Realtime::Unknown,
            next_op: 0,
            inflight: BTreeMap::new(),
            conversations: BTreeMap::new(),
            sends: BTreeMap::new(),
            wake: None,
        }
    }
}

impl SyncMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: Config) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Único ponto de entrada. Função pura sobre o estado: `now` vem do `Clock` do driver.
    pub fn handle(&mut self, now: Instant, event: Event) -> Vec<Decision> {
        let mut out = Vec::new();
        match event {
            Event::SendRequested {
                conversation,
                client_id,
            } => self.on_send_requested(&conversation, &client_id, &mut out),
            Event::Invalidated { conversation } => self.on_invalidated(&conversation, &mut out),
            Event::MessageObserved {
                conversation,
                client_id,
            } => self.on_message_observed(&conversation, &client_id, &mut out),
            Event::OperationFinished { op, outcome } => {
                self.on_finished(now, op, outcome, &mut out)
            }
            Event::ConnectionLost => self.on_connection_lost(now, &mut out),
            Event::ConnectionRestored => self.on_connection_restored(&mut out),
            Event::WakeUp => self.on_wake(now, &mut out),
        }
        out
    }

    // ---- leitura (driver/UI e testes) ----

    pub fn connectivity(&self) -> Connectivity {
        self.connectivity
    }

    pub fn realtime(&self) -> Realtime {
        self.realtime
    }

    pub fn send_state(&self, client_id: &ClientMessageId) -> Option<&SendState> {
        self.sends.get(client_id)
    }

    /// Todos os envios conhecidos, em ordem estável de `clientmessageid`.
    ///
    /// É o que permite ao driver/UI perceber **transições** (ex.: algo virou `Uncertain`) sem que o
    /// motor precise emitir um evento só para isso.
    pub fn sends(&self) -> impl Iterator<Item = (&ClientMessageId, &SendState)> {
        self.sends.iter()
    }

    pub fn conversation_state(&self, conversation: &ConversationId) -> Option<&ConversationState> {
        self.conversations.get(conversation)
    }

    /// Próximo prazo que o driver deve respeitar (último [`Decision::ScheduleWake`] emitido).
    pub fn next_wake(&self) -> Option<Instant> {
        self.wake
    }

    // ---- transições ----

    fn on_send_requested(
        &mut self,
        conversation: &ConversationId,
        client_id: &ClientMessageId,
        out: &mut Vec<Decision>,
    ) {
        // Duplicado/fora de ordem: o pedido já existe, não vira segundo envio.
        if self.sends.contains_key(client_id) {
            return;
        }
        self.sends.insert(
            client_id.clone(),
            SendState::Pending {
                conversation: conversation.clone(),
                attempts: 0,
            },
        );
        if self.connectivity == Connectivity::Down {
            return; // fica na caixa de saída até a rede voltar
        }
        self.try_send(conversation, client_id, out);
    }

    fn on_invalidated(&mut self, conversation: &ConversationId, out: &mut Vec<Decision>) {
        // Já buscando: coalesce (uma busca serve para N invalidações), mas anota a nova.
        if let Some(ConversationState::Fetching { stale_again, .. }) =
            self.conversations.get_mut(conversation)
        {
            *stale_again = true;
            return;
        }
        self.conversations
            .insert(conversation.clone(), ConversationState::Dirty);
        self.start_fetch(conversation, out);
    }

    fn on_message_observed(
        &mut self,
        conversation: &ConversationId,
        client_id: &ClientMessageId,
        out: &mut Vec<Decision>,
    ) {
        // Só confirma o envio **daquela** conversa, e só se ele já saiu: um id igual em outro chat
        // (ou um envio nunca tentado) não é prova de entrega.
        let confirms = match self.sends.get(client_id) {
            Some(
                SendState::InFlight {
                    conversation: sent, ..
                }
                | SendState::Reconciling {
                    conversation: sent, ..
                }
                | SendState::Uncertain {
                    conversation: sent, ..
                },
            ) => sent == conversation,
            _ => false,
        };
        if !confirms {
            return;
        }
        self.sends.insert(
            client_id.clone(),
            SendState::Delivered {
                conversation: conversation.clone(),
            },
        );
        self.persist_and_publish(conversation, out);
    }

    fn on_finished(&mut self, now: Instant, op: OpId, outcome: Outcome, out: &mut Vec<Decision>) {
        // Operação desconhecida = resultado de um op já abandonado (queda, duplicado): ignora.
        let Some(kind) = self.inflight.remove(&op) else {
            return;
        };
        match kind {
            OpKind::Send(client_id) => self.finish_send(now, op, &client_id, outcome, out),
            OpKind::Fetch(conversation) => self.finish_fetch(now, op, &conversation, outcome, out),
            OpKind::Reconnect => self.finish_reconnect(now, outcome, out),
        }
    }

    fn finish_send(
        &mut self,
        now: Instant,
        op: OpId,
        client_id: &ClientMessageId,
        outcome: Outcome,
        out: &mut Vec<Decision>,
    ) {
        let Some(state) = self.sends.get(client_id) else {
            return;
        };
        // Só o op que está de fato em voo resolve este envio (descarta resultado atrasado).
        let in_flight = matches!(state, SendState::InFlight { op: current, .. } if *current == op);
        if !in_flight {
            return;
        }
        let conversation = state.conversation().clone();
        let attempts = state.attempts();

        match outcome {
            Outcome::Succeeded => {
                self.sends.insert(
                    client_id.clone(),
                    SendState::Delivered {
                        conversation: conversation.clone(),
                    },
                );
                self.persist_and_publish(&conversation, out);
            }
            // Resposta perdida: **reconciliar**, nunca reenviar (SEND-RECONCILE-001).
            Outcome::Lost => {
                let deadline = now + self.config.reconcile_after;
                self.sends.insert(
                    client_id.clone(),
                    SendState::Reconciling {
                        conversation: conversation.clone(),
                        deadline,
                    },
                );
                self.start_fetch(&conversation, out);
                self.schedule(deadline, out);
            }
            Outcome::Failed { retryable } => {
                if retryable && attempts < self.config.send_attempts {
                    self.sends.insert(
                        client_id.clone(),
                        SendState::Pending {
                            conversation: conversation.clone(),
                            attempts,
                        },
                    );
                    self.try_send(&conversation, client_id, out);
                } else {
                    let reason = if retryable {
                        UncertainReason::RetriesExhausted
                    } else {
                        UncertainReason::Refused
                    };
                    self.mark_uncertain(client_id, &conversation, reason, out);
                }
            }
        }
    }

    fn finish_fetch(
        &mut self,
        now: Instant,
        op: OpId,
        conversation: &ConversationId,
        outcome: Outcome,
        out: &mut Vec<Decision>,
    ) {
        let (same_op, stale_again) = match self.conversations.get(conversation) {
            Some(ConversationState::Fetching {
                op: current,
                stale_again,
            }) => (*current == op, *stale_again),
            _ => return, // resultado atrasado de uma busca anterior
        };
        if !same_op {
            return;
        }

        match outcome {
            // O conteúdo local pode ser considerado fresco — mas isso **não** confirma envio nenhum:
            // entrega é confirmada por [`Event::MessageObserved`].
            Outcome::Succeeded | Outcome::Failed { retryable: false } => {
                self.conversations
                    .insert(conversation.clone(), ConversationState::Fresh);
            }
            Outcome::Failed { retryable: true } | Outcome::Lost => {
                self.conversations
                    .insert(conversation.clone(), ConversationState::Dirty);
                self.schedule(now + self.config.fetch_retry_after, out);
            }
        }

        // Chegou invalidação durante a busca: refazer agora que o estado é conhecido.
        if stale_again
            && matches!(
                self.conversations.get(conversation),
                Some(ConversationState::Fresh)
            )
        {
            self.conversations
                .insert(conversation.clone(), ConversationState::Dirty);
            self.start_fetch(conversation, out);
        }
    }

    fn finish_reconnect(&mut self, now: Instant, outcome: Outcome, out: &mut Vec<Decision>) {
        match outcome {
            Outcome::Succeeded => {
                self.realtime = Realtime::Connected;
                self.replay_gap(out);
            }
            // A capacidade `realtime` falhar **não** afeta histórico nem envio (§2.5).
            Outcome::Failed { .. } | Outcome::Lost => {
                self.realtime = Realtime::Reconnecting;
                self.schedule(now + self.config.reconnect_retry_after, out);
            }
        }
    }

    /// `REALTIME-GAP-001` (UNVERIFIED): entre a queda e a volta, as invalidações do Trouter que
    /// chegaram nesse intervalo **se perdem** — o registro do endpoint é refeito, mas o que passou
    /// não volta.
    ///
    /// Enquanto o cursor de replay (`lastSyncToken`) não tiver evidência, o caminho seguro é
    /// re-buscar tudo que estava fresco: os upserts são idempotentes, então refazer não duplica nem
    /// inventa mensagem. Custa uma busca por conversa aberta e nunca perde dado — a troca certa
    /// enquanto o protocolo é hipótese.
    fn replay_gap(&mut self, out: &mut Vec<Decision>) {
        let stale: Vec<ConversationId> = self
            .conversations
            .iter()
            .filter(|(_, state)| matches!(state, ConversationState::Fresh))
            .map(|(conversation, _)| conversation.clone())
            .collect();
        for conversation in stale {
            self.conversations
                .insert(conversation, ConversationState::Dirty);
        }
        self.flush(out);
    }

    fn on_connection_lost(&mut self, now: Instant, out: &mut Vec<Decision>) {
        self.connectivity = Connectivity::Down;
        self.realtime = Realtime::Reconnecting;
        // Nenhum resultado pendente é mais confiável neste ponto.
        self.inflight.clear();

        for state in self.conversations.values_mut() {
            if matches!(state, ConversationState::Fetching { .. }) {
                *state = ConversationState::Dirty;
            }
        }

        // Envio em voo cuja resposta não volta: reconciliação obrigatória.
        let unknown: Vec<ClientMessageId> = self
            .sends
            .iter()
            .filter(|(_, state)| matches!(state, SendState::InFlight { .. }))
            .map(|(client_id, _)| client_id.clone())
            .collect();
        for client_id in unknown {
            let Some(state) = self.sends.get(&client_id) else {
                continue;
            };
            let conversation = state.conversation().clone();
            let deadline = now + self.config.reconcile_after;
            self.sends.insert(
                client_id,
                SendState::Reconciling {
                    conversation: conversation.clone(),
                    deadline,
                },
            );
            self.start_fetch(&conversation, out);
            self.schedule(deadline, out);
        }
    }

    fn on_connection_restored(&mut self, out: &mut Vec<Decision>) {
        self.connectivity = Connectivity::Up;
        // A rede voltou, mas o socket do tempo real ainda não: reabrir.
        if self.realtime != Realtime::Connected && !self.reconnect_in_flight() {
            self.realtime = Realtime::Reconnecting;
            let op = self.allocate(OpKind::Reconnect);
            out.push(Decision::Reconnect { op });
        }
        // Envio e histórico não dependem do socket: seguem agora.
        self.flush(out);
    }

    fn on_wake(&mut self, now: Instant, out: &mut Vec<Decision>) {
        self.wake = None;

        // Reconciliações vencidas sem evidência: incerto, nunca "entregue".
        let expired: Vec<ClientMessageId> = self
            .sends
            .iter()
            .filter(|(_, state)| {
                matches!(state, SendState::Reconciling { deadline, .. } if *deadline <= now)
            })
            .map(|(client_id, _)| client_id.clone())
            .collect();
        for client_id in expired {
            let Some(state) = self.sends.get(&client_id) else {
                continue;
            };
            let conversation = state.conversation().clone();
            self.mark_uncertain(&client_id, &conversation, UncertainReason::NoEvidence, out);
        }

        self.flush(out);

        if self.realtime == Realtime::Reconnecting
            && self.connectivity != Connectivity::Down
            && !self.reconnect_in_flight()
        {
            let op = self.allocate(OpKind::Reconnect);
            out.push(Decision::Reconnect { op });
        }

        if let Some(next) = self.next_deadline() {
            self.schedule(next, out);
        }
    }

    // ---- auxiliares ----

    fn try_send(
        &mut self,
        conversation: &ConversationId,
        client_id: &ClientMessageId,
        out: &mut Vec<Decision>,
    ) {
        let attempts = match self.sends.get(client_id) {
            Some(SendState::Pending { attempts, .. }) => *attempts,
            Some(SendState::InFlight { attempts, .. }) => *attempts,
            _ => return,
        };
        let op = self.allocate(OpKind::Send(client_id.clone()));
        self.sends.insert(
            client_id.clone(),
            SendState::InFlight {
                conversation: conversation.clone(),
                op,
                attempts: attempts + 1,
            },
        );
        out.push(Decision::AttemptSend {
            op,
            conversation: conversation.clone(),
            client_id: client_id.clone(),
        });
    }

    fn start_fetch(&mut self, conversation: &ConversationId, out: &mut Vec<Decision>) {
        if matches!(
            self.conversations.get(conversation),
            Some(ConversationState::Fetching { .. })
        ) {
            return;
        }
        let op = self.allocate(OpKind::Fetch(conversation.clone()));
        self.conversations.insert(
            conversation.clone(),
            ConversationState::Fetching {
                op,
                stale_again: false,
            },
        );
        out.push(Decision::FetchHistory {
            op,
            conversation: conversation.clone(),
        });
    }

    /// Libera o que estava preso: envios pendentes e conversas sujas.
    fn flush(&mut self, out: &mut Vec<Decision>) {
        if self.connectivity == Connectivity::Down {
            return;
        }
        let pending: Vec<(ClientMessageId, ConversationId)> = self
            .sends
            .iter()
            .filter_map(|(client_id, state)| match state {
                SendState::Pending { conversation, .. } => {
                    Some((client_id.clone(), conversation.clone()))
                }
                _ => None,
            })
            .collect();
        for (client_id, conversation) in pending {
            self.try_send(&conversation, &client_id, out);
        }

        let dirty: Vec<ConversationId> = self
            .conversations
            .iter()
            .filter(|(_, state)| matches!(state, ConversationState::Dirty))
            .map(|(conversation, _)| conversation.clone())
            .collect();
        for conversation in dirty {
            self.start_fetch(&conversation, out);
        }
    }

    fn persist_and_publish(&self, conversation: &ConversationId, out: &mut Vec<Decision>) {
        out.push(Decision::Persist {
            conversation: conversation.clone(),
        });
        out.push(Decision::Publish {
            conversation: conversation.clone(),
        });
    }

    fn mark_uncertain(
        &mut self,
        client_id: &ClientMessageId,
        conversation: &ConversationId,
        reason: UncertainReason,
        out: &mut Vec<Decision>,
    ) {
        self.sends.insert(
            client_id.clone(),
            SendState::Uncertain {
                conversation: conversation.clone(),
                reason,
            },
        );
        self.persist_and_publish(conversation, out);
    }

    fn reconnect_in_flight(&self) -> bool {
        self.inflight
            .values()
            .any(|kind| matches!(kind, OpKind::Reconnect))
    }

    fn allocate(&mut self, kind: OpKind) -> OpId {
        self.next_op += 1;
        let op = OpId(self.next_op);
        self.inflight.insert(op, kind);
        op
    }

    /// Emite [`Decision::ScheduleWake`] só quando o prazo novo é mais cedo que o já agendado.
    fn schedule(&mut self, at: Instant, out: &mut Vec<Decision>) {
        let earlier = match self.wake {
            Some(current) => at < current,
            None => true,
        };
        if earlier {
            self.wake = Some(at);
            out.push(Decision::ScheduleWake { at });
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.sends
            .values()
            .filter_map(|state| match state {
                SendState::Reconciling { deadline, .. } => Some(*deadline),
                _ => None,
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clock, ManualClock};

    fn conv() -> ConversationId {
        ConversationId("19:abc@thread.v2".into())
    }

    fn cid() -> ClientMessageId {
        ClientMessageId("1700000000000000001".into())
    }

    fn send_op(decisions: &[Decision]) -> OpId {
        decisions
            .iter()
            .find_map(|d| match d {
                Decision::AttemptSend { op, .. } => Some(*op),
                _ => None,
            })
            .expect("uma decisão AttemptSend")
    }

    fn fetch_op(decisions: &[Decision]) -> OpId {
        decisions
            .iter()
            .find_map(|d| match d {
                Decision::FetchHistory { op, .. } => Some(*op),
                _ => None,
            })
            .expect("uma decisão FetchHistory")
    }

    fn reconnect_op(decisions: &[Decision]) -> OpId {
        decisions
            .iter()
            .find_map(|d| match d {
                Decision::Reconnect { op } => Some(*op),
                _ => None,
            })
            .expect("uma decisão Reconnect")
    }

    fn attempts(decisions: &[Decision]) -> usize {
        decisions
            .iter()
            .filter(|d| matches!(d, Decision::AttemptSend { .. }))
            .count()
    }

    fn persisted_and_published(decisions: &[Decision], conversation: &ConversationId) -> bool {
        let persist = decisions
            .iter()
            .any(|d| matches!(d, Decision::Persist { conversation: c } if c == conversation));
        let publish = decisions
            .iter()
            .any(|d| matches!(d, Decision::Publish { conversation: c } if c == conversation));
        persist && publish
    }

    #[test]
    fn send_is_attempted_then_delivered() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        let op = send_op(&decisions);
        assert_eq!(machine.send_state(&cid()).map(SendState::attempts), Some(1));

        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Succeeded,
            },
        );
        assert!(persisted_and_published(&decisions, &conv()));
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Delivered { .. })
        ));
    }

    #[test]
    fn lost_response_reconciles_instead_of_resending() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        let op = send_op(&decisions);

        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Lost,
            },
        );
        assert_eq!(attempts(&decisions), 0, "nunca reenviar sem evidência");
        fetch_op(&decisions); // busca histórico para achar a clientmessageid
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Reconciling { .. })
        ));

        // O driver busca o histórico e encontra a mensagem: prova de entrega.
        let decisions = machine.handle(
            clock.now(),
            Event::MessageObserved {
                conversation: conv(),
                client_id: cid(),
            },
        );
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Delivered { .. })
        ));
        assert!(persisted_and_published(&decisions, &conv()));

        // Resultado atrasado da operação perdida não muda nada.
        let late = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Lost,
            },
        );
        assert!(late.is_empty());
    }

    #[test]
    fn no_evidence_ends_uncertain_and_never_delivered() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        let op = send_op(&decisions);
        machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Lost,
            },
        );

        clock.advance(Duration::from_secs(61)); // passou do prazo de reconciliação
        let decisions = machine.handle(clock.now(), Event::WakeUp);

        assert_eq!(
            attempts(&decisions),
            0,
            "vencer o prazo não é licença para reenviar"
        );
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Uncertain {
                reason: UncertainReason::NoEvidence,
                ..
            })
        ));
        assert!(
            persisted_and_published(&decisions, &conv()),
            "a UI precisa saber"
        );
    }

    #[test]
    fn duplicate_send_request_does_not_create_a_second_attempt() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();
        let event = Event::SendRequested {
            conversation: conv(),
            client_id: cid(),
        };

        assert_eq!(attempts(&machine.handle(clock.now(), event.clone())), 1);
        assert!(machine.handle(clock.now(), event).is_empty());
    }

    #[test]
    fn invalidations_coalesce_and_then_refetch_once() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::Invalidated {
                conversation: conv(),
            },
        );
        let first = fetch_op(&decisions);

        // Segunda invalidação durante a busca: coalesce, não abre outra busca.
        let coalesced = machine.handle(
            clock.now(),
            Event::Invalidated {
                conversation: conv(),
            },
        );
        assert!(coalesced.is_empty());

        // Ao terminar, a invalidação coalescida ainda é atendida por uma nova busca.
        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op: first,
                outcome: Outcome::Succeeded,
            },
        );
        fetch_op(&decisions);
    }

    #[test]
    fn out_of_order_operation_finished_is_ignored() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op: OpId(9_999),
                outcome: Outcome::Succeeded,
            },
        );
        assert!(decisions.is_empty());
    }

    #[test]
    fn suspension_holds_a_queued_send_until_the_network_returns() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        assert!(machine
            .handle(clock.now(), Event::ConnectionLost)
            .is_empty());
        assert_eq!(machine.connectivity(), Connectivity::Down);

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        assert_eq!(attempts(&decisions), 0, "sem rede não se tenta enviar");
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Pending { .. })
        ));

        clock.advance(Duration::from_secs(8 * 3_600)); // 8 h de suspensão, sem esperar de verdade
        assert_eq!(attempts(&machine.handle(clock.now(), Event::WakeUp)), 0);

        let decisions = machine.handle(clock.now(), Event::ConnectionRestored);
        reconnect_op(&decisions);
        assert_eq!(attempts(&decisions), 1, "a caixa de saída é liberada");
    }

    #[test]
    fn retryable_failure_retries_then_ends_uncertain() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::with_config(Config {
            send_attempts: 2,
            ..Config::default()
        });

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        let first = send_op(&decisions);

        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op: first,
                outcome: Outcome::Failed { retryable: true },
            },
        );
        let second = send_op(&decisions);
        assert_eq!(machine.send_state(&cid()).map(SendState::attempts), Some(2));

        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op: second,
                outcome: Outcome::Failed { retryable: true },
            },
        );
        assert_eq!(attempts(&decisions), 0, "tentativas acabaram");
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Uncertain {
                reason: UncertainReason::RetriesExhausted,
                ..
            })
        ));
    }

    #[test]
    fn definitive_failure_is_uncertain_and_not_retried() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        let op = send_op(&decisions);

        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Failed { retryable: false },
            },
        );
        assert_eq!(attempts(&decisions), 0);
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Uncertain {
                reason: UncertainReason::Refused,
                ..
            })
        ));
    }

    #[test]
    fn realtime_failure_does_not_block_send_or_history() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        machine.handle(
            clock.now(),
            Event::Invalidated {
                conversation: conv(),
            },
        ); // conversa suja

        let decisions = machine.handle(clock.now(), Event::ConnectionRestored);
        let op = reconnect_op(&decisions);
        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Failed { retryable: true },
            },
        );

        assert_eq!(machine.realtime(), Realtime::Reconnecting);
        assert!(
            decisions
                .iter()
                .any(|d| matches!(d, Decision::ScheduleWake { .. })),
            "reagenda a reconexão"
        );

        // Envio continua disponível mesmo com o tempo real fora.
        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        assert_eq!(attempts(&decisions), 1);
        assert_eq!(machine.connectivity(), Connectivity::Up);
    }

    #[test]
    fn connection_loss_turns_in_flight_send_into_reconciliation() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        send_op(&decisions);

        let decisions = machine.handle(clock.now(), Event::ConnectionLost);
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Reconciling { .. })
        ));
        fetch_op(&decisions);
        assert!(decisions
            .iter()
            .any(|d| matches!(d, Decision::ScheduleWake { .. })));
        assert_eq!(attempts(&decisions), 0);
    }

    #[test]
    fn observation_from_another_conversation_is_not_proof() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        let op = send_op(&decisions);
        machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Lost,
            },
        );

        // Mesmo `clientmessageid`, outra conversa: não confirma nada.
        machine.handle(
            clock.now(),
            Event::MessageObserved {
                conversation: ConversationId("19:outra@thread.v2".into()),
                client_id: cid(),
            },
        );
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Reconciling { .. })
        ));
    }

    #[test]
    fn evidence_arriving_after_the_deadline_corrects_uncertain() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        let decisions = machine.handle(
            clock.now(),
            Event::SendRequested {
                conversation: conv(),
                client_id: cid(),
            },
        );
        let op = send_op(&decisions);
        machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Lost,
            },
        );
        clock.advance(Duration::from_secs(61));
        machine.handle(clock.now(), Event::WakeUp);
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Uncertain { .. })
        ));

        // Prova que chegou depois: corrigir para entregue é evidência, não invenção.
        let decisions = machine.handle(
            clock.now(),
            Event::MessageObserved {
                conversation: conv(),
                client_id: cid(),
            },
        );
        assert!(matches!(
            machine.send_state(&cid()),
            Some(SendState::Delivered { .. })
        ));
        assert!(persisted_and_published(&decisions, &conv()));
    }

    #[test]
    fn reconnect_replays_the_gap_by_refetching_what_was_fresh() {
        let clock = ManualClock::default();
        let mut machine = SyncMachine::new();

        // Uma conversa fresca (buscada com sucesso).
        let decisions = machine.handle(
            clock.now(),
            Event::Invalidated {
                conversation: conv(),
            },
        );
        let fetch = fetch_op(&decisions);
        machine.handle(
            clock.now(),
            Event::OperationFinished {
                op: fetch,
                outcome: Outcome::Succeeded,
            },
        );
        assert!(matches!(
            machine.conversation_state(&conv()),
            Some(ConversationState::Fresh)
        ));

        // Queda e volta; a reconexão dá certo (o registro do endpoint é refeito por baixo).
        machine.handle(clock.now(), Event::ConnectionLost);
        let decisions = machine.handle(clock.now(), Event::ConnectionRestored);
        let op = reconnect_op(&decisions);
        let decisions = machine.handle(
            clock.now(),
            Event::OperationFinished {
                op,
                outcome: Outcome::Succeeded,
            },
        );

        // O que estava fresco volta a ser buscado: invalidação perdida na queda não fica em
        // silêncio (`REALTIME-GAP-001`).
        fetch_op(&decisions);
        assert_eq!(machine.realtime(), Realtime::Connected);
    }

    #[test]
    fn machine_is_driven_by_an_injected_clock() {
        let manual = ManualClock::default();
        let mut machine = SyncMachine::new();

        let t0 = manual.now();
        machine.handle(t0, Event::ConnectionLost);

        manual.advance(Duration::from_secs(3_600)); // "suspensão" de 1h, sem esperar de verdade
        let t1 = manual.now();
        assert_eq!(t1 - t0, Duration::from_secs(3_600));

        machine.handle(t1, Event::WakeUp);
    }
}
