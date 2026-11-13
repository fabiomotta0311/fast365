//! Relógio injetável. Todo código do core que precisa de "agora" ou de esperar recebe um
//! `Arc<dyn Clock>` em vez de chamar o relógio de parede — assim testes controlam o tempo
//! (suspensão, expiração de token, backoff) de forma determinística, sem esperar de verdade.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::oneshot;

#[async_trait]
pub trait Clock: Send + Sync {
    /// Tempo monotônico — prazos, timeouts e backoff.
    fn now(&self) -> Instant;
    /// Tempo civil (UTC) — comparar com instantes externos, ex.: `exp` de um token.
    fn utc_now(&self) -> SystemTime;
    /// Espera `duration` no tempo deste relógio.
    async fn sleep(&self, duration: Duration);
}

/// Relógio real (produção).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Relógio manual para testes: o tempo só anda com [`ManualClock::advance`], que acorda todo
/// `sleep` cujo prazo já passou. Clones compartilham o mesmo tempo.
#[derive(Debug, Clone)]
pub struct ManualClock {
    inner: Arc<Mutex<ManualState>>,
}

#[derive(Debug)]
struct ManualState {
    base_instant: Instant,
    base_utc: SystemTime,
    elapsed: Duration,
    sleepers: Vec<(Duration, oneshot::Sender<()>)>,
}

impl ManualClock {
    /// Começa em `utc` (tempo civil) com o tempo monotônico parado.
    pub fn starting_at(utc: SystemTime) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ManualState {
                base_instant: Instant::now(),
                base_utc: utc,
                elapsed: Duration::ZERO,
                sleepers: Vec::new(),
            })),
        }
    }

    /// Avança o tempo e acorda os `sleep` vencidos.
    pub fn advance(&self, by: Duration) {
        let mut state = self.lock();
        state.elapsed += by;
        let now = state.elapsed;
        let (due, pending) = std::mem::take(&mut state.sleepers)
            .into_iter()
            .partition(|(deadline, _)| *deadline <= now);
        state.sleepers = pending;
        drop(state);
        for (_, waker) in due {
            let _ = waker.send(());
        }
    }

    /// Quantos `sleep` estão aguardando (útil para sincronizar o teste com a tarefa testada).
    pub fn pending_sleepers(&self) -> usize {
        self.lock().sleepers.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManualState> {
        // Envenenamento só acontece se um teste já entrou em pânico; propagar é o certo.
        self.inner.lock().expect("ManualClock envenenado")
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::starting_at(SystemTime::UNIX_EPOCH)
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        let state = self.lock();
        state.base_instant + state.elapsed
    }

    fn utc_now(&self) -> SystemTime {
        let state = self.lock();
        state.base_utc + state.elapsed
    }

    async fn sleep(&self, duration: Duration) {
        let rx = {
            let mut state = self.lock();
            if duration.is_zero() {
                return;
            }
            let (tx, rx) = oneshot::channel();
            let deadline = state.elapsed + duration;
            state.sleepers.push((deadline, tx));
            rx
        };
        // Erro só se o relógio for descartado com o sleep pendente: tratar como acordado.
        let _ = rx.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_only_moves_when_advanced() {
        let clock = ManualClock::starting_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        let t0 = clock.now();
        assert_eq!(clock.now(), t0);

        clock.advance(Duration::from_secs(90));

        assert_eq!(clock.now() - t0, Duration::from_secs(90));
        assert_eq!(
            clock.utc_now(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_090)
        );
    }

    #[tokio::test]
    async fn sleep_wakes_exactly_at_deadline() {
        let clock = ManualClock::default();
        let task = tokio::spawn({
            let clock = clock.clone();
            async move { clock.sleep(Duration::from_secs(60)).await }
        });
        while clock.pending_sleepers() == 0 {
            tokio::task::yield_now().await;
        }

        clock.advance(Duration::from_secs(59));
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "acordou antes do prazo");
        assert_eq!(clock.pending_sleepers(), 1);

        clock.advance(Duration::from_secs(1));
        task.await.expect("sleep deve terminar no prazo");
        assert_eq!(clock.pending_sleepers(), 0);
    }

    #[tokio::test]
    async fn zero_sleep_returns_immediately() {
        ManualClock::default().sleep(Duration::ZERO).await;
    }

    #[test]
    fn usable_as_trait_object() {
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let _ = clock.now();
    }
}
