//! Durable agent steps: one bounded model turn per step, through the same
//! `Agent` tool loop as chat turns but with task-scoped authority.
//!
//! Authority rule: the step authorizer auto-allows only what the standing
//! policy snapshot allows (pure tools bypass authorization entirely, as in
//! chat). Anything else suspends the turn with a pending approval, which
//! becomes a task `needs_review` carrying the exact capability request.
//! Approving via `task.review` stashes a single-use ticket; the retried
//! step resumes the turn transcript and the authorizer consumes the stash.
//! Policy denials (secret paths, etc.) never ask — they fail the call like
//! in chat.
//!
//! Attempt accounting: every approval round consumes one task attempt (the
//! executor counts `execute_task` runs), so agent steps that need N
//! approvals need `max_attempts` above N. Transcripts resume in memory;
//! after a crash the step replays from the prompt, bounded by attempts —
//! the same replay semantic as every other retried step.

use agent_core::{Agent, AgentError, AgentEvent, AgentLimits, ToolAuthorizer};
use model_core::ModelMessage;
use policy_core::{ApprovalQueue, AuthorizationContext, AuthorizationDecision};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use task_core::{StepContext, StepOutcome, Task, TaskStep};
use task_host::{AgentStepBackend, HostServices};

use super::model_gate::{ModelExecutionGate, ModelTurnKind};
use super::providers::ProviderFactory;
use super::EmitFn;

pub const TASK_AGENT_PROGRESS_EVENT: &str = ipc_core::events::TASK_AGENT_PROGRESS;
pub const TASK_AGENT_ID: &str = "task-agent";
pub const DEFAULT_STEP_ITERATIONS: usize = 8;
pub const MAX_STEP_ITERATIONS: usize = 25;
const MAX_STASHED_TRANSCRIPTS: usize = 64;
const APPROVAL_TICKET_TTL: Duration = Duration::from_secs(60);

const DEFAULT_STEP_SYSTEM_PROMPT: &str = "You are executing one step of a durable task. Complete only this step, keep tool calls to the minimum necessary, and end with a short summary of what you did and the key result. Do not start work belonging to other steps.";

pub struct TaskAgentBackend {
    providers: ProviderFactory,
    registry: Arc<tool_core::ToolRegistry>,
    approvals: Arc<Mutex<ApprovalQueue>>,
    emit: EmitFn,
    services: Arc<HostServices>,
    transcripts: Mutex<HashMap<(String, String), Vec<ModelMessage>>>,
    model_gate: Mutex<Option<Arc<ModelExecutionGate>>>,
}

impl TaskAgentBackend {
    pub fn new(
        providers: ProviderFactory,
        registry: Arc<tool_core::ToolRegistry>,
        approvals: Arc<Mutex<ApprovalQueue>>,
        emit: EmitFn,
        services: Arc<HostServices>,
    ) -> Self {
        Self {
            providers,
            registry,
            approvals,
            emit,
            services,
            transcripts: Mutex::new(HashMap::new()),
            model_gate: Mutex::new(None),
        }
    }

    /// Install the process-shared model gate (same instance as the
    /// interactive runtime). Background turns take it with lower priority.
    /// Unset backends run ungated (tests, task-cli).
    pub fn set_model_gate(&self, gate: Arc<ModelExecutionGate>) {
        if let Ok(mut slot) = self.model_gate.lock() {
            *slot = Some(gate);
        }
    }

    async fn acquire_model_permit(
        &self,
        task_id: &str,
        step_id: &str,
    ) -> Option<super::model_gate::ModelPermit> {
        let gate = self.model_gate.lock().ok().and_then(|slot| slot.clone())?;
        let started = std::time::Instant::now();
        let permit = gate.acquire(ModelTurnKind::Background).await;
        tracing::info!(
            task_id = %task_id,
            step_id = %step_id,
            kind = "background",
            provider_wait_ms = started.elapsed().as_millis() as u64,
            "model permit acquired"
        );
        Some(permit)
    }

    fn policy_snapshot(&self) -> Option<AuthorizationContext> {
        self.approvals.lock().ok().map(|queue| queue.context())
    }

    fn take_transcript(&self, task_id: &str, step_id: &str) -> Option<Vec<ModelMessage>> {
        self.transcripts
            .lock()
            .ok()?
            .remove(&(task_id.to_string(), step_id.to_string()))
    }

    fn stash_transcript(&self, task_id: &str, step_id: &str, messages: Vec<ModelMessage>) {
        let Ok(mut transcripts) = self.transcripts.lock() else {
            return;
        };
        if transcripts.len() >= MAX_STASHED_TRANSCRIPTS {
            if let Some(first) = transcripts.keys().next().cloned() {
                transcripts.remove(&first);
            }
        }
        transcripts.insert((task_id.to_string(), step_id.to_string()), messages);
    }

    fn progress_sink(&self, task_id: String, step_id: String) -> agent_core::AgentEventSink {
        let emit = Arc::clone(&self.emit);
        Arc::new(move |event| {
            let (kind, data) = match event {
                AgentEvent::TextDelta(_) => return,
                AgentEvent::ToolStarted { id, name } => (
                    "tool_started",
                    serde_json::json!({ "id": id, "name": name }),
                ),
                AgentEvent::ToolFinished { id, name, ok } => (
                    "tool_finished",
                    serde_json::json!({ "id": id, "name": name, "ok": ok }),
                ),
            };
            emit(ipc_core::HostEvent {
                event: TASK_AGENT_PROGRESS_EVENT.to_string(),
                data: serde_json::json!({
                    "task_id": task_id,
                    "step_id": step_id,
                    "kind": kind,
                    "tool": data,
                }),
            });
        })
    }
}

#[async_trait::async_trait]
impl AgentStepBackend for TaskAgentBackend {
    async fn run_agent_step(
        &self,
        ctx: &StepContext,
        _task: &Task,
        step: &TaskStep,
    ) -> StepOutcome {
        let prompt = step
            .input
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if prompt.is_empty() {
            return StepOutcome::Failed {
                message: "agent step needs a non-empty string 'prompt'".to_string(),
                retryable: false,
            };
        }
        let system = step
            .input
            .get("system")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(DEFAULT_STEP_SYSTEM_PROMPT);
        let max_iterations = step
            .input
            .get("max_iterations")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_STEP_ITERATIONS)
            .clamp(1, MAX_STEP_ITERATIONS);

        let stashed = self.take_transcript(&ctx.task_id, &step.id);
        let resumed = stashed.is_some();
        let mut messages = stashed.unwrap_or_else(|| vec![ModelMessage::user(prompt.to_string())]);
        if resumed {
            // Resumed after an approval: the stashed transcript is the
            // pre-call transcript, so the model re-issues the approved call
            // and the stash lets it through this time.
            messages.push(ModelMessage::user(
                "The user approved the requested capability. Continue the task from where you left off.".to_string(),
            ));
        }

        let provider = match (self.providers)().await {
            Ok(provider) => provider,
            Err(err) => {
                return StepOutcome::Failed {
                    message: format!("model provider unavailable: {err}"),
                    retryable: false,
                };
            }
        };
        let Some(policy) = self.policy_snapshot() else {
            return StepOutcome::Failed {
                message: "approval queue lock failed".to_string(),
                retryable: true,
            };
        };
        let agent = Agent::new(provider)
            .with_agent_id(capability_core::AgentId::new(TASK_AGENT_ID))
            .with_system_prompt(system.to_string())
            .with_limits(AgentLimits {
                max_iterations,
                ..AgentLimits::default()
            })
            .with_event_sink(self.progress_sink(ctx.task_id.clone(), step.id.clone()));
        let authorizer = TaskStepAuthorizer {
            context: policy,
            services: Arc::clone(&self.services),
            task_id: ctx.task_id.clone(),
            step_id: step.id.clone(),
        };
        tracing::info!(
            task_id = %ctx.task_id,
            step_id = %step.id,
            "agent step started"
        );
        // Serialize against interactive chat turns on the shared gate (when
        // installed). Held for the whole bounded turn, not per call.
        let _model_permit = self.acquire_model_permit(&ctx.task_id, &step.id).await;
        let model_started = std::time::Instant::now();
        let outcome = agent
            .turn_with_tools_authorized(messages, &self.registry, &authorizer)
            .await;
        tracing::info!(
            task_id = %ctx.task_id,
            step_id = %step.id,
            model_ms = model_started.elapsed().as_millis() as u64,
            "agent step completed"
        );
        match outcome {
            Ok(outcome) => match outcome.pending_approval {
                Some(pending) => {
                    self.stash_transcript(&ctx.task_id, &step.id, outcome.messages);
                    let reason =
                        serde_json::to_string(&task_host::runners::CapabilityReviewRequest::new(
                            &pending.tool_call.name,
                            pending.capability,
                            pending.resource,
                            pending.reason.clone(),
                        ))
                        .unwrap_or(pending.reason);
                    StepOutcome::NeedsReview { reason }
                }
                None => StepOutcome::Completed(serde_json::json!({
                    "status": "success",
                    "text": outcome.text,
                    "truncated": outcome.truncated,
                    "executed": outcome.executed.iter().map(|step| {
                        serde_json::json!({
                            "id": step.id,
                            "name": step.name,
                            "output": step.output.content,
                        })
                    }).collect::<Vec<_>>(),
                    "tool_steps": outcome.tool_steps.iter().map(|step| {
                        serde_json::json!({
                            "id": step.id,
                            "name": step.name,
                            "status": step.status.as_str(),
                            "ok": step.ok,
                            "output": step.output,
                            "error": step.error,
                        })
                    }).collect::<Vec<_>>(),
                })),
            },
            Err(AgentError::Model(message)) => StepOutcome::Failed {
                // Model/transport failures are the retryable class; auth and
                // config failures burn attempts and then fail visibly.
                message,
                retryable: true,
            },
            Err(other) => StepOutcome::Failed {
                message: other.to_string(),
                retryable: false,
            },
        }
    }
}

/// Step-scoped authorizer: standing policy first, single-use task-review
/// ticket second, user approval (via task review) last. Policy denials
/// stay denials — secrets never escalate to a review prompt.
struct TaskStepAuthorizer {
    context: AuthorizationContext,
    services: Arc<HostServices>,
    task_id: String,
    step_id: String,
}

impl TaskStepAuthorizer {
    fn policy_decision(
        &self,
        principal: &capability_core::Principal,
        request: &capability_core::CapabilityRequest,
    ) -> AuthorizationDecision {
        policy_core::authorize(principal, request, &self.context)
    }

    fn stash_covers(&self, request: &capability_core::CapabilityRequest) -> bool {
        self.services.peek_ticket_allows(
            &self.task_id,
            &self.step_id,
            &request.capability,
            &request.resource,
        )
    }
}

impl ToolAuthorizer for TaskStepAuthorizer {
    fn authorize(
        &self,
        principal: &capability_core::Principal,
        request: &capability_core::CapabilityRequest,
    ) -> AuthorizationDecision {
        match self.policy_decision(principal, request) {
            AuthorizationDecision::Allow { .. } => AuthorizationDecision::Allow {
                ticket_ttl: APPROVAL_TICKET_TTL,
            },
            AuthorizationDecision::Deny { reason } => AuthorizationDecision::Deny { reason },
            AuthorizationDecision::RequireUserApproval { reason } => {
                if self.stash_covers(request) {
                    AuthorizationDecision::Allow {
                        ticket_ttl: APPROVAL_TICKET_TTL,
                    }
                } else {
                    AuthorizationDecision::RequireUserApproval { reason }
                }
            }
        }
    }

    fn commit(
        &self,
        principal: &capability_core::Principal,
        request: &capability_core::CapabilityRequest,
    ) -> bool {
        if matches!(
            self.policy_decision(principal, request),
            AuthorizationDecision::Allow { .. }
        ) {
            return true;
        }
        if self.stash_covers(request) {
            // Single use: the take must succeed or the approval is void.
            return self
                .services
                .take_ticket(
                    &self.task_id,
                    &self.step_id,
                    capability_core::InvocationId::fresh(),
                )
                .is_some();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_core::{
        FinishReason, ModelError, ModelProvider, ModelRequest, ModelStreamEvent, ToolCall,
    };
    use policy_core::ApprovalQueue;
    use task_core::{TaskStatus, TaskStepStatus, TaskStepType};
    use tool_core::{Tool, ToolContext, ToolEffect, ToolMetadata};

    /// Scripted provider: pops one scripted turn per `stream` call.
    struct ScriptProvider {
        turns: Mutex<Vec<Vec<ModelStreamEvent>>>,
        seen: Mutex<Vec<ModelRequest>>,
    }

    impl ScriptProvider {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self {
                turns: Mutex::new(turns.into_iter().rev().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for ScriptProvider {
        async fn stream(
            &self,
            request: ModelRequest,
        ) -> Result<model_core::ModelStream, ModelError> {
            self.seen.lock().unwrap().push(request);
            let turn = self.turns.lock().unwrap().pop().unwrap_or_else(|| {
                vec![ModelStreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }]
            });
            Ok(Box::pin(futures_util::stream::iter(
                turn.into_iter().map(Ok),
            )))
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("system.echo"),
                description: "echo".to_string(),
                input_schema: serde_json::json!({}),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        async fn invoke(
            &self,
            _ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
            Ok(tool_core::ToolOutput::json(args))
        }
    }

    struct GatedTool;

    #[async_trait::async_trait]
    impl Tool for GatedTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("test.gated"),
                description: "gated".to_string(),
                input_schema: serde_json::json!({}),
                effects: vec![ToolEffect::ExternalSideEffect],
            }
        }

        fn required_capability(
            &self,
            _args: &serde_json::Value,
        ) -> Option<tool_core::CapabilityRequirement> {
            Some(tool_core::CapabilityRequirement {
                capability: capability_core::Capability::NotificationSend,
                resource: capability_core::Resource::NotificationService,
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            _args: serde_json::Value,
        ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
            if !ctx.has_ticket(
                capability_core::Capability::NotificationSend,
                capability_core::Resource::NotificationService,
            ) {
                return Err(tool_core::ToolError::structured(
                    "test.gated",
                    "permission_required",
                    "needs approval",
                ));
            }
            Ok(tool_core::ToolOutput::json(
                serde_json::json!({ "ok": true }),
            ))
        }
    }

    fn registry() -> Arc<tool_core::ToolRegistry> {
        let mut registry = tool_core::ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        registry.register(Arc::new(GatedTool)).unwrap();
        Arc::new(registry)
    }

    fn backend_with(
        provider: ScriptProvider,
    ) -> (TaskAgentBackend, Arc<ScriptProvider>, Arc<HostServices>) {
        let provider = Arc::new(provider);
        let factory_provider = Arc::clone(&provider);
        let factory = super::super::providers::sync_factory(move || Ok(factory_provider.clone()));
        let registry = registry();
        let services = Arc::new(HostServices::new(registry.clone()));
        let backend = TaskAgentBackend::new(
            factory,
            registry,
            Arc::new(Mutex::new(ApprovalQueue::new())),
            Arc::new(|_| {}),
            services.clone(),
        );
        (backend, provider, services)
    }

    fn ctx() -> StepContext {
        StepContext {
            task_id: "task-1".to_string(),
            attempt: 1,
            timeout_ms: 30_000,
            now_ms: 1_000,
        }
    }

    fn task() -> Task {
        Task {
            id: "task-1".to_string(),
            title: "t".to_string(),
            instruction: "i".to_string(),
            status: TaskStatus::Running,
            priority: 50,
            created_at: 0,
            updated_at: 0,
            scheduled_at: None,
            started_at: None,
            finished_at: None,
            attempts: 1,
            max_attempts: 5,
            lease_until: None,
            next_attempt_at: None,
            wait_for: None,
            steps: Vec::new(),
            current_step_index: 0,
            result: None,
            last_error: None,
            verification: None,
            parent_task_id: None,
        }
    }

    fn step(input: serde_json::Value) -> TaskStep {
        TaskStep {
            id: "step-1".to_string(),
            step_type: TaskStepType::Agent,
            status: TaskStepStatus::Running,
            input,
            result: None,
            attempts: 1,
            max_attempts: 5,
            started_at: Some(1_000),
            finished_at: None,
            error: None,
        }
    }

    fn text_turn(text: &str) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::TextDelta(text.to_string()),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ]
    }

    fn tool_turn(id: &str, name: &str, args: serde_json::Value) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args.to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    }

    #[tokio::test]
    async fn pure_tool_turn_completes_with_transcript() {
        let (backend, provider, _) = backend_with(ScriptProvider::new(vec![
            tool_turn("c1", "system.echo", serde_json::json!({ "a": 1 })),
            text_turn("echoed it"),
        ]));
        let outcome = backend
            .run_agent_step(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "prompt": "echo {\"a\":1} and summarize" })),
            )
            .await;
        match outcome {
            StepOutcome::Completed(value) => {
                assert_eq!(value["status"], "success");
                assert!(value["text"].as_str().unwrap_or("").contains("echoed"));
                assert_eq!(value["executed"][0]["name"], "system.echo");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(provider.seen_count(), 2);
    }

    #[tokio::test]
    async fn gated_tool_parks_then_resumes_after_stashed_approval() {
        let (backend, _, services) = backend_with(ScriptProvider::new(vec![
            tool_turn("c1", "test.gated", serde_json::json!({})),
            // Resumed turn re-issues the approved call, then summarizes.
            tool_turn("c2", "test.gated", serde_json::json!({})),
            text_turn("done after approval"),
        ]));
        let input = serde_json::json!({ "prompt": "run the gated tool" });
        let parked = backend
            .run_agent_step(&ctx(), &task(), &step(input.clone()))
            .await;
        let reason = match parked {
            StepOutcome::NeedsReview { reason } => reason,
            other => panic!("expected needs_review, got {other:?}"),
        };
        let request =
            task_host::runners::CapabilityReviewRequest::parse(&reason).expect("structured");
        assert_eq!(request.tool, "test.gated");
        assert_eq!(
            request.capability,
            capability_core::Capability::NotificationSend
        );

        // Same ticket TaskHost::review would mint on approval.
        let invocation = capability_core::InvocationId::fresh();
        let ticket = capability_core::CapabilityTicket::mint(
            services.task_principal(),
            request.capability,
            capability_core::ResourceScope::new(vec![request.resource]),
            invocation,
            Duration::from_secs(60),
        );
        services.stash_ticket("task-1", "step-1", ticket, invocation);

        let done = backend.run_agent_step(&ctx(), &task(), &step(input)).await;
        match done {
            StepOutcome::Completed(value) => {
                assert_eq!(value["status"], "success");
                assert!(value["text"].as_str().unwrap_or("").contains("approval"));
            }
            other => panic!("expected completion after approval, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_prompt_fails_closed() {
        let (backend, _, _) = backend_with(ScriptProvider::new(vec![]));
        let outcome = backend
            .run_agent_step(&ctx(), &task(), &step(serde_json::json!({})))
            .await;
        assert!(matches!(
            outcome,
            StepOutcome::Failed {
                retryable: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn provider_failure_is_explicit() {
        let factory = super::super::providers::sync_factory(move || {
            Err(super::super::RuntimeError::ModelNotConfigured)
        });
        let registry = registry();
        let backend = TaskAgentBackend::new(
            factory,
            registry.clone(),
            Arc::new(Mutex::new(ApprovalQueue::new())),
            Arc::new(|_| {}),
            Arc::new(HostServices::new(registry)),
        );
        let outcome = backend
            .run_agent_step(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "prompt": "hi" })),
            )
            .await;
        match outcome {
            StepOutcome::Failed { message, retryable } => {
                assert!(!retryable);
                assert!(message.contains("model provider unavailable"), "{message}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}
