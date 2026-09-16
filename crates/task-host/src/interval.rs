//! Generic interval/scheduled tasks: one agent step per prompt,
//! separated by waits. The instruction names any existing tool — no
//! per-use-case builders. The default agent creates these through
//! `tasks.create_interval`; `task-cli interval` verifies the same
//! builder headlessly.

use task_core::{NewTask, NewTaskStep, TaskStepType};

pub const DEFAULT_GAP_MS: i64 = 3_000;

/// Generic "do X with an interval" task: one agent step per prompt,
/// separated by waits. The instruction names any existing tool
/// (`system.time`, `notification.send`, …) — no new tools, no per-use
/// builders; the agent prompt is the only thing that varies per case.
#[derive(Debug, Clone)]
pub struct IntervalOptions {
    pub title: String,
    pub instruction: String,
    pub prompts: Vec<String>,
    pub gap_ms: i64,
    pub max_iterations: u64,
    /// First run no earlier than this (epoch ms). `None` runs as soon as
    /// the task manager picks the task up.
    pub scheduled_at: Option<i64>,
}

/// Expand one instruction into per-repetition prompts, each told its
/// own number. Identical prompts make the model guess which repetition
/// it is (observed live: wrong numbers on every turn), so the number
/// comes from the builder, never from the model.
pub fn numbered_prompts(instruction: &str, times: usize) -> Vec<String> {
    (0..times.max(1))
        .map(|index| {
            format!(
                "{instruction}\n(This is repetition {} of {}.)",
                index + 1,
                times.max(1)
            )
        })
        .collect()
}

/// Build the interval task: agent steps (one per prompt) separated by
/// waits. Pure constructor.
pub fn interval_task(options: &IntervalOptions) -> NewTask {
    let total = options.prompts.len().max(1);
    let mut steps: Vec<NewTaskStep> = Vec::with_capacity(total * 2);
    for index in 0..total {
        let prompt = options.prompts.get(index).cloned().unwrap_or_default();
        steps.push(NewTaskStep {
            step_type: TaskStepType::Agent,
            input: serde_json::json!({
                "prompt": prompt,
                "max_iterations": options.max_iterations,
            }),
            max_attempts: Some(3),
        });
        if index + 1 < total {
            steps.push(NewTaskStep {
                step_type: TaskStepType::Wait,
                input: serde_json::json!({ "duration_ms": options.gap_ms }),
                max_attempts: Some(1),
            });
        }
    }
    NewTask {
        title: options.title.clone(),
        instruction: options.instruction.clone(),
        scheduled_at: options.scheduled_at,
        priority: Some(task_core::model::priority::USER_REQUESTED),
        max_attempts: Some(3),
        verification: None,
        parent_task_id: None,
        steps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runners::AgentStepBackend;
    use std::sync::Arc;
    use std::time::Duration;
    use task_core::{StepContext, StepOutcome, Task, TaskStatus, TaskStep};

    fn options() -> IntervalOptions {
        IntervalOptions {
            title: "notify twice".to_string(),
            instruction: "send a notification".to_string(),
            prompts: vec!["first".to_string(), "second".to_string()],
            gap_ms: 3_000,
            max_iterations: 4,
            scheduled_at: None,
        }
    }

    #[test]
    fn interval_task_repeats_any_instruction_with_gaps() {
        let task = interval_task(&options());
        assert_eq!(task.title, "notify twice");
        assert_eq!(task.instruction, "send a notification");
        assert_eq!(task.steps.len(), 3);
        let prompts: Vec<&str> = task
            .steps
            .iter()
            .filter(|step| step.step_type == TaskStepType::Agent)
            .map(|step| {
                step.input
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(prompts, vec!["first", "second"]);
        assert_eq!(task.steps[1].step_type, TaskStepType::Wait);
        assert_eq!(
            task.steps[1]
                .input
                .get("duration_ms")
                .and_then(serde_json::Value::as_i64),
            Some(3_000)
        );
    }

    #[test]
    fn numbered_prompts_tell_each_repetition_its_number() {
        let prompts = numbered_prompts("get the date", 3);
        assert_eq!(prompts.len(), 3);
        for (index, prompt) in prompts.iter().enumerate() {
            assert!(prompt.starts_with("get the date"), "{prompt}");
            assert!(
                prompt.contains(&format!("repetition {} of 3", index + 1)),
                "{prompt}"
            );
        }
    }

    #[test]
    fn interval_task_carries_scheduled_start() {
        let task = interval_task(&IntervalOptions {
            scheduled_at: Some(99_000),
            ..options()
        });
        assert_eq!(task.scheduled_at, Some(99_000));
    }

    /// Stub agent backend: answers each repetition with canned text so the
    /// interval shape (agent/wait alternation) executes without a model.
    struct StubAgentBackend;

    #[async_trait::async_trait]
    impl AgentStepBackend for StubAgentBackend {
        async fn run_agent_step(
            &self,
            _ctx: &StepContext,
            _task: &Task,
            step: &TaskStep,
        ) -> StepOutcome {
            let prompt = step
                .input
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            StepOutcome::Completed(serde_json::json!({
                "status": "success",
                "text": format!("mock answer for: {}", prompt.chars().take(24).collect::<String>()),
            }))
        }
    }

    #[tokio::test]
    async fn interval_shape_executes_end_to_end() {
        let options = IntervalOptions {
            prompts: vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string(),
            ],
            gap_ms: 50,
            ..options()
        };
        let registry = Arc::new(tool_core::ToolRegistry::new());
        let services = Arc::new(crate::HostServices::new(registry));
        let host = crate::TaskHost::open_in_memory_with_services(
            services,
            Arc::new(|_| {}),
            Some(Arc::new(StubAgentBackend)),
        )
        .expect("host");
        let created = host.create(interval_task(&options)).await.expect("create");
        // Drive ticks until terminal: agent steps complete inline, 50ms
        // waits expire between ticks.
        let mut terminal = None;
        for _ in 0..20 {
            host.tick().await.expect("tick");
            let task = host.get(&created.id).await.expect("get").expect("task");
            if task.status.is_terminal() {
                terminal = Some(task);
                break;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let done = terminal.expect("interval task should finish");
        assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
        let sayings: Vec<&TaskStep> = done
            .steps
            .iter()
            .filter(|step| step.step_type == TaskStepType::Agent)
            .collect();
        assert_eq!(sayings.len(), 3);
        for saying in sayings {
            assert_eq!(
                saying.status,
                task_core::TaskStepStatus::Completed,
                "{saying:?}"
            );
            assert!(
                saying
                    .result
                    .as_ref()
                    .and_then(|result| result.get("text"))
                    .is_some(),
                "{saying:?}"
            );
        }
    }
}
