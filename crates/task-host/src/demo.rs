//! Built-in demo tasks for CLI verification. `multilingual_time_check`
//! is the `task-cli demo-time` task: the model says the current time in
//! another language per step (steps 0..N-1) with waits between sayings.
//! Results land in the task record; `task-cli watch` prints them.

use task_core::{NewTask, NewTaskStep, TaskStepType};

pub const DEFAULT_LANGS: &[&str] = &[
    "Spanish",
    "French",
    "German",
    "Italian",
    "Portuguese",
    "Dutch",
    "Japanese",
    "Chinese (Simplified)",
    "Arabic",
    "Hindi",
    "English",
];
pub const DEFAULT_STEPS: usize = 11;
pub const DEFAULT_GAP_MS: i64 = 3_000;

#[derive(Debug, Clone)]
pub struct DemoOptions {
    pub steps: usize,
    pub gap_ms: i64,
    pub langs: Vec<String>,
}

impl Default for DemoOptions {
    fn default() -> Self {
        Self {
            steps: DEFAULT_STEPS,
            gap_ms: DEFAULT_GAP_MS,
            langs: DEFAULT_LANGS.iter().map(|lang| lang.to_string()).collect(),
        }
    }
}

pub fn agent_prompt(index: usize, total: usize, lang: &str) -> String {
    format!(
        "Step {index}/{last}: use the system.time tool to read the current time, then reply \
         with ONLY the current time written in {lang} (one short line, no explanation). \
         Do not call any other tool.",
        last = total.saturating_sub(1),
    )
}

/// Build the verify task: agent sayings in rotating languages separated
/// by waits. Pure constructor.
pub fn multilingual_time_check(options: &DemoOptions) -> NewTask {
    let total = options.steps.max(1);
    let mut steps: Vec<NewTaskStep> = Vec::with_capacity(total * 2);
    for index in 0..total {
        let lang = &options.langs[index % options.langs.len()];
        steps.push(NewTaskStep {
            step_type: TaskStepType::Agent,
            input: serde_json::json!({
                "prompt": agent_prompt(index, total, lang),
                "max_iterations": 4,
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
        title: format!(
            "Multilingual time check (0..{}, {}s gaps)",
            total.saturating_sub(1),
            options.gap_ms as f64 / 1000.0
        ),
        instruction: "Say the current time in another language per step, 3s apart.".to_string(),
        scheduled_at: None,
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

    #[test]
    fn demo_task_alternates_sayings_and_waits() {
        let task = multilingual_time_check(&DemoOptions::default());
        assert_eq!(task.steps.len(), 21);
        for (index, step) in task.steps.iter().enumerate() {
            if index % 2 == 0 {
                assert_eq!(step.step_type, TaskStepType::Agent);
            } else {
                assert_eq!(step.step_type, TaskStepType::Wait);
                assert_eq!(
                    step.input
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_i64),
                    Some(DEFAULT_GAP_MS)
                );
            }
        }
        assert_eq!(task.title, "Multilingual time check (0..10, 3s gaps)");
    }

    #[test]
    fn demo_prompts_number_steps_and_cycle_languages() {
        let options = DemoOptions {
            steps: 3,
            gap_ms: 1_000,
            langs: vec!["Spanish".to_string(), "French".to_string()],
        };
        let task = multilingual_time_check(&options);
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
        assert_eq!(prompts.len(), 3);
        assert!(prompts[0].contains("Step 0/2"), "{}", prompts[0]);
        assert!(prompts[0].contains("Spanish"), "{}", prompts[0]);
        assert!(prompts[1].contains("Step 1/2"), "{}", prompts[1]);
        assert!(prompts[1].contains("French"), "{}", prompts[1]);
        assert!(prompts[2].contains("Step 2/2"), "{}", prompts[2]);
        assert!(prompts[2].contains("Spanish"), "{}", prompts[2]);
        for prompt in &prompts {
            assert!(prompt.contains("system.time"), "{prompt}");
        }
    }

    /// Stub agent backend: answers each saying step with canned text so the
    /// demo shape (agent/wait alternation) executes without a model.
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
    async fn demo_shape_executes_end_to_end() {
        let options = DemoOptions {
            steps: 3,
            gap_ms: 50,
            langs: vec!["Spanish".to_string(), "French".to_string()],
        };
        let registry = Arc::new(tool_core::ToolRegistry::new());
        let services = Arc::new(crate::HostServices::new(registry));
        let host = crate::TaskHost::open_in_memory_with_services(
            services,
            Arc::new(|_| {}),
            Some(Arc::new(StubAgentBackend)),
        )
        .expect("host");
        let created = host
            .create(multilingual_time_check(&options))
            .await
            .expect("create");
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
        let done = terminal.expect("demo task should finish");
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
