//! Shared model execution gate: at most one model turn runs at a time
//! across the interactive chat turn and background durable-task agent
//! steps, which share one provider and would otherwise contend for
//! rate limits. Interactive turns take priority, but background turns
//! age: after enough consecutive interactive admissions (or a long
//! enough wait) an old background waiter is admitted even while
//! interactive turns queue, so background work starves only briefly,
//! never forever.
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTurnKind {
    Interactive,
    Background,
}

impl ModelTurnKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelTurnKind::Interactive => "interactive",
            ModelTurnKind::Background => "background",
        }
    }
}

/// Aging bounds: how long interactive priority may starve background
/// turns before an old background waiter jumps the queue.
#[derive(Debug, Clone, Copy)]
pub struct GateConfig {
    /// Admit a background waiter that has waited at least this long,
    /// even while interactive turns queue.
    pub max_background_wait: Duration,
    /// Admit a background waiter after this many consecutive interactive
    /// admissions since the last background admission.
    pub max_consecutive_interactive: u32,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            max_background_wait: Duration::from_secs(30),
            max_consecutive_interactive: 8,
        }
    }
}

#[derive(Debug, Default)]
struct GateState {
    held: bool,
    interactive_waiters: usize,
    consecutive_interactive: u32,
    admissions_interactive: u64,
    admissions_background: u64,
    wait_ms_total_interactive: u64,
    wait_ms_total_background: u64,
    wait_ms_max_interactive: u64,
    wait_ms_max_background: u64,
}

/// Point-in-time fairness counters. Totals plus maxima show both the
/// typical queueing delay and the worst starvation each kind suffered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateSnapshot {
    pub held: bool,
    pub interactive_waiters: usize,
    pub consecutive_interactive: u32,
    pub admissions_interactive: u64,
    pub admissions_background: u64,
    pub wait_ms_total_interactive: u64,
    pub wait_ms_total_background: u64,
    pub wait_ms_max_interactive: u64,
    pub wait_ms_max_background: u64,
}

/// Fair priority mutex for model turns. Construct once per process and
/// share between [`crate::runtime::AgentRuntime`] and
/// [`crate::runtime::task_agent::TaskAgentBackend`].
#[derive(Debug)]
pub struct ModelExecutionGate {
    state: Mutex<GateState>,
    notify: Notify,
    config: GateConfig,
}

impl Default for ModelExecutionGate {
    fn default() -> Self {
        Self::with_config(GateConfig::default())
    }
}

impl ModelExecutionGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: GateConfig) -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            notify: Notify::new(),
            config,
        }
    }

    /// Current admission and queueing counters.
    pub fn snapshot(&self) -> GateSnapshot {
        let state = self.lock_state();
        GateSnapshot {
            held: state.held,
            interactive_waiters: state.interactive_waiters,
            consecutive_interactive: state.consecutive_interactive,
            admissions_interactive: state.admissions_interactive,
            admissions_background: state.admissions_background,
            wait_ms_total_interactive: state.wait_ms_total_interactive,
            wait_ms_total_background: state.wait_ms_total_background,
            wait_ms_max_interactive: state.wait_ms_max_interactive,
            wait_ms_max_background: state.wait_ms_max_background,
        }
    }

    /// Admit one turn. Background callers wait while the gate is held or
    /// any interactive turn is waiting — unless they have aged past the
    /// configured bounds, in which case they are admitted like any other
    /// waiter. Interactive callers wait only while held. Always completes
    /// (no timeout): the holder is a bounded model turn, never user input.
    pub async fn acquire(self: &Arc<Self>, kind: ModelTurnKind) -> ModelPermit {
        let mut counted = false;
        let started = Instant::now();
        loop {
            {
                let mut state = self.lock_state();
                let priority_ok = kind == ModelTurnKind::Interactive
                    || background_admissible(&state, &self.config, started.elapsed());
                if !state.held && priority_ok {
                    state.held = true;
                    if counted && kind == ModelTurnKind::Interactive {
                        state.interactive_waiters -= 1;
                    }
                    let waited_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    match kind {
                        ModelTurnKind::Interactive => {
                            state.consecutive_interactive =
                                state.consecutive_interactive.saturating_add(1);
                            state.admissions_interactive =
                                state.admissions_interactive.saturating_add(1);
                            state.wait_ms_total_interactive =
                                state.wait_ms_total_interactive.saturating_add(waited_ms);
                            state.wait_ms_max_interactive =
                                state.wait_ms_max_interactive.max(waited_ms);
                        }
                        ModelTurnKind::Background => {
                            state.consecutive_interactive = 0;
                            state.admissions_background =
                                state.admissions_background.saturating_add(1);
                            state.wait_ms_total_background =
                                state.wait_ms_total_background.saturating_add(waited_ms);
                            state.wait_ms_max_background =
                                state.wait_ms_max_background.max(waited_ms);
                        }
                    }
                    return ModelPermit {
                        gate: Arc::clone(self),
                    };
                }
                if kind == ModelTurnKind::Interactive && !counted {
                    state.interactive_waiters += 1;
                    counted = true;
                }
            }
            self.notify.notified().await;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, GateState> {
        match self.state.lock() {
            Ok(guard) => guard,
            // A poisoned gate means a holder panicked: break it open rather
            // than deadlocking every future model turn.
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn release(&self) {
        {
            let mut state = self.lock_state();
            state.held = false;
        }
        // Wake everyone: a single waking background waiter that still loses
        // priority would otherwise stall an eligible interactive waiter.
        self.notify.notify_waiters();
    }
}

/// Pure priority rule, kept out of the lock dance so tests pin it
/// directly: a background waiter is admissible when no interactive turn
/// queues, or when it has aged past either bound.
fn background_admissible(state: &GateState, config: &GateConfig, waited: Duration) -> bool {
    state.interactive_waiters == 0
        || state.consecutive_interactive >= config.max_consecutive_interactive
        || waited >= config.max_background_wait
}

/// Admission proof. Dropping releases the gate to the next waiter.
#[derive(Debug)]
pub struct ModelPermit {
    gate: Arc<ModelExecutionGate>,
}

impl Drop for ModelPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_acquire_is_immediate() {
        let gate = Arc::new(ModelExecutionGate::new());
        let _permit = gate.acquire(ModelTurnKind::Background).await;
        assert!(gate.lock_state().held);
    }

    #[tokio::test]
    async fn background_waits_for_interactive_priority() {
        let gate = Arc::new(ModelExecutionGate::new());
        let held = gate.acquire(ModelTurnKind::Background).await;

        // Background waiter arrives first, interactive second.
        let gate_bg = Arc::clone(&gate);
        let bg = tokio::spawn(async move { gate_bg.acquire(ModelTurnKind::Background).await });
        tokio::task::yield_now().await;
        let gate_it = Arc::clone(&gate);
        let it = tokio::spawn(async move { gate_it.acquire(ModelTurnKind::Interactive).await });
        // Let both reach the wait set before releasing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(held);

        // Interactive must win despite arriving second: it completes while
        // the background waiter is still parked, which only finishes after
        // the interactive permit drops.
        let interactive = tokio::time::timeout(std::time::Duration::from_secs(2), it)
            .await
            .expect("interactive admitted")
            .unwrap();
        assert!(gate.lock_state().held);
        drop(interactive);
        let _background = tokio::time::timeout(std::time::Duration::from_secs(2), bg)
            .await
            .expect("background admitted")
            .unwrap();
    }

    #[tokio::test]
    async fn release_wakes_a_waiter() {
        let gate = Arc::new(ModelExecutionGate::new());
        let held = gate.acquire(ModelTurnKind::Interactive).await;
        let gate2 = Arc::clone(&gate);
        let waiter = tokio::spawn(async move { gate2.acquire(ModelTurnKind::Interactive).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(held);
        let _permit = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiter wakes")
            .unwrap();
    }

    #[test]
    fn admissibility_prefers_interactive_until_aged() {
        let config = GateConfig {
            max_background_wait: Duration::from_secs(30),
            max_consecutive_interactive: 3,
        };
        let mut state = GateState::default();
        // No interactive waiters: background flows freely.
        assert!(background_admissible(&state, &config, Duration::ZERO));
        // An interactive waiter blocks a fresh background waiter.
        state.interactive_waiters = 1;
        assert!(!background_admissible(&state, &config, Duration::ZERO));
        // ...until either aging bound trips.
        state.consecutive_interactive = 3;
        assert!(background_admissible(&state, &config, Duration::ZERO));
        state.consecutive_interactive = 0;
        assert!(background_admissible(
            &state,
            &config,
            Duration::from_secs(30)
        ));
        assert!(!background_admissible(
            &state,
            &config,
            Duration::from_secs(29)
        ));
    }

    #[tokio::test]
    async fn snapshot_counts_admissions_and_waits() {
        let gate = Arc::new(ModelExecutionGate::new());
        let empty = gate.snapshot();
        assert_eq!(empty.admissions_interactive, 0);
        assert_eq!(empty.admissions_background, 0);

        let held = gate.acquire(ModelTurnKind::Background).await;
        let gate2 = Arc::clone(&gate);
        let waiter = tokio::spawn(async move { gate2.acquire(ModelTurnKind::Interactive).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(gate.snapshot().interactive_waiters, 1);
        drop(held);
        let permit = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("admitted")
            .unwrap();
        drop(permit);

        let snap = gate.snapshot();
        assert_eq!(snap.admissions_interactive, 1);
        assert_eq!(snap.admissions_background, 1);
        assert_eq!(snap.consecutive_interactive, 1);
        // The background acquire was immediate; the interactive one queued.
        assert_eq!(snap.wait_ms_max_background, 0);
        assert!(snap.wait_ms_max_interactive >= 20, "{snap:?}");
        assert_eq!(snap.wait_ms_total_interactive, snap.wait_ms_max_interactive);
    }

    #[tokio::test]
    async fn aged_background_breaks_continuous_interactive_pressure() {
        // Tight consecutive bound so the test converges in a few rounds
        // without depending on wall-clock aging.
        let gate = Arc::new(ModelExecutionGate::with_config(GateConfig {
            max_background_wait: Duration::from_secs(60),
            max_consecutive_interactive: 3,
        }));
        let held = gate.acquire(ModelTurnKind::Background).await;
        let gate_bg = Arc::clone(&gate);
        let mut bg = tokio::spawn(async move { gate_bg.acquire(ModelTurnKind::Background).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);

        // Keep exactly one interactive waiter queued at all times: without
        // aging the background waiter would starve forever.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bg_done = false;
        while Instant::now() < deadline && !bg_done {
            let gate_it = Arc::clone(&gate);
            let it = tokio::spawn(async move { gate_it.acquire(ModelTurnKind::Interactive).await });
            tokio::select! {
                permit = it => {
                    drop(permit.expect("interactive admitted"));
                }
                permit = &mut bg => {
                    drop(permit.expect("background admitted"));
                    bg_done = true;
                }
            }
        }
        assert!(bg_done, "background waiter starved under pressure");
        // The background admission resets the consecutive counter.
        assert_eq!(gate.snapshot().consecutive_interactive, 0);
        assert_eq!(gate.snapshot().admissions_background, 2);
    }
}
