//! Minimal agent runtime (plan Phase 14, Task 11).
//!
//! Scope: one model turn without tools. The agent owns conversation state
//! and streams text; tool calling, permission flow, and multi-iteration
//! loops arrive in Task 12+. Hard limits apply from the first turn.

use audit_core::{AuditOutcome, AuditRecord, AuditSink};
use capability_core::{AgentId, CapabilityRequest, CapabilityTicket, InvocationId, Principal};
use futures_util::StreamExt;
use model_core::{
    ModelError, ModelMessage, ModelProvider, ModelRequest, ModelStreamEvent, ToolCall,
    ToolDefinition, ToolResult,
};
use policy_core::{AuthorizationContext, AuthorizationDecision};
use std::sync::Arc;
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
    pub max_tool_output_bytes: usize,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: 64 * 1024,
            max_stream_events: 4096,
            max_iterations: 8,
            max_tool_calls: 16,
            max_tool_output_bytes: 16 * 1024,
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

pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    agent_id: AgentId,
    system_prompt: Option<String>,
    limits: AgentLimits,
    audit: Option<Arc<dyn AuditSink>>,
}

impl Agent {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            provider,
            agent_id: AgentId::new(uuid::Uuid::new_v4().to_string()),
            system_prompt: None,
            limits: AgentLimits::default(),
            audit: None,
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

    /// Run one model turn: prepend the system prompt, stream the response,
    /// accumulate text. Tool calls in the response are ignored here — use
    /// [`Agent::turn_with_tools`] to execute them.
    pub async fn turn(&self, mut messages: Vec<ModelMessage>) -> Result<AgentTurn, AgentError> {
        if let Some(prompt) = &self.system_prompt {
            messages.insert(0, ModelMessage::system(prompt.clone()));
        }
        let mut request = ModelRequest::new(messages);
        request.max_tokens = Some(1024);
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
        let turn_span = tracing::info_span!("agent.turn", agent = %self.agent_id);
        self.turn_with_tools_inner(messages, registry, policy)
            .instrument(turn_span)
            .await
    }

    async fn turn_with_tools_inner(
        &self,
        mut messages: Vec<ModelMessage>,
        registry: &ToolRegistry,
        policy: &AuthorizationContext,
    ) -> Result<AgentOutcome, AgentError> {
        if let Some(prompt) = &self.system_prompt {
            messages.insert(0, ModelMessage::system(prompt.clone()));
        }
        let tool_defs: Vec<ToolDefinition> = registry
            .list()
            .iter()
            .map(ToolDefinition::from_metadata)
            .collect();
        let invocation_id = InvocationId::fresh();
        let mut executed = Vec::new();
        let mut truncated = false;
        let mut final_text = String::new();

        for _ in 0..self.limits.max_iterations {
            let mut request = ModelRequest::new(messages.clone());
            request.max_tokens = Some(1024);
            request.tools = tool_defs.clone();
            let (text, calls, turn_truncated) = self.stream_turn(request).await?;
            truncated |= turn_truncated;
            final_text = text.clone();
            if calls.is_empty() {
                return Ok(AgentOutcome {
                    invocation_id,
                    text,
                    executed,
                    pending_approval: None,
                    truncated,
                    messages: messages.clone(),
                });
            }
            let mut results = Vec::new();
            for call in &calls {
                if executed.len() >= self.limits.max_tool_calls {
                    break;
                }
                match self.execute_call(registry, policy, call).await {
                    Ok(output) => {
                        executed.push(ExecutedTool {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            output: output.clone(),
                        });
                        results.push(ModelMessage::tool_result(ToolResult {
                            tool_call_id: call.id.clone(),
                            content: truncate_json(
                                &output.content,
                                self.limits.max_tool_output_bytes,
                            ),
                            is_error: false,
                        }));
                    }
                    Err(PendingOrFailed::Pending(pending)) => {
                        // Transcript WITHOUT the dangling assistant call:
                        // appending it would leave a tool call with no
                        // result, which providers reject. The host resumes
                        // by noting the approval outcome and re-running
                        // the turn, so the model re-issues (or drops) the
                        // call against the updated policy context.
                        return Ok(AgentOutcome {
                            invocation_id,
                            text,
                            executed,
                            pending_approval: Some(pending),
                            truncated,
                            messages: messages.clone(),
                        });
                    }
                    Err(PendingOrFailed::Failed(message)) => {
                        results.push(ModelMessage::tool_result(ToolResult {
                            tool_call_id: call.id.clone(),
                            content: message,
                            is_error: true,
                        }));
                    }
                }
            }
            messages.push(ModelMessage::assistant(text, calls));
            messages.extend(results);
        }
        Ok(AgentOutcome {
            invocation_id,
            text: final_text,
            executed,
            pending_approval: None,
            truncated,
            messages,
        })
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
                }
                ModelStreamEvent::ToolCall(call) => calls.push(call),
                ModelStreamEvent::Done { .. } => break,
            }
        }
        Ok((text, calls, truncated))
    }

    fn audit(
        &self,
        capability: Option<capability_core::Capability>,
        resource: Option<capability_core::Resource>,
        outcome: AuditOutcome,
        detail: String,
    ) {
        self.audit_timed(capability, resource, outcome, detail, None);
    }

    fn audit_timed(
        &self,
        capability: Option<capability_core::Capability>,
        resource: Option<capability_core::Resource>,
        outcome: AuditOutcome,
        detail: String,
        duration_ms: Option<u64>,
    ) {
        if let Some(sink) = &self.audit {
            let mut record = AuditRecord::now(
                self.principal(),
                capability,
                resource,
                outcome,
                detail,
            );
            if let Some(ms) = duration_ms {
                record = record.with_duration(ms);
            }
            sink.record(record);
        }
    }

    /// Resolve, authorize, and execute one tool call.
    async fn execute_call(
        &self,
        registry: &ToolRegistry,
        policy: &AuthorizationContext,
        call: &ToolCall,
    ) -> Result<ToolOutput, PendingOrFailed> {
        let tool = registry.resolve(&call.name).map_err(|e| {
            self.audit(None, None, AuditOutcome::Failed, e.to_string());
            PendingOrFailed::Failed(e.to_string())
        })?;
        let args: serde_json::Value = serde_json::from_str(&call.arguments).map_err(|e| {
            let message = format!("invalid tool arguments: {e}");
            self.audit(None, None, AuditOutcome::Failed, message.clone());
            PendingOrFailed::Failed(message)
        })?;
        // A policy Allow mints a ticket scoped to exactly the requested
        // resource (least privilege) and bound to this invocation. Brokers
        // re-validate it; the ticket — never the decision — opens the OS.
        let requirement = tool.required_capability(&args);
        let mut ticket = None;
        if let Some(requirement) = requirement.clone() {
            let principal = self.principal();
            let request = CapabilityRequest {
                principal: principal.clone(),
                capability: requirement.capability.clone(),
                resource: requirement.resource.clone(),
            };
            match policy_core::authorize(&principal, &request, policy) {
                AuthorizationDecision::Allow { ticket_ttl } => {
                    ticket = Some(CapabilityTicket::mint(
                        principal,
                        requirement.capability,
                        capability_core::ResourceScope::new(vec![requirement.resource]),
                        InvocationId::fresh(),
                        ticket_ttl,
                    ));
                }
                AuthorizationDecision::Deny { reason } => {
                    self.audit(
                        Some(request.capability.clone()),
                        Some(request.resource.clone()),
                        AuditOutcome::Denied,
                        reason.clone(),
                    );
                    return Err(PendingOrFailed::Failed(format!("denied by policy: {reason}")));
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
                        capability: requirement.capability,
                        resource: requirement.resource,
                        reason,
                    }));
                }
            }
        }
        let invocation_id = ticket
            .as_ref()
            .map(|t| t.invocation_id.clone())
            .unwrap_or_else(InvocationId::fresh);
        let mut ctx = ToolContext {
            principal: self.principal(),
            invocation_id,
            ticket: None,
        };
        if let Some(ticket) = ticket {
            // Invocation binding must match the ticket or the broker
            // rejects it; keep both from the same mint.
            ctx.invocation_id = ticket.invocation_id.clone();
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
        let span = tracing::info_span!("tool.invoke", tool = %call.name);
        let started = std::time::Instant::now();
        let outcome = tool.invoke(ctx, args).instrument(span).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(output) => {
                self.audit_timed(
                    capability,
                    resource,
                    AuditOutcome::Executed,
                    format!("{} ok", call.name),
                    Some(elapsed_ms),
                );
                Ok(output)
            }
            Err(err) => {
                self.audit_timed(
                    capability,
                    resource,
                    AuditOutcome::Failed,
                    err.to_string(),
                    Some(elapsed_ms),
                );
                Err(PendingOrFailed::Failed(err.to_string()))
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

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedTool {
    pub id: String,
    pub name: String,
    pub output: ToolOutput,
}

/// Outcome of a tool-calling turn.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub invocation_id: InvocationId,
    pub text: String,
    pub executed: Vec<ExecutedTool>,
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
    Failed(String),
}

fn truncate_json(value: &serde_json::Value, max_bytes: usize) -> String {
    let text = value.to_string();
    if text.len() <= max_bytes {
        text
    } else {
        format!("{}…[truncated]", &text[..max_bytes])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_core::{FinishReason, ToolCall};

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
        let agent = Agent::new(provider(vec![ModelStreamEvent::TextDelta("abcdef".to_string())]))
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
                scope: capability_core::ResourceScope::new(vec![
                    capability_core::Resource::Path(root.clone()),
                ]),
                lifetime: policy_core::GrantLifetime::Session,
            }],
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
        let provider = Arc::new(QueueProvider::new(vec![read_turn(), read_turn(), read_turn()]));
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
}
