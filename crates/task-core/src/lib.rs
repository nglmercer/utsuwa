//! Durable task orchestration core for the native host.
//!
//! Boundary: Rust decides WHAT must happen, WHEN, and whether it succeeded.
//! TypeScript decides HOW the VRM looks while doing it. The renderer produces
//! the receipt; Rust verifies it; only then does the task become complete.

pub mod clock;
pub mod error;
pub mod executor;
pub mod model;
pub mod recovery;
pub mod scheduler;
pub mod store;
pub mod verify;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{TaskCoreError, TaskCoreResult};
pub use executor::{
    backoff_ms, test_runners, ApprovalRunner, Executor, FnRunner, Runners, StepContext,
    StepOutcome, StepRunner, WaitRunner,
};
pub use model::{
    ExecutionReceipt, Ms, NewTask, NewTaskStep, ReceiptStatus, Task, TaskError, TaskEvent,
    TaskStatus, TaskStep, TaskStepStatus, TaskStepType, TaskWait, VerificationSpec,
};
pub use recovery::{expire_waits, recover_stale, DEFAULT_LEASE_MS};
pub use scheduler::{cancel_task, Scheduler, TickReport, DEFAULT_BATCH_LIMIT};
pub use store::{SqliteTaskStore, TaskStore, SCHEMA_VERSION};
pub use verify::verify_task;
