//! Generation guards and current-turn event emission.
use super::{AgentRuntime, RuntimeError, State};
use ipc_core::HostEvent;

impl AgentRuntime {
    pub(crate) fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, State>, RuntimeError> {
        self.state
            .lock()
            .map_err(|_| RuntimeError::Executor("runtime state lock failed".to_string()))
    }
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.lock_state()
            .map(|state| state.generation == generation)
            .unwrap_or(false)
    }
    pub(crate) fn emit_if_current(&self, generation: u64, event: &str, data: serde_json::Value) {
        if self.is_current(generation) {
            (self.emit)(HostEvent {
                event: event.to_string(),
                data,
            });
        }
    }
    /// Clear this generation's worker handle (when still current), then emit
    /// one terminal turn event. Clearing first guarantees an observer that
    /// sees `agent.turn_done` / `agent.turn_failed` / `agent.turn_suspended`
    /// also sees idle `running` state instead of a stale handle.
    pub(crate) fn emit_terminal(&self, generation: u64, event: &str, data: serde_json::Value) {
        self.clear_running_if_current(generation);
        self.emit_if_current(generation, event, data);
    }
    /// Park the worker handle for `generation` once its turn has resolved
    /// (done, failed, suspended, or panicked). A superseding turn bumps the
    /// generation and owns its own handle, so only the matching generation
    /// may clear.
    pub(crate) fn clear_running_if_current(&self, generation: u64) {
        if let Ok(mut state) = self.state.lock() {
            if state.generation == generation {
                state.running = None;
            }
        }
    }
}
