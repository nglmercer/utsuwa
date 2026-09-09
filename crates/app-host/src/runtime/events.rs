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
}
