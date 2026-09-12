//! Turn execution: provider calls, tool snapshots, outcomes.
use super::authorization::QueueAuthorizer;
use super::prompts::{compose_host_system_prompt_for_context, log_host_environment};
use super::providers::configured_tool_profile;
use super::session::Suspended;
use super::{AgentRuntime, RuntimeError};
use agent_core::{Agent, AgentEvent, AgentLimits, ToolReplayCache};
use file_target::{FileRef, FileResolver, TargetPurpose};
use host_core::HostEnvironment;
use ipc_core::HostEvent;
use model_core::ModelMessage;
use std::sync::Arc;
use tool_sdk::ToolLoadContext;
use tracing::Instrument as _;

impl AgentRuntime {
    fn build_agent(
        &self,
        generation: u64,
        system_prompt: Option<String>,
    ) -> Result<Agent, RuntimeError> {
        let provider = (self.provider_factory)()?;
        let mut agent = Agent::new(provider)
            .with_agent_id(self.agent_id.clone())
            .with_limits(AgentLimits::default());
        agent = agent.with_artifact_store(self.artifacts.clone());
        let captures = self.computer_sessions.clone();
        agent = agent.with_desktop_action_notifier(Arc::new(move || {
            let captures = captures.clone();
            Box::pin(async move { captures.notify_all_actions().await })
        }));
        if let Some(prompt) = system_prompt {
            agent = agent.with_system_prompt(prompt);
        }
        if let Some(sink) = &self.audit {
            agent = agent.with_audit_sink(sink.clone());
        }
        let state = Arc::clone(&self.state);
        let emit = Arc::clone(&self.emit);
        agent = agent.with_event_sink(Arc::new(move |event| {
            let current = state
                .lock()
                .map(|state| state.generation == generation)
                .unwrap_or(false);
            if !current {
                return;
            }
            let (name, data) = match event {
                AgentEvent::TextDelta(delta) => {
                    ("agent.text_delta", serde_json::json!({ "delta": delta }))
                }
                AgentEvent::ToolStarted { id, name } => (
                    "agent.tool_started",
                    serde_json::json!({ "id": id, "name": name }),
                ),
                AgentEvent::ToolFinished { id, name, ok } => (
                    "agent.tool_finished",
                    serde_json::json!({ "id": id, "name": name, "ok": ok }),
                ),
            };
            emit(HostEvent {
                event: name.to_string(),
                data,
            });
        }));
        Ok(agent)
    }
    pub(crate) fn spawn_turn(
        self: &Arc<Self>,
        transcript: Vec<ModelMessage>,
        generation: u64,
        task_id: String,
        turn_id: String,
        system_prompt: Option<String>,
        replay_cache: Arc<ToolReplayCache>,
        resume_note: Option<String>,
    ) -> Result<(), RuntimeError> {
        let this = Arc::clone(self);
        let executor_handle = this.executor.handle().clone();
        let handle = executor_handle.spawn(async move {
            this.run_turn(
                transcript,
                generation,
                task_id,
                turn_id,
                system_prompt,
                replay_cache,
                resume_note,
            )
            .await;
        });
        self.lock_state()?.running = Some(handle);
        Ok(())
    }
    async fn run_turn(
        &self,
        transcript: Vec<ModelMessage>,
        generation: u64,
        task_id: String,
        turn_id: String,
        system_prompt: Option<String>,
        replay_cache: Arc<ToolReplayCache>,
        resume_note: Option<String>,
    ) {
        let span = tracing::info_span!("host.turn", generation, resumed = resume_note.is_some());
        self.run_turn_inner(
            transcript,
            generation,
            task_id,
            turn_id,
            system_prompt,
            replay_cache,
            resume_note,
        )
        .instrument(span)
        .await
    }
    async fn run_turn_inner(
        &self,
        mut transcript: Vec<ModelMessage>,
        generation: u64,
        task_id: String,
        turn_id: String,
        system_prompt: Option<String>,
        replay_cache: Arc<ToolReplayCache>,
        resume_note: Option<String>,
    ) {
        if let Some(note) = resume_note {
            transcript.push(ModelMessage::user(note));
        }
        // Refresh from native storage at the start of every turn. The atomic
        // mirror is also updated by settings.set, so a toggle during an active
        // turn is observed by the next authorization request immediately.
        let autonomous_full_access = self.refresh_autonomous_full_access();
        let tool_profile = configured_tool_profile(self.storage.as_ref());
        let host_environment = HostEnvironment::snapshot();
        log_host_environment(&host_environment, tool_profile);
        let history_system_prompt = transcript
            .first()
            .filter(|message| message.role == model_core::ModelRole::System)
            .map(|message| message.content.as_str());
        let base_system_prompt = system_prompt.as_deref().or(history_system_prompt);
        let host_system_prompt = compose_host_system_prompt_for_context(
            base_system_prompt,
            autonomous_full_access,
            &host_environment,
            Some(&self.file_context),
        );
        // `Agent` intentionally leaves an existing system message alone. A
        // native caller may supply one in history, so replace that message
        // here to guarantee the trusted host context is present in every
        // native turn without discarding the user's character prompt.
        if transcript
            .first()
            .is_some_and(|message| message.role == model_core::ModelRole::System)
        {
            transcript[0] = ModelMessage::system(host_system_prompt.clone());
        }
        let agent = match self.build_agent(generation, Some(host_system_prompt)) {
            Ok(agent) => agent,
            Err(err) => {
                if self.is_current(generation) {
                    if let Ok(queue) = self.approvals.lock() {
                        queue.end_task(&task_id);
                    }
                }
                self.emit_if_current(
                    generation,
                    "agent.turn_failed",
                    serde_json::json!({ "error": err.to_string() }),
                );
                return;
            }
        };
        let agent = agent.with_replay_cache(replay_cache.clone());
        // Per-turn immutable tool snapshot: the catalog composes static
        // packs (system, process), the required builtin filesystem surface,
        // and best-effort sources (MCP, plugins, memory, desktop), then
        // applies the model-facing profile centrally. A failing core
        // source fails the turn; a failing extension source only skips.
        let registry = match self
            .tool_catalog(&host_environment)
            .await
            .snapshot(&ToolLoadContext::new(tool_profile))
            .await
        {
            Ok(registry) => registry,
            Err(err) => {
                if self.is_current(generation) {
                    if let Ok(queue) = self.approvals.lock() {
                        queue.end_task(&task_id);
                    }
                }
                self.emit_if_current(
                    generation,
                    "agent.turn_failed",
                    serde_json::json!({ "error": err.to_string() }),
                );
                return;
            }
        };
        let tool_ids: Vec<String> = registry
            .list()
            .into_iter()
            .map(|metadata| metadata.id.0)
            .collect();
        tracing::debug!(
            native_bridge = true,
            registered_tool_count = tool_ids.len(),
            registered_tool_ids = ?tool_ids,
            "native agent tool registry ready"
        );
        let authorizer = QueueAuthorizer {
            approvals: Arc::clone(&self.approvals),
            task_id: task_id.clone(),
            turn_id: turn_id.clone(),
            autonomous_full_access: Arc::clone(&self.autonomous_full_access),
        };
        match agent
            .turn_with_tools_authorized(transcript, &registry, &authorizer)
            .await
        {
            Err(err) => {
                if self.is_current(generation) {
                    if let Ok(queue) = self.approvals.lock() {
                        queue.end_task(&task_id);
                    }
                }
                self.emit_if_current(
                    generation,
                    "agent.turn_failed",
                    serde_json::json!({ "error": err.to_string() }),
                );
            }
            Ok(outcome) => {
                self.record_file_context(&outcome.executed, &host_environment);
                if self.is_current(generation) {
                    if let Ok(mut state) = self.state.lock() {
                        state.transcript = outcome.messages.clone();
                    }
                }
                match outcome.pending_approval {
                    Some(pending) => {
                        if !self.is_current(generation) {
                            return;
                        }
                        let request = match self.approvals.lock() {
                            Ok(queue) => queue.submit_for_task(
                                agent.principal(),
                                pending.capability.clone(),
                                pending.resource.clone(),
                                pending.reason.clone(),
                                Some(task_id.clone()),
                                Some(turn_id.clone()),
                            ),
                            Err(_) => {
                                if let Ok(queue) = self.approvals.lock() {
                                    queue.end_task(&task_id);
                                }
                                self.emit_if_current(
                                    generation,
                                    "agent.turn_failed",
                                    serde_json::json!({ "error": "approval queue lock failed" }),
                                );
                                return;
                            }
                        };
                        if let Ok(mut state) = self.state.lock() {
                            if state.generation == generation {
                                state.suspended = Some(Suspended {
                                    transcript: outcome.messages.clone(),
                                    request_id: request.id.clone(),
                                    task_id: task_id.clone(),
                                    turn_id: turn_id.clone(),
                                    system_prompt: system_prompt.clone(),
                                    replay_cache: replay_cache.clone(),
                                });
                            }
                        }
                        match serde_json::to_value(&request) {
                            Ok(data) => {
                                self.emit_if_current(generation, "permission.requested", data)
                            }
                            Err(err) => tracing::warn!(%err, "cannot serialize permission request"),
                        }
                        self.emit_if_current(
                            generation,
                            "agent.turn_suspended",
                            serde_json::json!({
                                "text": outcome.text,
                                "request_id": request.id,
                                "tool_steps": outcome.tool_steps.iter().map(serialize_tool_step).collect::<Vec<_>>(),
                            }),
                        );
                    }
                    None => {
                        if self.is_current(generation) {
                            if let Ok(queue) = self.approvals.lock() {
                                queue.end_task(&task_id);
                            }
                        }
                        let executed: Vec<serde_json::Value> = outcome
                            .executed
                            .iter()
                            .map(|step| {
                                serde_json::json!({
                                    "id": step.id,
                                    "name": step.name,
                                    "output": step.output.content,
                                    "content_parts": step.output.parts,
                                })
                            })
                            .collect();
                        let tool_steps: Vec<serde_json::Value> =
                            outcome.tool_steps.iter().map(serialize_tool_step).collect();
                        self.emit_if_current(
                            generation,
                            "agent.turn_done",
                            serde_json::json!({
                                "text": outcome.text,
                                "executed": executed,
                                "tool_steps": tool_steps,
                                "truncated": outcome.truncated,
                            }),
                        );
                    }
                }
            }
        }
    }
    fn record_file_context(
        &self,
        executed: &[agent_core::ExecutedTool],
        environment: &HostEnvironment,
    ) {
        let resolver = FileResolver::new(environment.clone());
        let Ok(mut context) = self.file_context.lock() else {
            return;
        };
        for step in executed {
            if !step.name.starts_with("filesystem.") {
                continue;
            }
            let Some(object) = step.output.content.as_object() else {
                continue;
            };
            if let Some(file_ref) = object.get("file_ref").and_then(serde_json::Value::as_str) {
                if let Ok(file_ref) = FileRef::parse(file_ref) {
                    context.record_success(file_ref);
                    continue;
                }
            }
            let Some(path) = object.get("path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if let Ok(resolved) = resolver.descriptor_for_absolute(
                std::path::Path::new(path),
                if step.name.contains("create") || step.name.contains("write") {
                    TargetPurpose::Create
                } else {
                    TargetPurpose::Existing
                },
            ) {
                context.record_success(resolved.file_ref);
            }
        }
    }
}
fn serialize_tool_step(step: &agent_core::ToolStep) -> serde_json::Value {
    serde_json::json!({
        "id": step.id.clone(),
        "name": step.name.clone(),
        "status": step.status.as_str(),
        "ok": step.ok,
        "output": step.output.clone(),
        "content_parts": step.parts.clone(),
        "error": step.error.clone(),
    })
}
