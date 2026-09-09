//! Session lifecycle: transcript ownership, suspension, cancellation.
use super::providers::read_autonomous_full_access;
use super::{AgentRuntime, RuntimeError};
use agent_core::ToolReplayCache;
use ipc_core::HostEvent;
use model_core::ModelMessage;
use std::sync::{atomic::Ordering, Arc};

/// Transcript cap: oldest messages are dropped past this bound so a long
/// session cannot grow memory (or model context) without limit.
const MAX_TRANSCRIPT_MESSAGES: usize = 100;

/// A turn paused for the user, kept until the request is resolved.
pub(crate) struct Suspended {
    pub(crate) transcript: Vec<model_core::ModelMessage>,
    pub(crate) request_id: String,
    pub(crate) task_id: String,
    pub(crate) turn_id: String,
    pub(crate) system_prompt: Option<String>,
    pub(crate) replay_cache: Arc<ToolReplayCache>,
}

pub(crate) struct State {
    pub(crate) generation: u64,
    pub(crate) transcript: Vec<ModelMessage>,
    pub(crate) suspended: Option<Suspended>,
    pub(crate) running: Option<tokio::task::JoinHandle<()>>,
    pub(crate) task_id: Option<String>,
    pub(crate) turn_id: Option<String>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) replay_cache: Option<Arc<ToolReplayCache>>,
}

/// Input accepted from the native bridge. The frontend can send its current
/// text history and prompt context on the first native turn; subsequent
/// approval resumes use the host-owned transcript stored in `State`.
#[derive(Debug, Clone, Default)]
pub struct AgentRequest {
    pub text: String,
    pub history: Vec<ModelMessage>,
    pub system_prompt: Option<String>,
    pub append_user_message: bool,
}

impl AgentRuntime {
    pub(crate) fn refresh_autonomous_full_access(&self) -> bool {
        // A native runtime always has storage. If that source cannot be read,
        // fail closed instead of retaining a previously enabled high-impact
        // mode; a later turn can re-read the durable value and re-enable it.
        let enabled = if self.storage.is_some() {
            read_autonomous_full_access(self.storage.as_ref()).unwrap_or(false)
        } else {
            // Headless/test runtimes without storage can still use the live
            // setter, but they have no durable value to refresh.
            self.autonomous_full_access_enabled()
        };
        self.autonomous_full_access.store(enabled, Ordering::SeqCst);
        enabled
    }
    pub(crate) fn resume_suspended_for_autonomous_access(self: &Arc<Self>) {
        let request_id = match self.lock_state() {
            Ok(state) => state
                .suspended
                .as_ref()
                .map(|suspended| suspended.request_id.clone()),
            Err(_) => None,
        };
        let Some(request_id) = request_id else {
            return;
        };

        // Only withdraw a pending request that the queue itself identifies as
        // an Agent request. If another principal ever owns the id, it remains
        // pending for its normal authorization path.
        let withdrawn = self
            .approvals
            .lock()
            .ok()
            .and_then(|queue| queue.withdraw_agent(&request_id));
        if withdrawn.is_none() {
            return;
        }

        (self.emit)(HostEvent {
            event: "permission.dismissed".to_string(),
            data: serde_json::json!({
                "id": request_id,
                "reason": "autonomous_full_access",
            }),
        });
        self.notify_decided(&request_id, true);
    }
    /// Queue a user message and run the turn in the background. Supersedes
    /// any in-flight or suspended turn (their late events are dropped).
    pub fn send_message(self: &Arc<Self>, text: String) -> Result<(), RuntimeError> {
        self.send_request(AgentRequest {
            text,
            append_user_message: true,
            ..AgentRequest::default()
        })
    }
    /// Start a turn with optional frontend history and prompt context. The
    /// host becomes the owner of the transcript as soon as this request is
    /// accepted; history is only an initial synchronization payload.
    pub fn send_request(self: &Arc<Self>, request: AgentRequest) -> Result<(), RuntimeError> {
        let (transcript, generation, task_id, turn_id, system_prompt, replay_cache, old_task) = {
            let mut state = self.lock_state()?;
            state.generation += 1;
            if let Some(handle) = state.running.take() {
                handle.abort();
            }
            let old_task = state.task_id.take();
            state.suspended = None;
            let task_id = uuid::Uuid::new_v4().to_string();
            let turn_id = uuid::Uuid::new_v4().to_string();
            let replay_cache = Arc::new(ToolReplayCache::new());
            let mut transcript = if state.transcript.is_empty() && !request.history.is_empty() {
                request.history
            } else {
                state.transcript.clone()
            };
            if let Some(prompt) = &request.system_prompt {
                if transcript
                    .first()
                    .is_some_and(|message| message.role == model_core::ModelRole::System)
                {
                    transcript[0] = ModelMessage::system(prompt.clone());
                }
            }
            let already_contains_text = transcript.last().is_some_and(|message| {
                message.role == model_core::ModelRole::User && message.content == request.text
            });
            if request.append_user_message && !already_contains_text {
                transcript.push(ModelMessage::user(request.text));
            }
            state.transcript = transcript.clone();
            state.task_id = Some(task_id.clone());
            state.turn_id = Some(turn_id.clone());
            state.system_prompt = request.system_prompt.clone();
            state.replay_cache = Some(Arc::clone(&replay_cache));
            while state.transcript.len() > MAX_TRANSCRIPT_MESSAGES {
                let remove_at = if state
                    .transcript
                    .first()
                    .is_some_and(|message| message.role == model_core::ModelRole::System)
                {
                    1
                } else {
                    0
                };
                state.transcript.remove(remove_at);
            }
            (
                state.transcript.clone(),
                state.generation,
                task_id,
                turn_id,
                request.system_prompt,
                replay_cache,
                old_task,
            )
        };
        if let Some(old_task) = old_task {
            if let Ok(queue) = self.approvals.lock() {
                queue.end_task(&old_task);
            }
        }
        self.spawn_turn(
            transcript,
            generation,
            task_id,
            turn_id,
            system_prompt,
            replay_cache,
            None,
        )
    }
    /// Resume a suspended turn after its permission request was resolved.
    /// Approving re-runs the turn so the call executes under the new
    /// grant; denying notes the refusal so the model works around it.
    /// Resolving an unknown or already-superseded request is a no-op.
    pub fn notify_decided(self: &Arc<Self>, request_id: &str, approved: bool) {
        let (transcript, generation, task_id, turn_id, system_prompt, replay_cache) =
            match self.lock_state() {
                Ok(mut state) => {
                    let Some(suspended) = state.suspended.take() else {
                        return;
                    };
                    if suspended.request_id != request_id {
                        state.suspended = Some(suspended);
                        return;
                    }
                    state.generation += 1;
                    if let Some(handle) = state.running.take() {
                        handle.abort();
                    }
                    state.transcript = suspended.transcript.clone();
                    state.task_id = Some(suspended.task_id.clone());
                    state.turn_id = Some(suspended.turn_id.clone());
                    state.system_prompt = suspended.system_prompt.clone();
                    (
                        suspended.transcript,
                        state.generation,
                        suspended.task_id,
                        suspended.turn_id,
                        suspended.system_prompt,
                        suspended.replay_cache,
                    )
                }
                Err(_) => return,
            };
        let note = if approved {
            format!(
                "The user approved permission request {request_id}. Continue the task; \
                 re-issue the tool call if it is still needed."
            )
        } else {
            format!(
                "The user denied permission request {request_id}. Do not retry that \
                 exact call; work around it or explain what you need."
            )
        };
        // Resume failures are terminal for this turn: the failure event
        // already tells the frontend what happened.
        let _ = self.spawn_turn(
            transcript,
            generation,
            task_id,
            turn_id,
            system_prompt,
            replay_cache,
            Some(note),
        );
    }
    /// Abort the in-flight turn and drop any suspended one. Late worker
    /// events are suppressed by the generation bump.
    pub fn cancel(self: &Arc<Self>) {
        let (generation, task_id) = match self.lock_state() {
            Ok(mut state) => {
                state.generation += 1;
                if let Some(handle) = state.running.take() {
                    handle.abort();
                }
                state.suspended = None;
                state.replay_cache = None;
                (state.generation, state.task_id.take())
            }
            Err(_) => return,
        };
        if let Some(task_id) = task_id {
            if let Ok(queue) = self.approvals.lock() {
                queue.end_task(&task_id);
            }
        }
        // The cancelling generation is current by construction.
        (self.emit)(HostEvent {
            event: "agent.turn_cancelled".to_string(),
            data: serde_json::json!({}),
        });
        let _ = generation;
    }
}
