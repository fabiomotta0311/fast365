//! Disponibilidade **por capacidade** (`docs/ARQUITETURA.md` §2.5) — Fase 2, #18.
//!
//! O estado do cliente não é "conectado/desconectado": é um mapa capacidade → rota → estado, por
//! conta/tenant e por **geração de sessão**. Falha de uma capacidade não derruba as outras, e
//! expiração/logout invalidam o conhecimento daquela geração — inclusive a **rota**, que é
//! conhecimento de sessão (§2.5: "expiração/logout invalidam o conhecimento daquela geração").
//!
//! Fronteira: nada aqui faz I/O e nada lê o relógio (`now` entra por parâmetro, como no resto do
//! crate). Quem exercita a capacidade é o driver: ele relata com [`Capabilities::observe_ok`] /
//! [`Capabilities::observe_failure`], e [`Capabilities::project`] casa com o que o motor de sync já
//! sabe (rede e tempo real) — sem inventar o que não foi provado.

use crate::machine::{Connectivity, Realtime, SyncMachine};
use std::collections::BTreeMap;
use std::time::Instant;

/// Capacidade exposta à UI. O `Ord` dá ordem estável de exibição (o mapa é `BTreeMap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    ListConversations,
    ReadHistory,
    DownloadMedia,
    Realtime,
    Send,
}

impl Capability {
    /// Todas as capacidades, na ordem em que a UI mostra.
    pub const ALL: [Capability; 5] = [
        Capability::ListConversations,
        Capability::ReadHistory,
        Capability::DownloadMedia,
        Capability::Realtime,
        Capability::Send,
    ];

    /// Rótulo estável para log/diagnóstico (a tradução é da UI).
    pub fn label(self) -> &'static str {
        match self {
            Capability::ListConversations => "listar conversas",
            Capability::ReadHistory => "ler histórico",
            Capability::DownloadMedia => "baixar mídia",
            Capability::Realtime => "tempo real",
            Capability::Send => "enviar",
        }
    }
}

/// Rota **conhecida** para exercer a capacidade.
///
/// `Unknown` significa "não se conhece nesta geração de sessão" — e escrita **não** se descobre por
/// tentativa e erro (§2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// HTTP do Rust (`reqwest`) pelo executor autenticado.
    HttpRust,
    /// `fetch` dentro da WebView parqueada (§2.1) — o fallback que evita No-Go.
    InPage,
    /// WebSocket do Trouter.
    Socket,
    /// Sem rota conhecida.
    Unknown,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Route::HttpRust => "HTTP Rust",
            Route::InPage => "WebView (in-page)",
            Route::Socket => "socket",
            Route::Unknown => "desconhecida",
        }
    }
}

/// Por que a capacidade está degradada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedReason {
    /// Rede fora: é a única falha que atinge todas as capacidades.
    Network,
    /// Canal de tempo real caiu e está sendo reaberto.
    Reconnecting,
    /// O servidor recusou a rota atual.
    Refused,
}

/// Estado de uma capacidade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Ainda não exercitada nesta geração de sessão: não se afirma nada.
    Unprobed,
    Available,
    Degraded {
        reason: DegradedReason,
    },
    /// Exige login novo: o conhecimento da sessão expirou.
    NeedsAuth,
}

impl State {
    /// Como a UI escreve isso (degradação parcial explícita, §2.5).
    pub fn describe(&self) -> &'static str {
        match self {
            State::Unprobed => "não exercitada",
            State::Available => "ok",
            State::Degraded {
                reason: DegradedReason::Network,
            } => "sem rede",
            State::Degraded {
                reason: DegradedReason::Reconnecting,
            } => "reconectando",
            State::Degraded {
                reason: DegradedReason::Refused,
            } => "servidor recusou",
            State::NeedsAuth => "exige novo login",
        }
    }
}

/// Uma linha do mapa: estado + rota conhecida.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub state: State,
    pub route: Route,
    pub since: Instant,
}

/// Geração da sessão. Expiração/logout incrementa; nada de estado antigo sobrevive a isso.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionGeneration(pub u64);

/// Mapa capacidade → rota → estado (§2.5).
#[derive(Debug, Clone)]
pub struct Capabilities {
    generation: SessionGeneration,
    entries: BTreeMap<Capability, Entry>,
}

impl Capabilities {
    /// Começa sem afirmar nada: todas as capacidades `Unprobed`, com a rota **declarada** do §2.5.
    pub fn new(now: Instant, generation: SessionGeneration) -> Self {
        let entries = Capability::ALL
            .into_iter()
            .map(|capability| {
                let entry = Entry {
                    state: State::Unprobed,
                    route: default_route(capability),
                    since: now,
                };
                (capability, entry)
            })
            .collect();
        Self {
            generation,
            entries,
        }
    }

    pub fn generation(&self) -> SessionGeneration {
        self.generation
    }

    pub fn entry(&self, capability: Capability) -> Option<&Entry> {
        self.entries.get(&capability)
    }

    /// Iteração estável (ordem de [`Capability`]), para UI e testes.
    pub fn iter(&self) -> impl Iterator<Item = (Capability, &Entry)> {
        self.entries
            .iter()
            .map(|(capability, entry)| (*capability, entry))
    }

    /// A rota funcionou: a capacidade está disponível.
    pub fn observe_ok(&mut self, now: Instant, capability: Capability) {
        let route = match self.entries.get(&capability) {
            Some(entry) if entry.route != Route::Unknown => entry.route,
            // Re-login recupera o conhecimento declarado; a rota observada substitui depois.
            _ => default_route(capability),
        };
        self.set(capability, State::Available, route, now);
    }

    /// A rota falhou. Não mexe nas outras capacidades.
    pub fn observe_failure(
        &mut self,
        now: Instant,
        capability: Capability,
        reason: DegradedReason,
    ) {
        let route = match self.entries.get(&capability) {
            Some(entry) => entry.route,
            None => default_route(capability),
        };
        self.set(capability, State::Degraded { reason }, route, now);
    }

    /// Troca a rota conhecida (ex.: degradar de HTTP Rust para `InPage` após bloqueio).
    pub fn set_route(&mut self, now: Instant, capability: Capability, route: Route) {
        let state = match self.entries.get(&capability) {
            Some(entry) => entry.state.clone(),
            None => State::Unprobed,
        };
        self.set(capability, state, route, now);
    }

    /// Expiração/logout: forget tudo da geração anterior (estado **e** rota).
    ///
    /// Geração igual ou menor é ignorada — evento atrasado não ressuscita conhecimento velho.
    pub fn invalidate_session(&mut self, now: Instant, generation: SessionGeneration) {
        if generation <= self.generation {
            return;
        }
        self.generation = generation;
        for capability in Capability::ALL {
            self.set(capability, State::NeedsAuth, Route::Unknown, now);
        }
    }

    /// Casa com o que o motor já sabe: rede ([`Connectivity`]) e tempo real ([`Realtime`]).
    ///
    /// É projeção **parcial** de propósito: envio e histórico são por conversa/mensagem, então quem
    /// relata é o driver (`observe_*`). `NeedsAuth` nunca é rebaixado — falta de login é mais forte
    /// que falta de rede.
    pub fn project(&mut self, now: Instant, machine: &SyncMachine) {
        if machine.connectivity() == Connectivity::Down {
            for capability in Capability::ALL {
                self.downgrade(
                    capability,
                    State::Degraded {
                        reason: DegradedReason::Network,
                    },
                    now,
                );
            }
            return;
        }

        // Rede de volta: o que só estava sem rede passa a "não exercitada" (não se prova sozinho).
        for capability in Capability::ALL {
            if matches!(
                self.entries.get(&capability).map(|entry| &entry.state),
                Some(State::Degraded {
                    reason: DegradedReason::Network
                })
            ) {
                self.downgrade(capability, State::Unprobed, now);
            }
        }

        match machine.realtime() {
            Realtime::Connected => self.downgrade(Capability::Realtime, State::Available, now),
            Realtime::Reconnecting => self.downgrade(
                Capability::Realtime,
                State::Degraded {
                    reason: DegradedReason::Reconnecting,
                },
                now,
            ),
            Realtime::Unknown => {}
        }
    }

    /// Uma linha por capacidade, na ordem de exibição: `"tempo real: reconectando"`.
    pub fn summary(&self) -> Vec<String> {
        self.iter()
            .map(|(capability, entry)| {
                format!("{}: {}", capability.label(), entry.state.describe())
            })
            .collect()
    }

    /// Aplica um estado derivado, preservando `NeedsAuth` e a rota conhecida.
    fn downgrade(&mut self, capability: Capability, state: State, now: Instant) {
        let Some(entry) = self.entries.get(&capability) else {
            return;
        };
        if entry.state == State::NeedsAuth {
            return;
        }
        let route = entry.route;
        if entry.state == state {
            return; // nada mudou: não reescreve o `since`
        }
        self.set(capability, state, route, now);
    }

    fn set(&mut self, capability: Capability, state: State, route: Route, now: Instant) {
        self.entries.insert(
            capability,
            Entry {
                state,
                route,
                since: now,
            },
        );
    }
}

/// Rota declarada para cada capacidade (§2.5), ponto de partida antes de qualquer observação.
fn default_route(capability: Capability) -> Route {
    match capability {
        Capability::ListConversations | Capability::ReadHistory | Capability::Send => {
            Route::HttpRust
        }
        Capability::DownloadMedia => Route::InPage,
        Capability::Realtime => Route::Socket,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{Config, SyncMachine};
    use crate::{Clock, Event, ManualClock};

    fn caps(clock: &ManualClock) -> Capabilities {
        Capabilities::new(clock.now(), SessionGeneration(1))
    }

    fn state(caps: &Capabilities, capability: Capability) -> State {
        caps.entry(capability)
            .expect("capacidade no mapa")
            .state
            .clone()
    }

    #[test]
    fn starts_without_claiming_anything_and_with_the_declared_routes() {
        let clock = ManualClock::default();
        let caps = caps(&clock);

        for capability in Capability::ALL {
            assert_eq!(state(&caps, capability), State::Unprobed);
        }
        assert_eq!(
            caps.entry(Capability::ListConversations).map(|e| e.route),
            Some(Route::HttpRust)
        );
        assert_eq!(
            caps.entry(Capability::DownloadMedia).map(|e| e.route),
            Some(Route::InPage)
        );
        assert_eq!(
            caps.entry(Capability::Realtime).map(|e| e.route),
            Some(Route::Socket)
        );
    }

    #[test]
    fn one_capability_failing_does_not_touch_the_others() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.observe_ok(clock.now(), Capability::ReadHistory);

        caps.observe_failure(
            clock.now(),
            Capability::DownloadMedia,
            DegradedReason::Refused,
        );

        assert_eq!(
            state(&caps, Capability::DownloadMedia),
            State::Degraded {
                reason: DegradedReason::Refused
            }
        );
        assert_eq!(state(&caps, Capability::ReadHistory), State::Available);
        assert_eq!(state(&caps, Capability::Send), State::Unprobed);
    }

    #[test]
    fn session_expiry_forgets_state_and_routes() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.observe_ok(clock.now(), Capability::Send);

        caps.invalidate_session(clock.now(), SessionGeneration(2));

        assert_eq!(caps.generation(), SessionGeneration(2));
        for capability in Capability::ALL {
            assert_eq!(state(&caps, capability), State::NeedsAuth);
            assert_eq!(
                caps.entry(capability).map(|e| e.route),
                Some(Route::Unknown),
                "rota é conhecimento de sessão"
            );
        }
    }

    #[test]
    fn stale_generation_does_not_resurrect_old_knowledge() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.invalidate_session(clock.now(), SessionGeneration(2));
        caps.observe_ok(clock.now(), Capability::Send);

        caps.invalidate_session(clock.now(), SessionGeneration(2)); // duplicado
        caps.invalidate_session(clock.now(), SessionGeneration(1)); // atrasado

        assert_eq!(caps.generation(), SessionGeneration(2));
        assert_eq!(state(&caps, Capability::Send), State::Available);
    }

    #[test]
    fn network_drop_hits_everything_and_restore_asks_for_new_evidence() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.observe_ok(clock.now(), Capability::ReadHistory);
        let mut machine = SyncMachine::new();

        machine.handle(clock.now(), Event::ConnectionLost);
        caps.project(clock.now(), &machine);
        for capability in Capability::ALL {
            assert_eq!(
                state(&caps, capability),
                State::Degraded {
                    reason: DegradedReason::Network
                }
            );
        }

        machine.handle(clock.now(), Event::ConnectionRestored);
        caps.project(clock.now(), &machine);
        assert_eq!(
            state(&caps, Capability::ReadHistory),
            State::Unprobed,
            "voltar a rede não prova que a rota funciona"
        );
    }

    #[test]
    fn realtime_follows_the_machine() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        let mut machine = SyncMachine::new();

        machine.handle(clock.now(), Event::ConnectionRestored);
        caps.project(clock.now(), &machine);
        assert_eq!(
            state(&caps, Capability::Realtime),
            State::Degraded {
                reason: DegradedReason::Reconnecting
            }
        );

        // O driver informa que o socket voltou.
        let op = machine
            .handle(clock.now(), Event::WakeUp)
            .into_iter()
            .find_map(|d| match d {
                crate::Decision::Reconnect { op } => Some(op),
                _ => None,
            });
        if let Some(op) = op {
            machine.handle(
                clock.now(),
                Event::OperationFinished {
                    op,
                    outcome: crate::Outcome::Succeeded,
                },
            );
            caps.project(clock.now(), &machine);
            assert_eq!(state(&caps, Capability::Realtime), State::Available);
        }
    }

    #[test]
    fn auth_wins_over_a_later_network_event() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.invalidate_session(clock.now(), SessionGeneration(2));
        let mut machine = SyncMachine::new();

        machine.handle(clock.now(), Event::ConnectionLost);
        caps.project(clock.now(), &machine);
        for capability in Capability::ALL {
            assert_eq!(
                state(&caps, capability),
                State::NeedsAuth,
                "falta de login não pode virar 'sem rede'"
            );
        }
    }

    #[test]
    fn re_probe_after_login_recovers_only_what_was_exercised() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.invalidate_session(clock.now(), SessionGeneration(2));

        caps.observe_ok(clock.now(), Capability::Send);

        assert_eq!(state(&caps, Capability::Send), State::Available);
        assert_eq!(
            caps.entry(Capability::Send).map(|e| e.route),
            Some(Route::HttpRust),
            "a rota declarada volta com o login"
        );
        assert_eq!(state(&caps, Capability::ReadHistory), State::NeedsAuth);
    }

    #[test]
    fn summary_is_stable_and_reads_like_the_ui_needs() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.observe_ok(clock.now(), Capability::ReadHistory);
        caps.observe_failure(
            clock.now(),
            Capability::Realtime,
            DegradedReason::Reconnecting,
        );
        caps.invalidate_session(clock.now(), SessionGeneration(2));
        caps.observe_ok(clock.now(), Capability::ReadHistory);
        caps.observe_failure(
            clock.now(),
            Capability::Realtime,
            DegradedReason::Reconnecting,
        );

        assert_eq!(
            caps.summary(),
            vec![
                "listar conversas: exige novo login".to_string(),
                "ler histórico: ok".to_string(),
                "baixar mídia: exige novo login".to_string(),
                "tempo real: reconectando".to_string(),
                "enviar: exige novo login".to_string(),
            ]
        );
    }

    #[test]
    fn changing_route_keeps_the_state() {
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        caps.observe_ok(clock.now(), Capability::ListConversations);

        caps.set_route(clock.now(), Capability::ListConversations, Route::InPage);

        let entry = caps.entry(Capability::ListConversations).expect("existe");
        assert_eq!(entry.route, Route::InPage);
        assert_eq!(entry.state, State::Available);
    }

    #[test]
    fn config_is_not_needed_to_project() {
        // Guarda de desenho: a projeção só olha o estado de rede/tempo real do motor.
        let clock = ManualClock::default();
        let mut caps = caps(&clock);
        let machine = SyncMachine::with_config(Config::default());
        caps.project(clock.now(), &machine);
        assert_eq!(state(&caps, Capability::Realtime), State::Unprobed);
    }
}
