//! Durable task data model: agent turns are temporary, tasks are durable.
//! Status transitions are explicit — arbitrary mutation is rejected — so a
//! task can only ever move through its legal lifecycle, including across
//! crashes and restarts.

use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch (UTC).
pub type Ms = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Scheduled,
    Ready,
    Running,
    Waiting,
    NeedsReview,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Scheduled => "scheduled",
            TaskStatus::Running => "running",
            TaskStatus::Ready => "ready",
            TaskStatus::Waiting => "waiting",
            TaskStatus::NeedsReview => "needs_review",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(TaskStatus::Pending),
            "scheduled" => Some(TaskStatus::Scheduled),
            "ready" => Some(TaskStatus::Ready),
            "running" => Some(TaskStatus::Running),
            "waiting" => Some(TaskStatus::Waiting),
            "needs_review" => Some(TaskStatus::NeedsReview),
            "completed" => Some(TaskStatus::Completed),
            "failed" => Some(TaskStatus::Failed),
            "cancelled" => Some(TaskStatus::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    /// Legal lifecycle edges. Anything else is a bug, not a retry.
    ///
    /// Note: `Running -> Ready` exists ONLY for crash/lease-expiry recovery
    /// (`recovery::recover_stale`). It requeues work; it never completes it.
    /// `Waiting -> Failed` and `NeedsReview -> Failed` are administrative
    /// edges used ONLY by the scheduler for wait exhaustion and review
    /// rejection — terminal decisions, never execution outcomes.
    pub fn can_transition_to(&self, next: TaskStatus) -> bool {
        if next == TaskStatus::Cancelled {
            return !self.is_terminal();
        }
        matches!(
            (self, next),
            (TaskStatus::Pending, TaskStatus::Ready)
                | (TaskStatus::Scheduled, TaskStatus::Ready)
                | (TaskStatus::Ready, TaskStatus::Running)
                | (TaskStatus::Running, TaskStatus::Ready)
                | (TaskStatus::Running, TaskStatus::Waiting)
                | (TaskStatus::Running, TaskStatus::NeedsReview)
                | (TaskStatus::Running, TaskStatus::Completed)
                | (TaskStatus::Running, TaskStatus::Failed)
                | (TaskStatus::Waiting, TaskStatus::Ready)
                | (TaskStatus::Waiting, TaskStatus::Failed)
                | (TaskStatus::Failed, TaskStatus::Ready)
                | (TaskStatus::NeedsReview, TaskStatus::Ready)
                | (TaskStatus::NeedsReview, TaskStatus::Failed)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepType {
    AvatarRoutine,
    Notification,
    Wait,
    Agent,
    Tool,
    Approval,
}

impl TaskStepType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStepType::AvatarRoutine => "avatar_routine",
            TaskStepType::Notification => "notification",
            TaskStepType::Wait => "wait",
            TaskStepType::Agent => "agent",
            TaskStepType::Tool => "tool",
            TaskStepType::Approval => "approval",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "avatar_routine" => Some(TaskStepType::AvatarRoutine),
            "notification" => Some(TaskStepType::Notification),
            "wait" => Some(TaskStepType::Wait),
            "agent" => Some(TaskStepType::Agent),
            "tool" => Some(TaskStepType::Tool),
            "approval" => Some(TaskStepType::Approval),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
}

impl TaskStepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStepStatus::Pending => "pending",
            TaskStepStatus::Running => "running",
            TaskStepStatus::Completed => "completed",
            TaskStepStatus::Failed => "failed",
            TaskStepStatus::Skipped => "skipped",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(TaskStepStatus::Pending),
            "running" => Some(TaskStepStatus::Running),
            "completed" => Some(TaskStepStatus::Completed),
            "failed" => Some(TaskStepStatus::Failed),
            "skipped" => Some(TaskStepStatus::Skipped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerificationSpec {
    None,
    ResultPresent,
    AvatarRoutine { expected_steps: Vec<String> },
    ToolReceipt { tool_name: String },
    FileExists { file_ref: String },
    AgentReview,
    HumanReview,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskError {
    pub message: String,
    pub retryable: bool,
    pub timestamp: Ms,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskWait {
    pub event_type: Option<String>,
    pub correlation_id: Option<String>,
    pub timeout_at: Option<Ms>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStep {
    pub id: String,
    pub step_type: TaskStepType,
    pub status: TaskStepStatus,
    pub input: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub started_at: Option<Ms>,
    pub finished_at: Option<Ms>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub instruction: String,
    pub status: TaskStatus,
    pub priority: i32,
    pub created_at: Ms,
    pub updated_at: Ms,
    pub scheduled_at: Option<Ms>,
    pub started_at: Option<Ms>,
    pub finished_at: Option<Ms>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub lease_until: Option<Ms>,
    pub next_attempt_at: Option<Ms>,
    pub wait_for: Option<TaskWait>,
    pub steps: Vec<TaskStep>,
    pub current_step_index: usize,
    pub result: Option<serde_json::Value>,
    pub last_error: Option<TaskError>,
    pub verification: Option<VerificationSpec>,
    pub parent_task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTaskStep {
    pub step_type: TaskStepType,
    pub input: serde_json::Value,
    pub max_attempts: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub title: String,
    pub instruction: String,
    pub scheduled_at: Option<Ms>,
    pub priority: Option<i32>,
    pub max_attempts: Option<i32>,
    pub verification: Option<VerificationSpec>,
    pub parent_task_id: Option<String>,
    pub steps: Vec<NewTaskStep>,
}

pub mod priority {
    pub const BACKGROUND: i32 = 10;
    pub const NORMAL: i32 = 50;
    pub const USER_REQUESTED: i32 = 80;
    pub const URGENT: i32 = 100;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Success,
    Failed,
    UnknownOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    pub execution_id: String,
    pub task_id: String,
    pub step_id: String,
    pub operation: String,
    pub status: ReceiptStatus,
    pub started_at: Ms,
    pub finished_at: Ms,
    pub external_id: Option<String>,
    pub output: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub id: String,
    pub event_type: String,
    pub task_id: Option<String>,
    pub step_id: Option<String>,
    pub correlation_id: Option<String>,
    pub payload: serde_json::Value,
    pub created_at: Ms,
}

pub mod event_type {
    pub const CREATED: &str = "task.created";
    pub const SCHEDULED: &str = "task.scheduled";
    pub const READY: &str = "task.ready";
    pub const STARTED: &str = "task.started";
    pub const STEP_STARTED: &str = "task.step.started";
    pub const STEP_COMPLETED: &str = "task.step.completed";
    pub const STEP_FAILED: &str = "task.step.failed";
    pub const WAITING: &str = "task.waiting";
    pub const REVIEW_REQUIRED: &str = "task.review_required";
    pub const COMPLETED: &str = "task.completed";
    pub const FAILED: &str = "task.failed";
    pub const CANCELLED: &str = "task.cancelled";
    pub const AVATAR_ROUTINE_REQUESTED: &str = "avatar.routine.requested";
    pub const AVATAR_ROUTINE_COMPLETED: &str = "avatar.routine.completed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_follow_the_lifecycle() {
        assert!(TaskStatus::Pending.can_transition_to(TaskStatus::Ready));
        assert!(TaskStatus::Scheduled.can_transition_to(TaskStatus::Ready));
        assert!(TaskStatus::Ready.can_transition_to(TaskStatus::Running));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Waiting));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Completed));
        assert!(TaskStatus::Waiting.can_transition_to(TaskStatus::Ready));
        assert!(TaskStatus::Failed.can_transition_to(TaskStatus::Ready));
        assert!(TaskStatus::NeedsReview.can_transition_to(TaskStatus::Ready));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Cancelled));
        assert!(TaskStatus::Scheduled.can_transition_to(TaskStatus::Cancelled));
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert!(!TaskStatus::Pending.can_transition_to(TaskStatus::Running));
        assert!(!TaskStatus::Ready.can_transition_to(TaskStatus::Completed));
        assert!(!TaskStatus::Completed.can_transition_to(TaskStatus::Ready));
        assert!(!TaskStatus::Completed.can_transition_to(TaskStatus::Cancelled));
        assert!(!TaskStatus::Failed.can_transition_to(TaskStatus::Cancelled));
        assert!(!TaskStatus::Waiting.can_transition_to(TaskStatus::Running));
        // Running -> Ready is the crash-recovery requeue edge, allowed.
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Ready));
        // Administrative failure edges for the scheduler only.
        assert!(TaskStatus::Waiting.can_transition_to(TaskStatus::Failed));
        assert!(TaskStatus::NeedsReview.can_transition_to(TaskStatus::Failed));
        assert!(!TaskStatus::Ready.can_transition_to(TaskStatus::Failed));
    }

    #[test]
    fn status_round_trips_through_storage_strings() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Scheduled,
            TaskStatus::Ready,
            TaskStatus::Running,
            TaskStatus::Waiting,
            TaskStatus::NeedsReview,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(TaskStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(TaskStatus::parse("bogus"), None);
    }
}
