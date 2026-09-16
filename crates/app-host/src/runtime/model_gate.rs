//! Shared model execution gate: at most one model turn runs at a time
//! across the interactive chat turn and background durable-task agent
//! steps, which share one provider and would otherwise contend for
//! rate limits. Interactive turns take priority: a waiting interactive
//! turn blocks new background acquisitions until it is admitted.
use std::sync::Arc;
use std::sync::Mutex;
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

#[derive(Debug, Default)]
struct GateState {
    held: bool,
    interactive_waiters: usize,
}

/// Fair priority mutex for model turns. Construct once per process and
/// share between [`crate::runtime::AgentRuntime`] and
/// [`crate::runtime::task_agent::TaskAgentBackend`].
#[derive(Debug, Default)]
pub struct ModelExecutionGate {
    state: Mutex<GateState>,
    notify: Notify,
}

impl ModelExecutionGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit one turn. Background callers wait while the gate is held or
    /// any interactive turn is waiting; interactive callers wait only
    /// while held. Always completes (no timeout): the holder is a bounded
    /// model turn, never user input.
    pub async fn acquire(self: &Arc<Self>, kind: ModelTurnKind) -> ModelPermit {
        let mut counted = false;
        loop {
            {
                let mut state = self.lock_state();
                let priority_ok =
                    kind == ModelTurnKind::Interactive || state.interactive_waiters == 0;
                if !state.held && priority_ok {
                    state.held = true;
                    if counted && kind == ModelTurnKind::Interactive {
                        state.interactive_waiters -= 1;
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
}
