//! Política da captura, separada da janela para ser testável sem WebView: quando sondar o storage,
//! quando desistir e quais resultados de sonda podem virar sessão.

use crate::session::{is_trusted_origin, origin_of, Blocker};
use std::time::{Duration, Instant};

/// Agenda absoluta: eventos da janela (mouse, foco, redimensionar…) acordam o loop, mas não
/// empurram a próxima sonda nem o prazo — ambos são instantes fixos.
#[derive(Debug, Clone)]
pub(crate) struct Schedule {
    next_probe: Instant,
    deadline: Instant,
    every: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    Probe,
    TimedOut,
}

impl Schedule {
    pub fn new(start: Instant, timeout: Duration, every: Duration) -> Self {
        Self {
            next_probe: start,
            deadline: start + timeout,
            every,
        }
    }

    /// Chamado em TODO despertar do loop, seja qual for o motivo.
    pub fn on_wake(&mut self, now: Instant) -> Option<Wake> {
        if now >= self.deadline {
            return Some(Wake::TimedOut);
        }
        if now >= self.next_probe {
            self.next_probe = now + self.every;
            return Some(Wake::Probe);
        }
        None
    }

    /// Até quando o loop pode dormir.
    pub fn wait_until(&self) -> Instant {
        self.next_probe.min(self.deadline)
    }
}

/// Um resultado de sonda só conta se veio do documento atual (nenhuma navegação de topo desde que
/// foi pedido) e se tanto a página quanto a WebView estão numa origem confiável do Teams.
pub(crate) fn accept_probe(
    probe_generation: u64,
    current_generation: u64,
    page_origin: &str,
    webview_url: &str,
) -> bool {
    probe_generation == current_generation
        && is_trusted_origin(page_origin)
        && origin_of(webview_url).as_deref() == Some(page_origin)
}

/// Bloqueio só vira erro depois de persistir por `grace` (a página pode estar no meio da carga).
#[derive(Debug, Default)]
pub(crate) struct BlockerTimer {
    since: Option<(Blocker, Instant)>,
}

impl BlockerTimer {
    pub fn observe(
        &mut self,
        blocker: Option<Blocker>,
        now: Instant,
        grace: Duration,
    ) -> Option<Blocker> {
        match (blocker, self.since) {
            (None, _) => {
                self.since = None;
                None
            }
            (Some(b), Some((prev, t0))) if b == prev => {
                (now.duration_since(t0) >= grace).then_some(b)
            }
            (Some(b), _) => {
                self.since = Some((b, now));
                (grace.is_zero()).then_some(b)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    /// Achado do review: eventos mais frequentes que a sonda não podem adiá-la nem adiar o prazo.
    #[test]
    fn frequent_window_events_do_not_starve_probes_or_timeout() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0, Duration::from_secs(10), S);
        let mut probes = 0;
        let mut timed_out_at = None;
        // um evento a cada 100 ms durante 12 s
        for i in 0..=120u64 {
            let now = t0 + Duration::from_millis(100 * i);
            match s.on_wake(now) {
                Some(Wake::Probe) => probes += 1,
                Some(Wake::TimedOut) => {
                    timed_out_at = Some(now);
                    break;
                }
                None => assert!(s.wait_until() > now),
            }
        }
        assert_eq!(probes, 10, "uma sonda por segundo, de 0 s a 9 s");
        assert_eq!(timed_out_at, Some(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn wait_never_goes_past_deadline() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0, Duration::from_millis(1500), S);
        assert_eq!(s.on_wake(t0), Some(Wake::Probe));
        assert_eq!(s.on_wake(t0 + S), Some(Wake::Probe));
        assert_eq!(s.wait_until(), t0 + Duration::from_millis(1500));
    }

    #[test]
    fn probe_from_previous_document_is_discarded() {
        let url = "https://teams.cloud.microsoft/v2/";
        assert!(accept_probe(3, 3, "https://teams.cloud.microsoft", url));
        assert!(!accept_probe(2, 3, "https://teams.cloud.microsoft", url));
    }

    #[test]
    fn untrusted_page_cannot_deliver_a_session() {
        let evil = "https://phish.example";
        assert!(!accept_probe(1, 1, evil, "https://phish.example/login"));
        // página mente a origem: a URL nativa da WebView não confirma
        assert!(!accept_probe(
            1,
            1,
            "https://teams.cloud.microsoft",
            "https://phish.example/"
        ));
        // login/federação legítimos também não entregam sessão
        assert!(!accept_probe(
            1,
            1,
            "https://login.microsoftonline.com",
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize"
        ));
    }

    #[test]
    fn blocker_becomes_error_only_after_grace() {
        let t0 = Instant::now();
        let g = Duration::from_secs(20);
        let mut b = BlockerTimer::default();
        assert_eq!(b.observe(Some(Blocker::Encrypted), t0, g), None);
        assert_eq!(
            b.observe(Some(Blocker::Encrypted), t0 + Duration::from_secs(19), g),
            None
        );
        assert_eq!(b.observe(None, t0 + Duration::from_secs(19), g), None); // sumiu: zera
        assert_eq!(
            b.observe(Some(Blocker::Encrypted), t0 + Duration::from_secs(20), g),
            None
        );
        assert_eq!(
            b.observe(
                Some(Blocker::AmbiguousAccount),
                t0 + Duration::from_secs(21),
                g
            ),
            None,
            "bloqueio diferente reinicia a contagem"
        );
        assert_eq!(
            b.observe(
                Some(Blocker::AmbiguousAccount),
                t0 + Duration::from_secs(41),
                g
            ),
            Some(Blocker::AmbiguousAccount)
        );
    }
}
