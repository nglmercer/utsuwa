//! Native agent runtime (plan Phase 14+).
//!
//! The agent owns conversation state, streams text, and runs the bounded
//! model → tool → model loop behind the policy/capability boundary.

use artifact_core::ContentPart;
use audit_core::{AuditOutcome, AuditRecord, AuditSink};
use capability_core::{AgentId, CapabilityRequest, CapabilityTicket, InvocationId, Principal};
use futures_util::StreamExt;
use model_core::{
    ModelError, ModelMessage, ModelProvider, ModelRequest, ModelStreamEvent, ToolCall,
    ToolDefinition, ToolResult,
};
use policy_core::{AuthorizationContext, AuthorizationDecision};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tool_core::{ToolContext, ToolOutput, ToolRegistry};
use tracing::Instrument as _;

/// Hard limits enforced on every turn (plan Phase 36). More limits
/// (iterations, tool calls, wall-clock) join with the agent loop.
#[derive(Debug, Clone)]
pub struct AgentLimits {
    /// Maximum assistant text bytes accumulated per turn.
    pub max_output_bytes: usize,
    /// Maximum streamed events consumed per turn (runaway guard).
    pub max_stream_events: usize,
    /// Maximum model→tool→model iterations per turn (Task 12 loop bound).
    pub max_iterations: usize,
    /// Maximum tool executions per turn.
    pub max_tool_calls: usize,
    /// Maximum serialized bytes kept per tool result.
    ///
    /// Kept as a compatibility alias for callers that configured the
    /// original single-string limit. The effective text limit is the lower
    /// of this and `max_tool_text_output_bytes`.
    pub max_tool_output_bytes: usize,
    /// Maximum UTF-8 text/JSON bytes sent back to a provider for one tool.
    pub max_tool_text_output_bytes: usize,
    /// Maximum number of artifact parts sent for one tool result.
    pub max_tool_artifacts: usize,
    /// Maximum declared size of one artifact accepted by the agent loop.
    pub max_artifact_bytes: u64,
    /// Maximum dimensions accepted for an image artifact.
    pub max_image_width: u32,
    pub max_image_height: u32,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: 64 * 1024,
            max_stream_events: 4096,
            max_iterations: 8,
            max_tool_calls: 16,
            max_tool_output_bytes: 16 * 1024,
            max_tool_text_output_bytes: 16 * 1024,
            max_tool_artifacts: 4,
            max_artifact_bytes: 8 * 1024 * 1024,
            max_image_width: 4096,
            max_image_height: 4096,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AgentError {
    #[error("model error: {0}")]
    Model(String),
    #[error("turn exceeded max output bytes ({0})")]
    OutputTooLarge(usize),
    #[error("turn exceeded max stream events ({0})")]
    TooManyEvents(usize),
    #[error("turn cancelled")]
    Cancelled,
}

impl From<ModelError> for AgentError {
    fn from(err: ModelError) -> Self {
        match err {
            ModelError::Cancelled => AgentError::Cancelled,
            other => AgentError::Model(other.to_string()),
        }
    }
}

/// One completed model turn (no tools yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTurn {
    pub invocation_id: InvocationId,
    pub text: String,
    pub truncated: bool,
}

/// Events emitted while a native turn is running. The host maps these to
/// frontend events so text and tool status use the same runtime path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    TextDelta(String),
    ToolStarted { id: String, name: String },
    ToolFinished { id: String, name: String, ok: bool },
}

pub type AgentEventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Host-provided hook used to increase desktop observation cadence after a
/// control action. The agent core does not know how captures are managed; it
/// only invokes this after a desktop-control tool has had a chance to touch
/// the OS, including when the backend reports a failure after a partial act.
pub type DesktopActionNotifier =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

/// Task-scoped replay cache for tools with external effects. When approval
/// suspends a later model call, the host resumes with the transcript that
/// already contains earlier tool results. If the model repeats one of those
/// calls, return the original result instead of repeating its side effect.
#[derive(Default)]
pub struct ToolReplayCache {
    outputs: Mutex<HashMap<String, ToolOutput>>,
}

impl ToolReplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(
        &self,
        name: &str,
        args: &serde_json::Value,
        metadata: &tool_core::ToolMetadata,
    ) -> Option<ToolOutput> {
        if !is_side_effecting(metadata) {
            return None;
        }
        self.outputs
            .lock()
            .ok()
            .and_then(|outputs| outputs.get(&replay_key(name, args)).cloned())
    }

    fn insert(
        &self,
        name: &str,
        args: &serde_json::Value,
        metadata: &tool_core::ToolMetadata,
        output: ToolOutput,
    ) {
        if !is_side_effecting(metadata) {
            return;
        }
        if let Ok(mut outputs) = self.outputs.lock() {
            outputs.insert(replay_key(name, args), output);
        }
    }
}

fn is_side_effecting(metadata: &tool_core::ToolMetadata) -> bool {
    metadata
        .effects
        .iter()
        .any(|effect| !matches!(effect, tool_core::ToolEffect::ReadOnly))
}

fn is_desktop_control(metadata: &tool_core::ToolMetadata) -> bool {
    metadata
        .effects
        .iter()
        .any(|effect| matches!(effect, tool_core::ToolEffect::DesktopControl))
}

fn is_desktop_observation(name: &str) -> bool {
    matches!(
        name,
        "desktop.observe"
            | "desktop.screenshot"
            | "desktop.capture_frame"
            | "desktop.accessibility_tree"
            | "desktop.inspect"
    )
}

/// Stable enough for a repeated model call: object keys are sorted so a
/// semantically identical JSON object cannot evade the task replay guard by
/// changing key order.
fn replay_key(name: &str, args: &serde_json::Value) -> String {
    fn canonical(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => {
                let mut keys: Vec<_> = object.keys().collect();
                keys.sort_unstable();
                let mut sorted = serde_json::Map::new();
                for key in keys {
                    if let Some(value) = object.get(key) {
                        sorted.insert(key.clone(), canonical(value));
                    }
                }
                serde_json::Value::Object(sorted)
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(canonical).collect())
            }
            other => other.clone(),
        }
    }

    format!("{name}:{}", canonical(args))
}

/// Authorization boundary used by the tool loop. The ordinary public API
/// uses an immutable policy snapshot; the host supplies a queue-backed
/// implementation that consumes once grants before invocation.
pub trait ToolAuthorizer: Send + Sync {
    fn authorize(
        &self,
        principal: &Principal,
        request: &CapabilityRequest,
    ) -> AuthorizationDecision;

    /// Commit authority immediately before a tool can cause an effect.
    fn commit(&self, _principal: &Principal, _request: &CapabilityRequest) -> bool {
        true
    }

    /// Optional audit label for the authorization mode that allowed a
    /// capability-bearing call. Ordinary authorizers leave this unset;
    /// host-specific modes can identify themselves without weakening the
    /// ticket/broker boundary.
    fn authorization_mode(&self) -> Option<&'static str> {
        None
    }
}

struct ContextAuthorizer<'a> {
    context: &'a AuthorizationContext,
}

impl ToolAuthorizer for ContextAuthorizer<'_> {
    fn authorize(
        &self,
        principal: &Principal,
        request: &CapabilityRequest,
    ) -> AuthorizationDecision {
        policy_core::authorize(principal, request, self.context)
    }
}

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    agent_id: AgentId,
    system_prompt: Option<String>,
    limits: AgentLimits,
    audit: Option<Arc<dyn AuditSink>>,
    event_sink: Option<AgentEventSink>,
    desktop_action_notifier: Option<DesktopActionNotifier>,
    replay_cache: Option<Arc<ToolReplayCache>>,
    artifact_store: Option<Arc<dyn artifact_core::ArtifactStore>>,
}

impl Agent {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            provider,
            agent_id: AgentId::new(uuid::Uuid::new_v4().to_string()),
            system_prompt: None,
            limits: AgentLimits::default(),
            audit: None,
            event_sink: None,
            desktop_action_notifier: None,
            replay_cache: None,
            artifact_store: None,
        }
    }

    pub fn with_audit_sink(mut self, sink: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(sink);
        self
    }

    /// Pin the agent identity (the host reuses one id per process so
    /// approvals, grants, and audit records refer to the same agent
    /// across turns and across approval resumes).
    pub fn with_agent_id(mut self, id: AgentId) -> Self {
        self.agent_id = id;
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn with_limits(mut self, limits: AgentLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_event_sink(mut self, sink: AgentEventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Attach the host's capture-sampling hook. This is deliberately a
    /// callback rather than a capture-manager dependency so `agent-core`
    /// remains platform-neutral and the existing registry stays authoritative.
    pub fn with_desktop_action_notifier(mut self, notifier: DesktopActionNotifier) -> Self {
        self.desktop_action_notifier = Some(notifier);
        self
    }

    /// Reuse successful side-effect results for the lifetime of one host
    /// task. The cache is intentionally supplied by the host so it survives
    /// an approval suspension but never survives task cancellation/restart.
    pub fn with_replay_cache(mut self, cache: Arc<ToolReplayCache>) -> Self {
        self.replay_cache = Some(cache);
        self
    }

    /// Attach the per-host expiring artifact store. The handle is carried in
    /// each model request but never enters transcript state or provider JSON.
    pub fn with_artifact_store(mut self, store: Arc<dyn artifact_core::ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    fn emit(&self, event: AgentEvent) {
        if let Some(sink) = &self.event_sink {
            sink(event);
        }
    }

    /// Run one model turn: prepend the system prompt, stream the response,
    /// accumulate text. Tool calls in the response are ignored here — use
    /// [`Agent::turn_with_tools`] to execute them.
    pub async fn turn(&self, mut messages: Vec<ModelMessage>) -> Result<AgentTurn, AgentError> {
        if !messages
            .first()
            .is_some_and(|message| message.role == model_core::ModelRole::System)
        {
            if let Some(prompt) = &self.system_prompt {
                messages.insert(0, ModelMessage::system(prompt.clone()));
            }
        }
        let mut request = ModelRequest::new(messages);
        request.max_tokens = Some(1024);
        request.artifact_store = self.artifact_store.clone();
        let (text, _calls, truncated) = self.stream_turn(request).await?;
        Ok(AgentTurn {
            invocation_id: InvocationId::fresh(),
            text,
            truncated,
        })
    }

    /// The typed identity this agent acts under. The host publishes
    /// approval requests under this principal so grants match later turns.
    pub fn principal(&self) -> Principal {
        Principal::Agent(self.agent_id.clone())
    }

    /// Run a full tool-calling turn (Task 12):
    ///
    /// ```text
    /// Model → [tool calls] → registry → policy → execute → model → …
    /// ```
    ///
    /// Pure tools (no capability requirement, e.g. `system.echo`) execute
    /// directly. Privileged tools go through `authorize`: `Allow` executes,
    /// `Deny` feeds an error back to the model, `RequireUserApproval`
    /// stops the turn with [`AgentOutcome::pending_approval`] set — the
    /// frontend approval UI (Task 14) resumes it later.
    pub async fn turn_with_tools(
        &self,
        messages: Vec<ModelMessage>,
        registry: &ToolRegistry,
        policy: &AuthorizationContext,
    ) -> Result<AgentOutcome, AgentError> {
        let authorizer = ContextAuthorizer { context: policy };
        self.turn_with_tools_authorized(messages, registry, &authorizer)
            .await
    }

    /// Run a tool-calling turn against a live authorization boundary. The
    /// host uses this entry point so preflight and grant consumption share
    /// the same approval queue.
    pub async fn turn_with_tools_authorized(
        &self,
        messages: Vec<ModelMessage>,
        registry: &ToolRegistry,
        authorizer: &dyn ToolAuthorizer,
    ) -> Result<AgentOutcome, AgentError> {
        let turn_span = tracing::info_span!("agent.turn", agent = %self.agent_id);
        self.turn_with_tools_inner(messages, registry, authorizer)
            .instrument(turn_span)
            .await
    }

    async fn turn_with_tools_inner(
        &self,
        mut messages: Vec<ModelMessage>,
        registry: &ToolRegistry,
        authorizer: &dyn ToolAuthorizer,
    ) -> Result<AgentOutcome, AgentError> {
        if !messages
            .first()
            .is_some_and(|message| message.role == model_core::ModelRole::System)
        {
            if let Some(prompt) = &self.system_prompt {
                messages.insert(0, ModelMessage::system(prompt.clone()));
            }
        }
        let tool_defs: Vec<ToolDefinition> = registry
            .list()
            .iter()
            .map(ToolDefinition::from_metadata)
            .collect();
        let tool_ids: Vec<String> = tool_defs.iter().map(|tool| tool.name.clone()).collect();
        let invocation_id = InvocationId::fresh();
        let mut executed: Vec<ExecutedTool> = Vec::new();
        let mut tool_steps = Vec::new();
        let mut truncated = false;
        let mut final_text = String::new();
        let mut read_only_follow_up_nudged = false;
        // A deterministic filesystem validation failure must not cause the
        // model to spin by emitting the exact same mutation again. The model
        // can still recover by changing the target, reading the file, or
        // selecting the create tool.
        let mut blocked_retries: HashMap<String, String> = HashMap::new();
        // Computer-use actions are intentionally serialized across model
        // observations. The first action also requires an observation; one
        // assistant response may perform at most one desktop-control action,
        // and the next action requires a fresh observation result so the
        // model cannot accidentally emit a blind coordinate macro.
        let mut desktop_observation_required = true;

        for _ in 0..self.limits.max_iterations {
            let mut request = ModelRequest::new(messages.clone());
            request.max_tokens = Some(1024);
            request.tools = tool_defs.clone();
            request.artifact_store = self.artifact_store.clone();
            debug_assert_eq!(request.tools.len(), tool_defs.len());
            tracing::debug!(
                tool_count = request.tools.len(),
                tool_ids = ?tool_ids,
                "native agent model request includes tool definitions"
            );
            let (text, calls, turn_truncated) = self.stream_turn(request).await?;
            truncated |= turn_truncated;
            final_text = text.clone();
            if calls.is_empty() {
                let last_tool_needs_text_continuation = executed.last().is_some_and(|step| {
                    Self::tool_may_need_follow_up(&step.name)
                        && (!matches!(step.name.as_str(), "filesystem.read")
                            || text.trim().is_empty())
                });
                let should_nudge = !read_only_follow_up_nudged && last_tool_needs_text_continuation;
                if should_nudge {
                    let last_tool = executed
                        .last()
                        .map(|step| step.name.as_str())
                        .unwrap_or("unknown");
                    tracing::debug!(
                        last_tool,
                        "native read-only tool completed but model returned text; requesting one bounded continuation"
                    );
                    messages.push(ModelMessage::assistant(text, Vec::new()));
                    messages.push(ModelMessage::user(
                        "The previous native tool completed successfully. Continue the user's request now. If another native operation is needed, call the appropriate tool; do not claim a mutation succeeded without a successful mutation result.",
                    ));
                    read_only_follow_up_nudged = true;
                    continue;
                }
                if !tool_defs.is_empty() {
                    // This is useful for distinguishing a missing Utsuwa
                    // registry from a provider/model that accepted `tools`
                    // but chose to return ordinary text.
                    tracing::debug!(
                        tool_count = tool_defs.len(),
                        "model returned text after tools were supplied; tool calling may be unsupported"
                    );
                }
                messages.push(ModelMessage::assistant(text.clone(), Vec::new()));
                return Ok(AgentOutcome {
                    invocation_id,
                    text,
                    executed,
                    tool_steps,
                    pending_approval: None,
                    truncated,
                    messages: messages.clone(),
                });
            }
            let response_has_desktop_observation =
                calls.iter().any(|call| is_desktop_observation(&call.name));
            // Preflight every call before invoking any of them. A later
            // approval request therefore cannot arrive after an earlier
            // side effect has already happened.
            let available = self.limits.max_tool_calls.saturating_sub(executed.len());
            let mut prepared = Vec::new();
            let mut results = Vec::new();
            let mut desktop_control_reserved_in_response = false;
            for call in calls.iter().take(available) {
                tracing::debug!(
                    tool_call_id = %call.id,
                    tool_name = %call.name,
                    "model requested native tool"
                );
                let is_desktop_control_call = registry
                    .resolve(&call.name)
                    .ok()
                    .is_some_and(|tool| is_desktop_control(&tool.metadata()));
                if is_desktop_control_call
                    && (desktop_control_reserved_in_response
                        || desktop_observation_required
                        || response_has_desktop_observation)
                {
                    let message = if desktop_observation_required {
                        "desktop control is paused until the model receives a fresh desktop observation; call desktop.observe, desktop.screenshot, desktop.capture_frame, or desktop.accessibility_tree first".to_string()
                    } else if response_has_desktop_observation {
                        "desktop observation and desktop control cannot be issued in the same model response; observe first, then act on the next response".to_string()
                    } else {
                        "only one desktop-control action is allowed per model response; observe the changed state before issuing another action".to_string()
                    };
                    tool_steps.push(ToolStep::failure(
                        call,
                        ToolStepStatus::Failed,
                        message.clone(),
                    ));
                    results.push(ModelMessage::tool_result(ToolResult {
                        tool_call_id: call.id.clone(),
                        content: message.clone(),
                        parts: vec![model_core::ModelContentPart::Text(message)],
                        is_error: true,
                    }));
                    continue;
                }
                if is_desktop_control_call {
                    desktop_control_reserved_in_response = true;
                }
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
                    let key = replay_key(&call.name, &args);
                    if let Some(message) = blocked_retries.get(&key) {
                        tool_steps.push(ToolStep::failure(
                            call,
                            ToolStepStatus::Failed,
                            message.clone(),
                        ));
                        results.push(ModelMessage::tool_result(ToolResult {
                            tool_call_id: call.id.clone(),
                            content: message.clone(),
                            parts: vec![model_core::ModelContentPart::Text(message.clone())],
                            is_error: true,
                        }));
                        continue;
                    }
                }
                if let Some(output) = self.replayed_output(registry, call) {
                    if is_desktop_control_call {
                        desktop_observation_required = true;
                    }
                    executed.push(ExecutedTool {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        output: output.clone(),
                    });
                    tool_steps.push(ToolStep::success(call, &output));
                    results.push(ModelMessage::tool_result(tool_result_from_output(
                        &call.id,
                        &output,
                        &self.limits,
                        false,
                    )));
                    continue;
                }
                match self.prepare_call(registry, authorizer, call) {
                    Ok(prepared_call) => prepared.push(prepared_call),
                    Err(PendingOrFailed::Pending(pending)) => {
                        return Ok(AgentOutcome {
                            invocation_id,
                            text,
                            executed,
                            tool_steps,
                            pending_approval: Some(pending),
                            truncated,
                            messages: messages.clone(),
                        });
                    }
                    Err(PendingOrFailed::Failed { message, status }) => {
                        tool_steps.push(ToolStep::failure(call, status, message.clone()));
                        results.push(ModelMessage::tool_result(ToolResult {
                            tool_call_id: call.id.clone(),
                            content: message.clone(),
                            parts: vec![model_core::ModelContentPart::Text(message.clone())],
                            is_error: true,
                        }));
                    }
                }
            }
            for prepared_call in prepared {
                let call_id = prepared_call.call.id.clone();
                let call_name = prepared_call.call.name.clone();
                let step_call = prepared_call.call.clone();
                let retry_key = replay_key(&call_name, &prepared_call.args);
                let is_desktop_control_call = is_desktop_control(&prepared_call.tool.metadata());
                if is_desktop_control_call {
                    // Mark before invoking: a backend may partially perform
                    // an input operation before returning an error, so the
                    // next model step must still re-observe the desktop.
                    desktop_observation_required = true;
                }
                match self.execute_prepared(authorizer, prepared_call).await {
                    Ok(output) => {
                        executed.push(ExecutedTool {
                            id: call_id.clone(),
                            name: call_name.clone(),
                            output: output.clone(),
                        });
                        tool_steps.push(ToolStep::success(&step_call, &output));
                        results.push(ModelMessage::tool_result(tool_result_from_output(
                            &call_id,
                            &output,
                            &self.limits,
                            false,
                        )));
                        if is_desktop_observation(&call_name) {
                            desktop_observation_required = false;
                        }
                    }
                    Err(PendingOrFailed::Failed { message, status }) => {
                        if should_block_same_tool_retry(&call_name, status, &message) {
                            blocked_retries.insert(
                                retry_key,
                                repeated_tool_call_message(&call_name, &message),
                            );
                        }
                        tool_steps.push(ToolStep::failure(&step_call, status, message.clone()));
                        results.push(ModelMessage::tool_result(ToolResult {
                            tool_call_id: call_id.clone(),
                            content: message.clone(),
                            parts: vec![model_core::ModelContentPart::Text(message.clone())],
                            is_error: true,
                        }));
                    }
                    Err(PendingOrFailed::Pending(_)) => {
                        // `execute_prepared` only commits already-approved
                        // calls; a pending result here would indicate a
                        // broken authorizer implementation.
                        tool_steps.push(ToolStep::failure(
                            &step_call,
                            ToolStepStatus::Denied,
                            "authorization changed during execution".to_string(),
                        ));
                        results.push(ModelMessage::tool_result(ToolResult {
                            tool_call_id: call_id,
                            content: "authorization changed during execution".to_string(),
                            parts: vec![model_core::ModelContentPart::Text(
                                "authorization changed during execution".to_string(),
                            )],
                            is_error: true,
                        }));
                    }
                }
            }
            messages.push(ModelMessage::assistant(text, calls));
            messages.extend(results);
        }
        // The loop limit is a completed, bounded outcome as well. The last
        // assistant tool-call message and its results were already appended
        // above; do not add a synthetic assistant message that the model
        // never produced.
        Ok(AgentOutcome {
            invocation_id,
            text: final_text,
            executed,
            tool_steps,
            pending_approval: None,
            truncated,
            messages,
        })
    }

    fn tool_may_need_follow_up(name: &str) -> bool {
        matches!(
            name,
            "system.time"
                | "system.environment"
                | "filesystem.read"
                | "filesystem.read_range"
                | "filesystem.stat"
                | "filesystem.list"
                | "filesystem.search_text"
                | "filesystem.glob"
                | "desktop.inspect"
                | "desktop.status"
                | "desktop.screenshot"
                | "desktop.list_displays"
                | "desktop.observe"
                | "desktop.capture_frame"
        )
    }

    /// Refactored single-turn streaming shared by `turn` and the tool loop.
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<(String, Vec<ToolCall>, bool), AgentError> {
        let mut stream = self.provider.stream(request).await?;
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut events = 0usize;
        let mut truncated = false;
        while let Some(event) = stream.next().await {
            events += 1;
            if events > self.limits.max_stream_events {
                return Err(AgentError::TooManyEvents(self.limits.max_stream_events));
            }
            match event? {
                ModelStreamEvent::TextDelta(delta) => {
                    if text.len() + delta.len() > self.limits.max_output_bytes {
                        truncated = true;
                        break;
                    }
                    text.push_str(&delta);
                    self.emit(AgentEvent::TextDelta(delta));
                }
                ModelStreamEvent::ToolCall(call) => calls.push(call),
                ModelStreamEvent::Done { .. } => break,
            }
        }
        Ok((text, calls, truncated))
    }

    fn replayed_output(&self, registry: &ToolRegistry, call: &ToolCall) -> Option<ToolOutput> {
        let cache = self.replay_cache.as_ref()?;
        let tool = registry.resolve(&call.name).ok()?;
        let args = serde_json::from_str(&call.arguments).ok()?;
        let metadata = tool.metadata();
        cache.get(&call.name, &args, &metadata)
    }

    fn audit(
        &self,
        capability: Option<capability_core::Capability>,
        resource: Option<capability_core::Resource>,
        outcome: AuditOutcome,
        detail: String,
    ) {
        self.audit_timed(capability, resource, outcome, detail, None, None);
    }

    fn audit_timed(
        &self,
        capability: Option<capability_core::Capability>,
        resource: Option<capability_core::Resource>,
        outcome: AuditOutcome,
        detail: String,
        duration_ms: Option<u64>,
        mutation: Option<tool_core::MutationEvidence>,
    ) {
        if let Some(sink) = &self.audit {
            let mut record =
                AuditRecord::now(self.principal(), capability, resource, outcome, detail);
            if let Some(ms) = duration_ms {
                record = record.with_duration(ms);
            }
            if let Some(evidence) = mutation {
                record = record.with_mutation(evidence);
            }
            sink.record(record);
        }
    }

    /// Resolve and authorize a call without invoking it. All calls in one
    /// model response pass this phase before any prepared call executes.
    fn prepare_call(
        &self,
        registry: &ToolRegistry,
        authorizer: &dyn ToolAuthorizer,
        call: &ToolCall,
    ) -> Result<PreparedCall, PendingOrFailed> {
        let tool = registry.resolve(&call.name).map_err(|e| {
            self.audit(None, None, AuditOutcome::Failed, e.to_string());
            PendingOrFailed::Failed {
                message: e.to_string(),
                status: ToolStepStatus::Failed,
            }
        })?;
        let args: serde_json::Value = serde_json::from_str(&call.arguments).map_err(|e| {
            let message = format!("invalid tool arguments: {e}");
            self.audit(None, None, AuditOutcome::Failed, message.clone());
            PendingOrFailed::Failed {
                message,
                status: ToolStepStatus::Failed,
            }
        })?;
        // A policy Allow mints a ticket scoped to exactly the requested
        // resource (least privilege) and bound to this invocation. Brokers
        // re-validate it; the ticket — never the decision — opens the OS.
        let requirements = tool.required_capabilities(&args);
        let mut ticket_ttls = Vec::with_capacity(requirements.len());
        for requirement in &requirements {
            let principal = self.principal();
            let request = CapabilityRequest {
                principal: principal.clone(),
                capability: requirement.capability.clone(),
                resource: requirement.resource.clone(),
            };
            match authorizer.authorize(&principal, &request) {
                AuthorizationDecision::Allow { ticket_ttl: ttl } => {
                    ticket_ttls.push(ttl);
                }
                AuthorizationDecision::Deny { reason } => {
                    self.audit(
                        Some(request.capability.clone()),
                        Some(request.resource.clone()),
                        AuditOutcome::Denied,
                        reason.clone(),
                    );
                    return Err(PendingOrFailed::Failed {
                        message: format!("denied by policy: {reason}"),
                        status: ToolStepStatus::Denied,
                    });
                }
                AuthorizationDecision::RequireUserApproval { reason } => {
                    self.audit(
                        Some(request.capability.clone()),
                        Some(request.resource.clone()),
                        AuditOutcome::ApprovalRequested,
                        reason.clone(),
                    );
                    return Err(PendingOrFailed::Pending(PendingApproval {
                        tool_call: call.clone(),
                        capability: requirement.capability.clone(),
                        resource: requirement.resource.clone(),
                        reason,
                    }));
                }
            }
        }
        Ok(PreparedCall {
            call: call.clone(),
            tool,
            args,
            requirement: requirements.first().cloned(),
            requirements,
            ticket_ttls,
        })
    }

    /// Consume live authority, mint the invocation ticket, and invoke one
    /// already-preflighted call.
    async fn execute_prepared(
        &self,
        authorizer: &dyn ToolAuthorizer,
        prepared: PreparedCall,
    ) -> Result<ToolOutput, PendingOrFailed> {
        let PreparedCall {
            call,
            tool,
            args,
            requirement,
            requirements,
            ticket_ttls,
        } = prepared;
        let metadata = tool.metadata();
        if let Some(cache) = &self.replay_cache {
            if let Some(output) = cache.get(&call.name, &args, &metadata) {
                self.emit(AgentEvent::ToolStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
                let detail = match (authorizer.authorization_mode(), requirement.as_ref()) {
                    (Some(mode), Some(_)) => {
                        format!("{} replayed prior result; side effect skipped; authorization_mode={mode}", call.name)
                    }
                    _ => format!("{} replayed prior result; side effect skipped", call.name),
                };
                self.audit(None, None, AuditOutcome::Executed, detail);
                self.emit(AgentEvent::ToolFinished {
                    id: call.id,
                    name: call.name,
                    ok: true,
                });
                return Ok(output);
            }
        }
        let invocation_id = InvocationId::fresh();
        let mut tickets = Vec::with_capacity(requirements.len());
        for (requirement, ttl) in requirements.iter().zip(ticket_ttls.iter().copied()) {
            let principal = self.principal();
            let request = CapabilityRequest {
                principal: principal.clone(),
                capability: requirement.capability.clone(),
                resource: requirement.resource.clone(),
            };
            if !authorizer.commit(&principal, &request) {
                self.audit(
                    Some(request.capability),
                    Some(request.resource),
                    AuditOutcome::Denied,
                    "grant was revoked or already consumed before execution".to_string(),
                );
                return Err(PendingOrFailed::Failed {
                    message: "permission is no longer available for this tool call".to_string(),
                    status: ToolStepStatus::Denied,
                });
            }
            tickets.push(CapabilityTicket::mint(
                principal,
                requirement.capability.clone(),
                capability_core::ResourceScope::new(vec![requirement.resource.clone()]),
                invocation_id,
                ttl,
            ));
        }
        let mut ctx = ToolContext {
            principal: self.principal(),
            invocation_id,
            ticket: None,
            tickets: Vec::new(),
        };
        for ticket in tickets {
            // Invocation binding must match the ticket or the broker
            // rejects it; keep both from the same mint.
            ctx = ctx.with_ticket(ticket);
        }
        let (capability, resource) = match &requirement {
            Some(requirement) => (
                Some(requirement.capability.clone()),
                Some(requirement.resource.clone()),
            ),
            None => (None, None),
        };
        // Span carries the tool name only: arguments may embed secrets
        // and are never log fields.
        self.emit(AgentEvent::ToolStarted {
            id: call.id.clone(),
            name: call.name.clone(),
        });
        let span = tracing::info_span!("tool.invoke", tool = %call.name);
        let started = std::time::Instant::now();
        let outcome = tool.invoke(ctx, args.clone()).instrument(span).await;
        if is_desktop_control(&metadata) {
            if let Some(notifier) = &self.desktop_action_notifier {
                notifier().await;
            }
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(output) => {
                if let Some(cache) = &self.replay_cache {
                    cache.insert(&call.name, &args, &metadata, output.clone());
                }
                // Mutation evidence (if the tool attached any) joins the
                // audit record: path + before/after hashes, never contents.
                let mutation = output.mutation.clone();
                let detail = match (authorizer.authorization_mode(), requirement.as_ref()) {
                    (Some(mode), Some(_)) => format!("{} ok; authorization_mode={mode}", call.name),
                    _ => format!("{} ok", call.name),
                };
                self.audit_timed(
                    capability,
                    resource,
                    AuditOutcome::Executed,
                    detail,
                    Some(elapsed_ms),
                    mutation,
                );
                tracing::debug!(
                    tool_name = %call.name,
                    success = true,
                    "native tool execution succeeded"
                );
                self.emit(AgentEvent::ToolFinished {
                    id: call.id,
                    name: call.name,
                    ok: true,
                });
                Ok(output)
            }
            Err(err) => {
                let model_message = err.model_message();
                let (error_kind, retryable, recovery_code) = match &err {
                    tool_core::ToolError::RetryRequired { recovery, .. } => (
                        "retry_required",
                        true,
                        recovery
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(""),
                    ),
                    tool_core::ToolError::Filesystem {
                        code, retryable, ..
                    } => ("filesystem", *retryable, code.as_str()),
                    tool_core::ToolError::InvalidArgs { .. } => ("invalid_args", false, ""),
                    tool_core::ToolError::Denied { .. } => ("denied", false, ""),
                    tool_core::ToolError::Failed { .. } => ("failed", false, ""),
                    _ => ("other", false, ""),
                };
                let detail = match (authorizer.authorization_mode(), requirement.as_ref()) {
                    (Some(mode), Some(_)) => format!("{}; authorization_mode={mode}", err),
                    _ => err.to_string(),
                };
                self.audit_timed(
                    capability,
                    resource,
                    AuditOutcome::Failed,
                    detail,
                    Some(elapsed_ms),
                    None,
                );
                tracing::debug!(
                    tool_name = %call.name,
                    success = false,
                    error_kind,
                    retryable,
                    recovery_code,
                    "native tool execution failed"
                );
                self.emit(AgentEvent::ToolFinished {
                    id: call.id,
                    name: call.name,
                    ok: false,
                });
                Err(PendingOrFailed::Failed {
                    message: model_message,
                    status: tool_error_status(&err),
                })
            }
        }
    }
}

/// A tool call the policy engine stopped for user approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub tool_call: ToolCall,
    pub capability: capability_core::Capability,
    pub resource: capability_core::Resource,
    pub reason: String,
}

struct PreparedCall {
    call: ToolCall,
    tool: Arc<dyn tool_core::Tool>,
    args: serde_json::Value,
    requirement: Option<tool_core::CapabilityRequirement>,
    requirements: Vec<tool_core::CapabilityRequirement>,
    ticket_ttls: Vec<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedTool {
    pub id: String,
    pub name: String,
    pub output: ToolOutput,
}

/// The authoritative record of one model-requested tool step. Unlike
/// [`ExecutedTool`], this also records calls that were denied or failed so a
/// host can show the user what actually happened instead of relying on the
/// model's final prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStepStatus {
    Success,
    Retry,
    Failed,
    Denied,
}

impl ToolStepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Retry => "retry",
            Self::Failed => "failed",
            Self::Denied => "denied",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolStep {
    pub id: String,
    pub name: String,
    pub status: ToolStepStatus,
    pub ok: bool,
    /// Successful tool output, kept as the model-facing JSON value. Failed
    /// and denied calls carry their message in `error` instead.
    pub output: Option<serde_json::Value>,
    /// Typed parts (most importantly artifact references) for host activity
    /// consumers. Bytes are never copied into a tool step.
    pub parts: Vec<ContentPart>,
    pub error: Option<String>,
}

impl ToolStep {
    fn success(call: &ToolCall, output: &ToolOutput) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            status: ToolStepStatus::Success,
            ok: true,
            output: Some(output.content.clone()),
            parts: output.parts.clone(),
            error: None,
        }
    }

    fn failure(call: &ToolCall, status: ToolStepStatus, error: String) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            status,
            ok: false,
            output: None,
            parts: Vec::new(),
            error: Some(error),
        }
    }
}

/// Outcome of a tool-calling turn.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub invocation_id: InvocationId,
    pub text: String,
    pub executed: Vec<ExecutedTool>,
    pub tool_steps: Vec<ToolStep>,
    pub pending_approval: Option<PendingApproval>,
    pub truncated: bool,
    /// Conversation transcript after the turn (user/assistant/tool
    /// messages the loop appended). On `pending_approval` this is the
    /// pre-call transcript — safe to extend with an approval note and
    /// re-run, with no dangling tool call. The host owns resumption.
    pub messages: Vec<ModelMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingOrFailed {
    Pending(PendingApproval),
    Failed {
        message: String,
        status: ToolStepStatus,
    },
}

fn truncate_json(value: &serde_json::Value, max_bytes: usize) -> String {
    truncate_text(&value.to_string(), max_bytes)
}

fn effective_tool_text_limit(limits: &AgentLimits) -> usize {
    limits
        .max_tool_output_bytes
        .min(limits.max_tool_text_output_bytes)
}

fn truncate_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let suffix = "…[truncated]";
    if max_bytes <= suffix.len() {
        let mut end = max_bytes;
        while end > 0 && !suffix.is_char_boundary(end) {
            end -= 1;
        }
        return suffix[..end].to_string();
    }
    // Do not split a UTF-8 code point while enforcing the byte limit, and
    // keep the returned string itself within the requested byte budget.
    let prefix_limit = max_bytes - suffix.len();
    let mut end = prefix_limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], suffix)
}

fn model_content_parts(
    output: &ToolOutput,
    limits: &AgentLimits,
) -> Vec<model_core::ModelContentPart> {
    let text_limit = effective_tool_text_limit(limits);
    let mut result = Vec::new();
    let mut artifacts = 0usize;

    // Desktop media tools keep their small capture metadata in the legacy
    // JSON view and their image in a typed part. Preserve both when crossing
    // into the model representation; otherwise the provider would receive a
    // useful screenshot with no capture/window/dimension context.
    let has_image = output
        .parts
        .iter()
        .any(|part| matches!(part, ContentPart::Image(_)));
    let has_textual_part = output
        .parts
        .iter()
        .any(|part| matches!(part, ContentPart::Text(_) | ContentPart::Json(_)));
    if has_image && !has_textual_part && !output.content.is_null() {
        let metadata = output.content.to_string();
        if metadata.len() <= text_limit {
            result.push(model_core::ModelContentPart::Json(output.content.clone()));
        } else {
            result.push(model_core::ModelContentPart::Text(truncate_text(
                &metadata, text_limit,
            )));
        }
    }

    for part in &output.parts {
        match part {
            ContentPart::Text(text) => result.push(model_core::ModelContentPart::Text(
                truncate_text(text, text_limit),
            )),
            ContentPart::Json(value) => {
                let text = value.to_string();
                if text.len() <= text_limit {
                    result.push(model_core::ModelContentPart::Json(value.clone()));
                } else {
                    result.push(model_core::ModelContentPart::Text(truncate_text(
                        &text, text_limit,
                    )));
                }
            }
            ContentPart::Image(image) => {
                let artifact = &image.artifact;
                if artifacts >= limits.max_tool_artifacts {
                    continue;
                }
                if artifact.size_bytes > limits.max_artifact_bytes
                    || image.width > limits.max_image_width
                    || image.height > limits.max_image_height
                {
                    result.push(model_core::ModelContentPart::Text(format!(
                        "[image artifact omitted: {} exceeds the model media limits]",
                        artifact.id
                    )));
                    continue;
                }
                artifacts += 1;
                result.push(model_core::ModelContentPart::Image {
                    artifact: artifact.clone(),
                    detail: None,
                });
            }
            ContentPart::Audio(_) | ContentPart::Video(_) => {
                if artifacts < limits.max_tool_artifacts {
                    artifacts += 1;
                    result.push(model_core::ModelContentPart::Text(
                        "[non-image media artifact is available but this provider adapter does not accept it yet]"
                            .to_string(),
                    ));
                }
            }
        }
    }
    if result.is_empty() {
        result.push(model_core::ModelContentPart::Text(truncate_json(
            &output.content,
            text_limit,
        )));
    }
    result
}

fn tool_result_from_output(
    call_id: &str,
    output: &ToolOutput,
    limits: &AgentLimits,
    is_error: bool,
) -> ToolResult {
    ToolResult {
        tool_call_id: call_id.to_string(),
        content: truncate_json(&output.content, effective_tool_text_limit(limits)),
        parts: model_content_parts(output, limits),
        is_error,
    }
}

fn tool_error_status(error: &tool_core::ToolError) -> ToolStepStatus {
    match error {
        tool_core::ToolError::RetryRequired { .. } => ToolStepStatus::Retry,
        tool_core::ToolError::Filesystem {
            retryable: true, ..
        } => ToolStepStatus::Retry,
        tool_core::ToolError::Denied { .. } => ToolStepStatus::Denied,
        _ => ToolStepStatus::Failed,
    }
}

fn should_block_same_tool_retry(tool: &str, status: ToolStepStatus, message: &str) -> bool {
    if !tool.starts_with("filesystem.") {
        return false;
    }
    if status == ToolStepStatus::Retry {
        return true;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(message) else {
        return false;
    };
    let code = match value.get("error") {
        Some(serde_json::Value::Object(error)) => {
            error.get("code").and_then(|value| value.as_str())
        }
        Some(serde_json::Value::String(code)) => Some(code.as_str()),
        _ => None,
    };
    matches!(
        code,
        Some(
            "file_not_found"
                | "stale_file_ref"
                | "invalid_file_target"
                | "invalid_directory"
                | "directory_unavailable"
                | "invalid_relative_path"
                | "outside_allowed_directory"
                | "path_traversal"
                | "symlink_escape"
                | "target_is_directory"
                | "directory_not_found"
                | "invalid_file_ref"
                | "ambiguous_target"
                | "repeated_tool_call"
        )
    )
}

fn repeated_tool_call_message(tool: &str, previous_error: &str) -> String {
    serde_json::json!({
        "error": "repeated_tool_call",
        "code": "repeated_tool_call",
        "retryable": false,
        "retry_action": "change_arguments",
        "retry_same_arguments": false,
        "tool": tool,
        "message": "The exact same filesystem call already failed. Do not repeat it; change the target or operation, read/stat the target, or use filesystem.create_user_file when the user asked to create a new file.",
        "previous_error": previous_error,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifact_core::{ArtifactId, ArtifactRef, ImageArtifactRef};
    use model_core::{FinishReason, ToolCall};

    #[test]
    fn media_tool_result_retains_metadata_and_image_reference() {
        let image = ImageArtifactRef::new(
            ArtifactRef::new(ArtifactId::new("capture-1"), "image/png", 4),
            2,
            2,
        );
        let metadata = serde_json::json!({
            "capture_id": "capture-1",
            "width": 2,
            "height": 2,
        });
        let output =
            ToolOutput::multipart(metadata.clone(), vec![ContentPart::Image(image.clone())]);

        assert_eq!(
            model_content_parts(&output, &AgentLimits::default()),
            vec![
                model_core::ModelContentPart::Json(metadata),
                model_core::ModelContentPart::Image {
                    artifact: image.artifact,
                    detail: None,
                },
            ]
        );
    }

    struct ScriptedProvider {
        events: Vec<Result<ModelStreamEvent, ModelError>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for ScriptedProvider {
        async fn stream(
            &self,
            _request: ModelRequest,
        ) -> Result<model_core::ModelStream, ModelError> {
            let events = self.events.clone();
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    fn provider(events: Vec<ModelStreamEvent>) -> Arc<dyn ModelProvider> {
        Arc::new(ScriptedProvider {
            events: events.into_iter().map(Ok).collect(),
        })
    }

    #[tokio::test]
    async fn turn_accumulates_text_and_prepends_system_prompt() {
        struct CaptureProvider;
        #[async_trait::async_trait]
        impl ModelProvider for CaptureProvider {
            async fn stream(
                &self,
                request: ModelRequest,
            ) -> Result<model_core::ModelStream, ModelError> {
                assert_eq!(request.messages[0].content, "sys");
                assert_eq!(request.messages[1].content, "hi");
                let events = vec![
                    Ok(ModelStreamEvent::TextDelta("a".to_string())),
                    Ok(ModelStreamEvent::TextDelta("b".to_string())),
                    Ok(ModelStreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ];
                Ok(Box::pin(futures_util::stream::iter(events)))
            }
        }
        let agent = Agent::new(Arc::new(CaptureProvider)).with_system_prompt("sys");
        let turn = agent.turn(vec![ModelMessage::user("hi")]).await.unwrap();
        assert_eq!(turn.text, "ab");
        assert!(!turn.truncated);
    }

    #[tokio::test]
    async fn turn_ignores_tool_calls_for_now() {
        let agent = Agent::new(provider(vec![
            ModelStreamEvent::TextDelta("x".to_string()),
            ModelStreamEvent::ToolCall(ToolCall {
                id: "1".to_string(),
                name: "system.echo".to_string(),
                arguments: "{}".to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]));
        let turn = agent.turn(vec![ModelMessage::user("hi")]).await.unwrap();
        assert_eq!(turn.text, "x");
    }

    #[tokio::test]
    async fn output_cap_truncates() {
        let agent = Agent::new(provider(vec![ModelStreamEvent::TextDelta(
            "abcdef".to_string(),
        )]))
        .with_limits(AgentLimits {
            max_output_bytes: 3,
            max_stream_events: 100,
            ..AgentLimits::default()
        });
        let turn = agent.turn(vec![ModelMessage::user("hi")]).await.unwrap();
        assert!(turn.truncated);
        assert!(turn.text.len() <= 3);
    }

    #[tokio::test]
    async fn event_flood_is_rejected() {
        let events = (0..10)
            .map(|i| ModelStreamEvent::TextDelta(i.to_string()))
            .collect();
        let agent = Agent::new(provider(events)).with_limits(AgentLimits {
            max_output_bytes: 1024,
            max_stream_events: 5,
            ..AgentLimits::default()
        });
        assert_eq!(
            agent.turn(vec![ModelMessage::user("hi")]).await,
            Err(AgentError::TooManyEvents(5))
        );
    }

    #[tokio::test]
    async fn model_errors_surface_as_agent_errors() {
        let agent = Agent::new(Arc::new(ScriptedProvider {
            events: vec![Err(ModelError::Transport("down".to_string()))],
        }));
        assert!(matches!(
            agent.turn(vec![ModelMessage::user("hi")]).await,
            Err(AgentError::Model(_))
        ));
    }

    /// Scripted multi-turn provider: pops one turn per call, records requests.
    struct QueueProvider {
        turns: std::sync::Mutex<Vec<Vec<ModelStreamEvent>>>,
        seen: std::sync::Mutex<Vec<ModelRequest>>,
    }

    impl QueueProvider {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self {
                turns: std::sync::Mutex::new(turns.into_iter().rev().collect()),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    struct WrongPathRecoveryProvider {
        wrong: std::path::PathBuf,
        correct: std::path::PathBuf,
        calls: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for WrongPathRecoveryProvider {
        async fn stream(
            &self,
            request: ModelRequest,
        ) -> Result<model_core::ModelStream, ModelError> {
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let current = *calls;
                *calls += 1;
                current
            };
            let events = match call {
                0 => vec![
                    ModelStreamEvent::ToolCall(ToolCall {
                        id: "wrong-path".to_string(),
                        name: "filesystem.write".to_string(),
                        arguments: serde_json::json!({
                            "path": self.wrong,
                            "content": "Hello from Utsuwa",
                        })
                        .to_string(),
                    }),
                    ModelStreamEvent::Done {
                        finish_reason: FinishReason::ToolCalls,
                    },
                ],
                1 => {
                    let result = request
                        .messages
                        .iter()
                        .find_map(|message| {
                            message
                                .tool_result
                                .as_ref()
                                .filter(|result| result.tool_call_id == "wrong-path")
                        })
                        .expect("failed write result must reach the model");
                    assert!(result.is_error);
                    assert!(result.content.contains("parent directory does not exist"));
                    vec![
                        ModelStreamEvent::ToolCall(ToolCall {
                            id: "correct-path".to_string(),
                            name: "filesystem.write".to_string(),
                            arguments: serde_json::json!({
                                "path": self.correct,
                                "content": "Hello from Utsuwa",
                            })
                            .to_string(),
                        }),
                        ModelStreamEvent::Done {
                            finish_reason: FinishReason::ToolCalls,
                        },
                    ]
                }
                _ => {
                    let result = request
                        .messages
                        .iter()
                        .find_map(|message| {
                            message
                                .tool_result
                                .as_ref()
                                .filter(|result| result.tool_call_id == "correct-path")
                        })
                        .expect("successful write result must reach the model");
                    assert!(!result.is_error);
                    vec![
                        ModelStreamEvent::TextDelta("created it".to_string()),
                        ModelStreamEvent::Done {
                            finish_reason: FinishReason::Stop,
                        },
                    ]
                }
            };
            Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            )))
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for QueueProvider {
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

    fn echo_call(id: &str) -> ModelStreamEvent {
        ModelStreamEvent::ToolCall(ToolCall {
            id: id.to_string(),
            name: "system.echo".to_string(),
            arguments: r#"{"text":"ping"}"#.to_string(),
        })
    }

    fn text_turn(text: &str) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::TextDelta(text.to_string()),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ]
    }

    fn registry_with_echo() -> tool_core::ToolRegistry {
        let mut registry = tool_core::ToolRegistry::new();
        registry.register(Arc::new(tool_core::EchoTool)).unwrap();
        registry
    }

    struct TimeTool;

    #[async_trait::async_trait]
    impl tool_core::Tool for TimeTool {
        fn metadata(&self) -> tool_core::ToolMetadata {
            tool_core::ToolMetadata {
                id: capability_core::ToolId::new("system.time"),
                description: "current time (test double)".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                effects: vec![tool_core::ToolEffect::ReadOnly],
            }
        }

        async fn invoke(
            &self,
            _ctx: tool_core::ToolContext,
            _args: serde_json::Value,
        ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
            Ok(tool_core::ToolOutput::new(
                serde_json::json!({"local": "2026-09-09T17:00:00-05:00"}),
            ))
        }
    }

    fn registry_with_time_and_echo() -> tool_core::ToolRegistry {
        let mut registry = tool_core::ToolRegistry::new();
        registry.register(Arc::new(TimeTool)).unwrap();
        registry.register(Arc::new(tool_core::EchoTool)).unwrap();
        registry
    }

    fn time_call(id: &str) -> ModelStreamEvent {
        ModelStreamEvent::ToolCall(ToolCall {
            id: id.to_string(),
            name: "system.time".to_string(),
            arguments: "{}".to_string(),
        })
    }

    #[tokio::test]
    async fn read_only_tool_text_gets_one_bounded_follow_up() {
        let provider = Arc::new(QueueProvider::new(vec![
            vec![
                time_call("time-1"),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            text_turn("the current time is 17:00:00"),
            vec![
                echo_call("echo-1"),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            text_turn("done"),
        ]));
        let agent = Agent::new(provider.clone());
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("add the current time")],
                &registry_with_time_and_echo(),
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.text, "done");
        assert_eq!(
            outcome
                .executed
                .iter()
                .map(|step| step.name.as_str())
                .collect::<Vec<_>>(),
            vec!["system.time", "system.echo"]
        );
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert!(seen[2].messages.iter().any(|message| {
            message.role == model_core::ModelRole::User
                && message.content.contains("Continue the user's request now")
        }));
        assert!(seen[1].messages.iter().any(|message| {
            message
                .tool_result
                .as_ref()
                .is_some_and(|result| result.tool_call_id == "time-1" && !result.is_error)
        }));
    }

    #[tokio::test]
    async fn echo_executes_and_result_returns_to_model() {
        let provider = Arc::new(QueueProvider::new(vec![
            vec![
                ModelStreamEvent::TextDelta("calling".to_string()),
                echo_call("c1"),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            text_turn("done"),
        ]));
        let agent = Agent::new(provider.clone());
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("hi")],
                &registry_with_echo(),
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.text, "done");
        assert_eq!(outcome.executed.len(), 1);
        assert_eq!(outcome.executed[0].name, "system.echo");
        assert!(outcome.pending_approval.is_none());
        // Turn 2 saw the assistant tool call + the echo result.
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].tools.iter().any(|t| t.name == "system.echo"));
        let turn2 = &seen[1].messages;
        assert!(turn2.iter().any(|m| !m.tool_calls.is_empty()));
        let result = turn2
            .iter()
            .find_map(|m| m.tool_result.clone())
            .expect("tool result fed back");
        assert_eq!(result.tool_call_id, "c1");
        assert!(result.content.contains("ping"));
        assert!(!result.is_error);
        assert_eq!(
            outcome.messages.last().unwrap().role,
            model_core::ModelRole::Assistant
        );
        assert_eq!(outcome.messages.last().unwrap().content, "done");
    }

    #[tokio::test]
    async fn multiple_tool_calls_keep_ids_and_results_in_transcript_order() {
        let provider = Arc::new(QueueProvider::new(vec![
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "call-a".to_string(),
                    name: "system.echo".to_string(),
                    arguments: r#"{"text":"a"}"#.to_string(),
                }),
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "call-b".to_string(),
                    name: "system.echo".to_string(),
                    arguments: r#"{"text":"b"}"#.to_string(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            text_turn("both results received"),
        ]));
        let outcome = Agent::new(Arc::clone(&provider) as Arc<dyn ModelProvider>)
            .turn_with_tools(
                vec![ModelMessage::user("echo twice")],
                &registry_with_echo(),
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.text, "both results received");
        assert_eq!(outcome.executed.len(), 2);
        assert_eq!(outcome.messages.len(), 5);
        assert_eq!(outcome.messages[1].tool_calls[0].id, "call-a");
        assert_eq!(outcome.messages[1].tool_calls[1].id, "call-b");
        assert_eq!(
            outcome.messages[2]
                .tool_result
                .as_ref()
                .unwrap()
                .tool_call_id,
            "call-a"
        );
        assert_eq!(
            outcome.messages[3]
                .tool_result
                .as_ref()
                .unwrap()
                .tool_call_id,
            "call-b"
        );

        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].tools[0].name, "system.echo");
        assert_eq!(
            seen[1].messages[2]
                .tool_result
                .as_ref()
                .unwrap()
                .tool_call_id,
            "call-a"
        );
        assert_eq!(
            seen[1].messages[3]
                .tool_result
                .as_ref()
                .unwrap()
                .tool_call_id,
            "call-b"
        );
    }

    #[tokio::test]
    async fn desktop_actions_require_observation_before_and_between_steps() {
        struct DesktopAction {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl tool_core::Tool for DesktopAction {
            fn metadata(&self) -> tool_core::ToolMetadata {
                tool_core::ToolMetadata {
                    id: capability_core::ToolId::new("desktop.test_action"),
                    description: "test desktop action".to_string(),
                    input_schema: serde_json::json!({"type":"object"}),
                    effects: vec![tool_core::ToolEffect::DesktopControl],
                }
            }

            async fn invoke(
                &self,
                _ctx: tool_core::ToolContext,
                _args: serde_json::Value,
            ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(tool_core::ToolOutput::json(serde_json::json!({"ok":true})))
            }
        }

        struct DesktopObservation;

        #[async_trait::async_trait]
        impl tool_core::Tool for DesktopObservation {
            fn metadata(&self) -> tool_core::ToolMetadata {
                tool_core::ToolMetadata {
                    id: capability_core::ToolId::new("desktop.observe"),
                    description: "test desktop observation".to_string(),
                    input_schema: serde_json::json!({"type":"object"}),
                    effects: vec![tool_core::ToolEffect::ReadOnly],
                }
            }

            async fn invoke(
                &self,
                _ctx: tool_core::ToolContext,
                _args: serde_json::Value,
            ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
                Ok(tool_core::ToolOutput::json(serde_json::json!({
                    "changed": true
                })))
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(DesktopAction {
                calls: calls.clone(),
            }))
            .unwrap();
        registry.register(Arc::new(DesktopObservation)).unwrap();

        let call = |id: &str| {
            ModelStreamEvent::ToolCall(ToolCall {
                id: id.to_string(),
                name: "desktop.test_action".to_string(),
                arguments: "{}".to_string(),
            })
        };
        let provider = Arc::new(QueueProvider::new(vec![
            vec![
                call("action-1"),
                call("action-2"),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "observation-1".to_string(),
                    name: "desktop.observe".to_string(),
                    arguments: "{}".to_string(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                call("action-3"),
                call("action-4"),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "observation-2".to_string(),
                    name: "desktop.observe".to_string(),
                    arguments: "{}".to_string(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                call("action-5"),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            text_turn("done"),
        ]));

        let outcome = Agent::new(Arc::clone(&provider) as Arc<dyn ModelProvider>)
            .turn_with_tools(
                vec![ModelMessage::user("use the desktop")],
                &registry,
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            outcome
                .executed
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "desktop.observe",
                "desktop.test_action",
                "desktop.observe",
                "desktop.test_action",
            ]
        );
        let seen = provider.seen.lock().unwrap();
        assert!(seen[1].messages.iter().any(|message| {
            message.tool_result.as_ref().is_some_and(|result| {
                (result.tool_call_id == "action-1" || result.tool_call_id == "action-2")
                    && result.is_error
                    && result.content.contains("fresh desktop observation")
            })
        }));
        assert!(seen[3].messages.iter().any(|message| {
            message.tool_result.as_ref().is_some_and(|result| {
                result.tool_call_id == "action-4"
                    && result.is_error
                    && result.content.contains("one desktop-control action")
            })
        }));
    }

    #[tokio::test]
    async fn replay_cache_prevents_a_second_side_effect_for_the_same_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingEffect {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl tool_core::Tool for CountingEffect {
            fn metadata(&self) -> tool_core::ToolMetadata {
                tool_core::ToolMetadata {
                    id: capability_core::ToolId::new("test.effect"),
                    description: "count one external effect".to_string(),
                    input_schema: serde_json::json!({"type":"object"}),
                    effects: vec![tool_core::ToolEffect::FilesystemWrite],
                }
            }

            fn required_capability(
                &self,
                _args: &serde_json::Value,
            ) -> Option<tool_core::CapabilityRequirement> {
                Some(tool_core::CapabilityRequirement {
                    capability: capability_core::Capability::FilesystemWrite,
                    resource: capability_core::Resource::Path("/work/replay.txt".into()),
                })
            }

            async fn invoke(
                &self,
                _ctx: tool_core::ToolContext,
                _args: serde_json::Value,
            ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
                let count = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(tool_core::ToolOutput::new(
                    serde_json::json!({ "count": count }),
                ))
            }
        }

        let call = |id: &str| {
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: id.to_string(),
                    name: "test.effect".to_string(),
                    arguments: r#"{"value":"same"}"#.to_string(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ]
        };
        let provider = Arc::new(QueueProvider::new(vec![
            call("first"),
            text_turn("first done"),
            call("repeated"),
            text_turn("second done"),
        ]));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(CountingEffect {
                calls: Arc::clone(&calls),
            }))
            .unwrap();
        let policy = AuthorizationContext {
            grants: vec![policy_core::GrantedScope::new(
                capability_core::PrincipalKind::Agent,
                capability_core::Capability::FilesystemWrite,
                capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                    "/work".into(),
                )]),
                policy_core::GrantLifetime::Session,
                None,
                None,
            )],
            ..AuthorizationContext::default()
        };
        let agent = Agent::new(provider).with_replay_cache(Arc::new(ToolReplayCache::new()));
        let first = agent
            .turn_with_tools(vec![ModelMessage::user("first")], &registry, &policy)
            .await
            .unwrap();
        let second = agent
            .turn_with_tools(vec![ModelMessage::user("second")], &registry, &policy)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.executed[0].output.content["count"], 1);
        assert_eq!(second.executed[0].output.content["count"], 1);
    }

    /// A tool declaring a privileged requirement stops the turn for approval
    /// instead of executing.
    struct WriteTool;
    #[async_trait::async_trait]
    impl tool_core::Tool for WriteTool {
        fn metadata(&self) -> tool_core::ToolMetadata {
            tool_core::ToolMetadata {
                id: capability_core::ToolId::new("filesystem.write"),
                description: "write (test double)".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                effects: vec![tool_core::ToolEffect::FilesystemWrite],
            }
        }
        fn required_capability(
            &self,
            _args: &serde_json::Value,
        ) -> Option<tool_core::CapabilityRequirement> {
            Some(tool_core::CapabilityRequirement {
                capability: capability_core::Capability::FilesystemWrite,
                resource: capability_core::Resource::Path("/work/f".into()),
            })
        }
        async fn invoke(
            &self,
            _ctx: tool_core::ToolContext,
            _args: serde_json::Value,
        ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
            panic!("must not execute without approval");
        }
    }

    #[tokio::test]
    async fn privileged_call_waits_for_approval_without_executing() {
        let provider = Arc::new(QueueProvider::new(vec![vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "c9".to_string(),
                name: "filesystem.write".to_string(),
                arguments: "{}".to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]]));
        let mut registry = tool_core::ToolRegistry::new();
        registry.register(Arc::new(WriteTool)).unwrap();
        let agent = Agent::new(provider.clone());
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("write it")],
                &registry,
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();
        let pending = outcome.pending_approval.expect("approval required");
        assert_eq!(pending.tool_call.id, "c9");
        assert_eq!(
            pending.capability,
            capability_core::Capability::FilesystemWrite
        );
        assert!(outcome.executed.is_empty());
        // Model was consulted exactly once — no second turn after pending.
        assert_eq!(provider.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn preflight_blocks_all_side_effects_until_every_call_is_authorized() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct SideEffectTool {
            count: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl tool_core::Tool for SideEffectTool {
            fn metadata(&self) -> tool_core::ToolMetadata {
                tool_core::ToolMetadata {
                    id: capability_core::ToolId::new("test.first_side_effect"),
                    description: "first side effect".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    effects: vec![tool_core::ToolEffect::FilesystemWrite],
                }
            }
            fn required_capability(
                &self,
                _args: &serde_json::Value,
            ) -> Option<tool_core::CapabilityRequirement> {
                Some(tool_core::CapabilityRequirement {
                    capability: capability_core::Capability::FilesystemWrite,
                    resource: capability_core::Resource::Path("/work/first".into()),
                })
            }
            async fn invoke(
                &self,
                _ctx: tool_core::ToolContext,
                _args: serde_json::Value,
            ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Ok(tool_core::ToolOutput::new(serde_json::json!({"ok": true})))
            }
        }

        struct NeedsApprovalTool;
        #[async_trait::async_trait]
        impl tool_core::Tool for NeedsApprovalTool {
            fn metadata(&self) -> tool_core::ToolMetadata {
                tool_core::ToolMetadata {
                    id: capability_core::ToolId::new("test.second_side_effect"),
                    description: "second side effect".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    effects: vec![tool_core::ToolEffect::FilesystemWrite],
                }
            }
            fn required_capability(
                &self,
                _args: &serde_json::Value,
            ) -> Option<tool_core::CapabilityRequirement> {
                Some(tool_core::CapabilityRequirement {
                    capability: capability_core::Capability::FilesystemWrite,
                    resource: capability_core::Resource::Path("/work/second".into()),
                })
            }
            async fn invoke(
                &self,
                _ctx: tool_core::ToolContext,
                _args: serde_json::Value,
            ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
                panic!("preflight must stop before the second call")
            }
        }

        let first_count = Arc::new(AtomicUsize::new(0));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(SideEffectTool {
                count: Arc::clone(&first_count),
            }))
            .unwrap();
        registry.register(Arc::new(NeedsApprovalTool)).unwrap();
        let provider = Arc::new(QueueProvider::new(vec![vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "first".to_string(),
                name: "test.first_side_effect".to_string(),
                arguments: "{}".to_string(),
            }),
            ModelStreamEvent::ToolCall(ToolCall {
                id: "second".to_string(),
                name: "test.second_side_effect".to_string(),
                arguments: "{}".to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]]));
        let first_grant = policy_core::GrantedScope::new(
            capability_core::PrincipalKind::Agent,
            capability_core::Capability::FilesystemWrite,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                "/work/first".into(),
            )]),
            policy_core::GrantLifetime::Session,
            None,
            None,
        );
        let agent = Agent::new(provider);
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("do both")],
                &registry,
                &AuthorizationContext {
                    grants: vec![first_grant],
                    ..AuthorizationContext::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome.pending_approval.unwrap().tool_call.id, "second");
        assert_eq!(first_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_tool_feeds_error_back_and_continues() {
        let provider = Arc::new(QueueProvider::new(vec![
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "cx".to_string(),
                    name: "nope.missing".to_string(),
                    arguments: "{}".to_string(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            text_turn("recovered"),
        ]));
        let agent = Agent::new(provider);
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("hi")],
                &registry_with_echo(),
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.text, "recovered");
        assert!(outcome.executed.is_empty());
    }

    #[tokio::test]
    async fn repeated_filesystem_retry_is_blocked_until_arguments_change() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct RetryFilesystemTool {
            invocations: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl tool_core::Tool for RetryFilesystemTool {
            fn metadata(&self) -> tool_core::ToolMetadata {
                tool_core::ToolMetadata {
                    id: capability_core::ToolId::new("filesystem.edit"),
                    description: "test filesystem edit".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    effects: vec![tool_core::ToolEffect::FilesystemWrite],
                }
            }

            async fn invoke(
                &self,
                _ctx: tool_core::ToolContext,
                _args: serde_json::Value,
            ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
                self.invocations.fetch_add(1, Ordering::SeqCst);
                Err(tool_core::ToolError::RetryRequired {
                    tool: "filesystem.edit".to_string(),
                    message: "change the file target".to_string(),
                    recovery: serde_json::json!({
                        "error": "file_not_found",
                        "retry_same_arguments": false,
                        "next_tool": "filesystem.create_user_file"
                    }),
                })
            }
        }

        let invocations = Arc::new(AtomicUsize::new(0));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(RetryFilesystemTool {
                invocations: Arc::clone(&invocations),
            }))
            .unwrap();
        let arguments = serde_json::json!({
            "path": "/home/meme/Escritorio/resumen_hoy.txt",
            "old_text": "old",
            "new_text": "new"
        })
        .to_string();
        let provider = Arc::new(QueueProvider::new(vec![
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "first-edit".to_string(),
                    name: "filesystem.edit".to_string(),
                    arguments: arguments.clone(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "repeated-edit".to_string(),
                    name: "filesystem.edit".to_string(),
                    arguments,
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            text_turn("choose a different operation"),
        ]));

        let outcome = Agent::new(provider)
            .turn_with_tools(
                vec![ModelMessage::user("create the file")],
                &registry,
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.text, "choose a different operation");
        assert_eq!(invocations.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.tool_steps[0].status, ToolStepStatus::Retry);
        assert_eq!(outcome.tool_steps[1].status, ToolStepStatus::Failed);
        assert!(outcome.messages.iter().any(|message| {
            message
                .tool_result
                .as_ref()
                .is_some_and(|result| result.content.contains("repeated_tool_call"))
        }));
    }

    #[tokio::test]
    async fn filesystem_write_missing_parent_error_drives_wrong_path_recovery() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-agent-write-recovery-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let wrong = home.join("Desktop/hello.txt");
        let correct = desktop.join("hello.txt");
        let provider = Arc::new(WrongPathRecoveryProvider {
            wrong: wrong.clone(),
            correct: correct.clone(),
            calls: std::sync::Mutex::new(0),
        });
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(tool_filesystem::WriteTool {
                limits: tool_filesystem::FilesystemLimits::default(),
            }))
            .unwrap();
        let grant = policy_core::GrantedScope::new(
            capability_core::PrincipalKind::Agent,
            capability_core::Capability::FilesystemWrite,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                home.clone(),
            )]),
            policy_core::GrantLifetime::Session,
            None,
            None,
        );
        let outcome = Agent::new(provider)
            .turn_with_tools(
                vec![ModelMessage::user("create it")],
                &registry,
                &AuthorizationContext {
                    grants: vec![grant],
                    ..AuthorizationContext::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome.text, "created it");
        assert_eq!(outcome.executed.len(), 1);
        assert_eq!(outcome.tool_steps.len(), 2);
        assert_eq!(outcome.tool_steps[0].status, ToolStepStatus::Failed);
        assert!(!outcome.tool_steps[0].ok);
        assert!(outcome.tool_steps[0]
            .error
            .as_deref()
            .unwrap()
            .contains("parent directory does not exist"));
        assert_eq!(outcome.tool_steps[1].status, ToolStepStatus::Success);
        assert_eq!(
            outcome.tool_steps[1].output.as_ref().unwrap()["path"],
            correct.to_string_lossy().as_ref()
        );
        assert!(!home.join("Desktop").exists());
        assert_eq!(
            std::fs::read_to_string(&correct).unwrap(),
            "Hello from Utsuwa"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// Task 13 acceptance, end to end: with a session grant on a project
    /// dir the agent reads inside it; reaching outside stops for approval
    /// and the broker independently denies out-of-scope tickets.
    #[tokio::test]
    async fn agent_reads_granted_dir_but_not_outside() {
        let root = std::env::temp_dir().join(format!(
            "utsuwa-agent-fs-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "project notes").unwrap();

        let read_call = |id: &str, path: &str| {
            ModelStreamEvent::ToolCall(ToolCall {
                id: id.to_string(),
                name: "filesystem.read".to_string(),
                arguments: serde_json::json!({"path": path}).to_string(),
            })
        };
        let provider = Arc::new(QueueProvider::new(vec![
            vec![
                read_call("r1", &root.join("notes.txt").to_string_lossy()),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                read_call("r2", "/etc/hostname"),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
        ]));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(tool_filesystem::ReadTool {
                limits: tool_filesystem::FilesystemLimits::default(),
            }))
            .unwrap();
        let policy = AuthorizationContext {
            grants: vec![policy_core::GrantedScope {
                principal_kind: capability_core::PrincipalKind::Agent,
                capability: capability_core::Capability::FilesystemRead,
                scope: capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                    root.clone(),
                )]),
                lifetime: policy_core::GrantLifetime::Session,
                ..policy_core::GrantedScope::new(
                    capability_core::PrincipalKind::Agent,
                    capability_core::Capability::FilesystemRead,
                    capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                        root.clone(),
                    )]),
                    policy_core::GrantLifetime::Session,
                    None,
                    None,
                )
            }],
            ..AuthorizationContext::default()
        };
        let agent = Agent::new(provider);
        let outcome = agent
            .turn_with_tools(vec![ModelMessage::user("inspect")], &registry, &policy)
            .await
            .unwrap();
        // Inside the grant: executed, content visible.
        assert_eq!(outcome.executed.len(), 1);
        assert!(outcome.executed[0].output.content["content"]
            .as_str()
            .unwrap()
            .contains("project notes"));
        // Outside the grant: no grant matched, so the turn stopped for
        // approval instead of executing.
        let pending = outcome.pending_approval.expect("approval required");
        assert_eq!(pending.tool_call.id, "r2");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Task 14, denied and approved reads: an ungranted read stops for
    /// approval; denying keeps it stopped, approving (session grant) lets a
    /// resumed turn execute the read through ticket + broker.
    #[tokio::test]
    async fn denied_and_approved_reads() {
        let root = std::env::temp_dir().join(format!(
            "utsuwa-agent-approve-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "secret notes").unwrap();
        let target = root.join("notes.txt").to_string_lossy().into_owned();

        let read_turn = || {
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "r1".to_string(),
                    name: "filesystem.read".to_string(),
                    arguments: serde_json::json!({"path": target}).to_string(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ]
        };
        // Three scripted read turns (one per run); the loop's follow-up
        // model call after the approved execution falls back to Done.
        let provider = Arc::new(QueueProvider::new(vec![
            read_turn(),
            read_turn(),
            read_turn(),
        ]));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(tool_filesystem::ReadTool {
                limits: tool_filesystem::FilesystemLimits::default(),
            }))
            .unwrap();
        let agent = Agent::new(provider);
        let queue = policy_core::ApprovalQueue::new();

        // No grant yet: the turn stops for approval without touching disk.
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("read it")],
                &registry,
                &queue.context(),
            )
            .await
            .unwrap();
        let pending = outcome.pending_approval.expect("approval required");
        assert!(outcome.executed.is_empty());

        // Publish + deny: still no grant, still stopped.
        let published = queue.submit(
            capability_core::Principal::Agent(capability_core::AgentId::new("a")),
            pending.capability.clone(),
            pending.resource.clone(),
            pending.reason.clone(),
        );
        assert_eq!(queue.decide(&published.id, None), Ok(false));
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("read it")],
                &registry,
                &queue.context(),
            )
            .await
            .unwrap();
        assert!(outcome.pending_approval.is_some());
        assert!(outcome.executed.is_empty());

        // Publish + approve for the session: the resumed turn executes and
        // the broker releases the file content.
        let published = queue.submit(
            capability_core::Principal::Agent(capability_core::AgentId::new("a")),
            pending.capability,
            pending.resource,
            pending.reason,
        );
        assert_eq!(
            queue.decide(&published.id, Some(policy_core::GrantLifetime::Session)),
            Ok(true)
        );
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("read it")],
                &registry,
                &queue.context(),
            )
            .await
            .unwrap();
        assert!(outcome.pending_approval.is_none());
        assert_eq!(outcome.executed.len(), 1);
        assert!(outcome.executed[0].output.content["content"]
            .as_str()
            .unwrap()
            .contains("secret notes"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Task 16 acceptance: an agent file edit flows through approval,
    /// ticket, and broker while the audit trail captures every step —
    /// request, denial, approval, execution.
    #[tokio::test]
    async fn patch_audit_trail_end_to_end() {
        use audit_core::{AuditOutcome, InMemorySink};

        let root = std::env::temp_dir().join(format!(
            "utsuwa-agent-patch-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("notes.txt");
        std::fs::write(&target, "version one").unwrap();
        let target_str = target.to_string_lossy().into_owned();

        let patch_turn = || {
            vec![
                ModelStreamEvent::ToolCall(ToolCall {
                    id: "p1".to_string(),
                    name: "filesystem.patch".to_string(),
                    arguments: serde_json::json!({
                        "path": target_str,
                        "replacements": [{"old": "one", "new": "two"}],
                    })
                    .to_string(),
                }),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ]
        };
        let provider = Arc::new(QueueProvider::new(vec![
            patch_turn(),
            patch_turn(),
            patch_turn(),
        ]));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(tool_filesystem::PatchTool {
                limits: tool_filesystem::FilesystemLimits::default(),
            }))
            .unwrap();
        let sink = Arc::new(InMemorySink::new());
        let queue = policy_core::ApprovalQueue::new().with_sink(sink.clone());
        let agent = Agent::new(provider).with_audit_sink(sink.clone());

        // 1. No grant: stopped for approval, file untouched.
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("patch it")],
                &registry,
                &queue.context(),
            )
            .await
            .unwrap();
        let pending = outcome.pending_approval.expect("approval required");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "version one");

        // 2. Deny: still stopped, still untouched.
        let published = queue.submit(
            capability_core::Principal::Agent(capability_core::AgentId::new("a")),
            pending.capability.clone(),
            pending.resource.clone(),
            pending.reason.clone(),
        );
        assert_eq!(queue.decide(&published.id, None), Ok(false));
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("patch it")],
                &registry,
                &queue.context(),
            )
            .await
            .unwrap();
        assert!(outcome.pending_approval.is_some());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "version one");

        // 3. Approve: resumed turn executes the patch.
        let published = queue.submit(
            capability_core::Principal::Agent(capability_core::AgentId::new("a")),
            pending.capability,
            pending.resource,
            pending.reason,
        );
        assert_eq!(
            queue.decide(&published.id, Some(policy_core::GrantLifetime::Session)),
            Ok(true)
        );
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("patch it")],
                &registry,
                &queue.context(),
            )
            .await
            .unwrap();
        assert!(outcome.pending_approval.is_none());
        assert_eq!(outcome.executed.len(), 1);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "version two");

        // 4. Audit trail captures every step in order: the agent stopping
        // and the queue publishing each log their own request record.
        assert_eq!(
            sink.outcomes(),
            vec![
                AuditOutcome::ApprovalRequested, // run 1 stopped
                AuditOutcome::ApprovalRequested, // submit #1 published
                AuditOutcome::ApprovalDenied,    // decide deny
                AuditOutcome::ApprovalRequested, // run 2 stopped
                AuditOutcome::ApprovalRequested, // submit #2 published
                AuditOutcome::Approved,          // decide approve
                AuditOutcome::Executed,          // run 3 patched
            ]
        );
        // 5. The Executed record witnesses the mutation: path plus
        // before/after hashes, never file contents.
        {
            use sha2::{Digest, Sha256};
            let hash = |s: &[u8]| format!("{:x}", Sha256::digest(s));
            let executed = sink
                .records()
                .into_iter()
                .find(|r| r.outcome == AuditOutcome::Executed)
                .expect("executed record");
            let evidence = executed.mutation.as_ref().expect("mutation evidence");
            assert_eq!(evidence.path, target_str);
            assert_eq!(
                evidence.before_sha256.as_deref(),
                Some(hash(b"version one").as_str())
            );
            assert_eq!(
                evidence.after_sha256.as_deref(),
                Some(hash(b"version two").as_str())
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn iteration_bound_stops_runaway_loops() {
        let provider = Arc::new(QueueProvider::new(vec![vec![
            echo_call("c1"),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]]));
        let agent = Agent::new(provider).with_limits(AgentLimits {
            max_iterations: 3,
            ..AgentLimits::default()
        });
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("hi")],
                &registry_with_echo(),
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();
        // 1 initial + 3 loop turns = 4 model calls max, then stop.
        assert!(outcome.executed.len() <= 3);
        assert!(outcome.pending_approval.is_none());
    }

    #[tokio::test]
    async fn bounded_turn_keeps_the_model_tool_exchange_exactly_once() {
        let provider = Arc::new(QueueProvider::new(vec![vec![
            echo_call("c1"),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]]));
        let agent = Agent::new(provider).with_limits(AgentLimits {
            max_iterations: 1,
            ..AgentLimits::default()
        });
        let outcome = agent
            .turn_with_tools(
                vec![ModelMessage::user("hi")],
                &registry_with_echo(),
                &AuthorizationContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.messages.len(), 3);
        assert_eq!(outcome.messages[1].role, model_core::ModelRole::Assistant);
        assert_eq!(outcome.messages[1].tool_calls.len(), 1);
        assert_eq!(outcome.messages[2].role, model_core::ModelRole::Tool);
    }
}
