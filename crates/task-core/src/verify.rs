//! Verification: Rust checks the receipt; only then does the task become
//! complete. The LLM never self-certifies its own requested movement.

use crate::error::{TaskCoreError, TaskCoreResult};
use crate::model::{Task, VerificationSpec};
use serde_json::Value;

pub fn verify_task(task: &Task) -> TaskCoreResult<()> {
    let spec = task
        .verification
        .as_ref()
        .unwrap_or(&VerificationSpec::None);
    match spec {
        VerificationSpec::None => Ok(()),
        VerificationSpec::ResultPresent => {
            if task.result.as_ref().is_some_and(|v| !v.is_null()) {
                Ok(())
            } else {
                Err(TaskCoreError::Verification("result missing".to_string()))
            }
        }
        VerificationSpec::AvatarRoutine { expected_steps } => {
            verify_avatar_routine(task.result.as_ref(), expected_steps)
        }
        VerificationSpec::ToolReceipt { tool_name } => {
            verify_tool_receipt(task.result.as_ref(), tool_name)
        }
        VerificationSpec::FileExists { file_ref } => {
            verify_file_exists(task.result.as_ref(), file_ref)
        }
        VerificationSpec::AgentReview | VerificationSpec::HumanReview => {
            Err(TaskCoreError::Verification(
                "review-gated verification must be discharged by a review step, not the executor"
                    .to_string(),
            ))
        }
    }
}

fn verify_avatar_routine(result: Option<&Value>, expected_steps: &[String]) -> TaskCoreResult<()> {
    let result = result
        .ok_or_else(|| TaskCoreError::Verification("avatar routine receipt missing".to_string()))?;
    let status = result.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "success" {
        return Err(TaskCoreError::Verification(format!(
            "avatar routine receipt status is '{status}', expected 'success'"
        )));
    }
    let completed = result
        .get("completed_steps")
        .and_then(Value::as_array)
        .map(|steps| {
            steps
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let missing: Vec<&String> = expected_steps
        .iter()
        .filter(|step| !completed.iter().any(|done| done == *step))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(TaskCoreError::Verification(format!(
            "avatar routine missing completed steps: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

fn verify_tool_receipt(result: Option<&Value>, tool_name: &str) -> TaskCoreResult<()> {
    let result =
        result.ok_or_else(|| TaskCoreError::Verification("tool receipt missing".to_string()))?;
    let name = result.get("tool").and_then(Value::as_str).unwrap_or("");
    if name != tool_name {
        return Err(TaskCoreError::Verification(format!(
            "tool receipt is for '{name}', expected '{tool_name}'"
        )));
    }
    match result.get("status").and_then(Value::as_str) {
        Some("success") => Ok(()),
        Some(other) => Err(TaskCoreError::Verification(format!(
            "tool receipt status is '{other}', expected 'success'"
        ))),
        None => Err(TaskCoreError::Verification(
            "tool receipt has no status".to_string(),
        )),
    }
}

fn verify_file_exists(result: Option<&Value>, file_ref: &str) -> TaskCoreResult<()> {
    let result =
        result.ok_or_else(|| TaskCoreError::Verification("file receipt missing".to_string()))?;
    let exists = result
        .get("files")
        .and_then(Value::as_object)
        .and_then(|files| files.get(file_ref))
        .and_then(|entry| entry.get("exists"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if exists {
        Ok(())
    } else {
        Err(TaskCoreError::Verification(format!(
            "file '{file_ref}' not confirmed present"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TaskStatus, TaskWait};

    fn task_with(result: Option<Value>, verification: VerificationSpec) -> Task {
        Task {
            id: "t".to_string(),
            title: "t".to_string(),
            instruction: "i".to_string(),
            status: TaskStatus::Running,
            priority: 50,
            created_at: 0,
            updated_at: 0,
            scheduled_at: None,
            started_at: None,
            finished_at: None,
            attempts: 0,
            max_attempts: 3,
            lease_until: None,
            next_attempt_at: None,
            wait_for: None::<TaskWait>,
            steps: Vec::new(),
            current_step_index: 0,
            result,
            last_error: None,
            verification: Some(verification),
            parent_task_id: None,
        }
    }

    #[test]
    fn avatar_receipt_must_list_expected_steps() {
        let spec = VerificationSpec::AvatarRoutine {
            expected_steps: vec!["nod".to_string(), "wave".to_string()],
        };
        let ok = task_with(
            Some(serde_json::json!({
                "status": "success",
                "completed_steps": ["nod", "wave"],
            })),
            spec.clone(),
        );
        assert!(verify_task(&ok).is_ok());

        let partial = task_with(
            Some(serde_json::json!({
                "status": "success",
                "completed_steps": ["nod"],
            })),
            spec,
        );
        assert!(verify_task(&partial).is_err());
    }

    #[test]
    fn avatar_receipt_rejects_non_success_status() {
        let task = task_with(
            Some(serde_json::json!({ "status": "failed", "completed_steps": [] })),
            VerificationSpec::AvatarRoutine {
                expected_steps: Vec::new(),
            },
        );
        assert!(verify_task(&task).is_err());
    }

    #[test]
    fn tool_receipt_checks_name_and_status() {
        let ok = task_with(
            Some(serde_json::json!({ "tool": "notify", "status": "success" })),
            VerificationSpec::ToolReceipt {
                tool_name: "notify".to_string(),
            },
        );
        assert!(verify_task(&ok).is_ok());
        let wrong = task_with(
            Some(serde_json::json!({ "tool": "other", "status": "success" })),
            VerificationSpec::ToolReceipt {
                tool_name: "notify".to_string(),
            },
        );
        assert!(verify_task(&wrong).is_err());
    }
}
