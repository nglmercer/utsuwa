//! task-cli: submit, inspect, and headlessly execute durable tasks.
//!
//! Two execution modes:
//!
//! - App mode (default): the running app owns execution; the CLI only
//!   talks to the same `tasks.db` (`demo-time`, `list`, `get`, `watch`).
//! - Headless verify: `run` drives a local tick loop with an explicit
//!   `--provider/--model` (same approach as model-cli, kilo-auto/free by
//!   default) to check whether a model can complete a task. Close the app
//!   first — two tick loops racing can double-execute steps.
//!
//! Full verify loop for the multilingual time check:
//!
//! ```sh
//! cargo run -p app-host --bin task-cli -- demo-time        # prints task id
//! cargo run -p app-host --bin task-cli -- run <task-id> --yes
//! ```
//!
//! `demo-time` builds steps 0..10 where the model reads `system.time` and
//! says the time in another language, with a 3s wait between sayings.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use task_core::{TaskStatus, TaskStep, TaskStore};
use task_host::demo::{DemoOptions, DEFAULT_GAP_MS, DEFAULT_LANGS, DEFAULT_STEPS};
use tool_sdk::ToolPack as _;

const DEFAULT_WATCH_INTERVAL_MS: u64 = 500;
const DEFAULT_WATCH_TIMEOUT_MS: u64 = 600_000;
const MAX_DEMO_STEPS: usize = 30;

fn usage() -> &'static str {
    "usage: task-cli <command> [options]\n\
     \n\
     commands:\n\
     \x20 demo-time [--steps N] [--gap-ms MS] [--langs a,b,c]\n\
     \x20 \x20 \x20 create the multilingual time check (default: steps 0..10,\n\
     \x20 \x20 \x20 3s gaps, 11-language cycle). Prints the task id.\n\
     \x20 list [--status STATUS] [--limit N]\n\
     \x20 get <task-id>\n\
     \x20 watch <task-id> [--interval-ms MS] [--timeout-ms MS]\n\
     \x20 run <task-id> [--provider ID] [--model ID] [--base-url URL]\n\
     \x20 \x20 \x20 [--api-key KEY] [--yes] [--timeout-secs N] [--interval-ms MS]\n\
     \n\
     demo-time/list/get/watch assume the app is running (it executes tasks\n\
     from the shared tasks.db). run executes headlessly instead: close the\n\
     app first. run defaults to --provider kilo --model kilo-auto/free\n\
     (no API key); other providers need --base-url plus --api-key or\n\
     $UTSUWA_MODEL_API_KEY. --yes auto-approves review requests.\n\
     DB: <state-dir>/utsuwa/tasks.db (same path the host uses)."
}

fn tasks_db_path() -> PathBuf {
    // UTSUWA_TASKS_DB is a test/probe escape hatch; production always uses
    // the same path the host uses so the CLI and app share one authority.
    if let Ok(path) = std::env::var("UTSUWA_TASKS_DB") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    storage_core::default_state_dir("utsuwa").join("tasks.db")
}

/// One-line human summary of a finished step for `watch` output.
fn summarize_step(step: &TaskStep) -> String {
    let key = format!("{}:{}", step.step_type.as_str(), step.status.as_str());
    let detail = step
        .result
        .as_ref()
        .and_then(|result| result.get("text"))
        .and_then(serde_json::Value::as_str)
        .map(|text| {
            let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            single_line.chars().take(120).collect::<String>()
        })
        .or_else(|| step.error.clone())
        .unwrap_or_default();
    if detail.is_empty() {
        key
    } else {
        format!("{key} — {detail}")
    }
}

fn parse_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find_map(|pair| {
        if pair[0] == flag {
            Some(pair[1].clone())
        } else {
            None
        }
    })
}

fn parse_usize_arg(args: &[String], flag: &str, default: usize) -> Result<usize, String> {
    match parse_flag_value(args, flag) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| format!("{flag} expects a positive integer, got '{raw}'")),
    }
}

fn parse_ms_arg(args: &[String], flag: &str, default: i64) -> Result<i64, String> {
    match parse_flag_value(args, flag) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<i64>()
            .map_err(|_| format!("{flag} expects an integer of milliseconds, got '{raw}'")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn cmd_demo_time(args: &[String]) -> Result<(), String> {
    let steps = parse_usize_arg(args, "--steps", DEFAULT_STEPS)?;
    if steps == 0 || steps > MAX_DEMO_STEPS {
        return Err(format!("--steps must be 1..={MAX_DEMO_STEPS}"));
    }
    let gap_ms = parse_ms_arg(args, "--gap-ms", DEFAULT_GAP_MS)?;
    if gap_ms < 0 {
        return Err("--gap-ms must be >= 0".to_string());
    }
    let langs: Vec<String> = match parse_flag_value(args, "--langs") {
        None => DEFAULT_LANGS.iter().map(|lang| lang.to_string()).collect(),
        Some(raw) => {
            let langs: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|lang| !lang.is_empty())
                .map(str::to_string)
                .collect();
            if langs.is_empty() {
                return Err("--langs needs at least one language".to_string());
            }
            langs
        }
    };
    let options = DemoOptions {
        steps,
        gap_ms,
        langs,
    };
    let store = task_core::SqliteTaskStore::open(&tasks_db_path())
        .map_err(|err| format!("cannot open tasks.db: {err}"))?;
    let created = store
        .create(task_host::demo::multilingual_time_check(&options), now_ms())
        .await
        .map_err(|err| format!("cannot create task: {err}"))?;
    println!("task: {} ({})", created.id, created.title);
    println!(
        "steps: {} ({} sayings, {} waits)",
        created.steps.len(),
        options.steps,
        options.steps.saturating_sub(1)
    );
    println!("watch: task-cli watch {}", created.id);
    Ok(())
}

async fn cmd_list(args: &[String]) -> Result<(), String> {
    let status = match parse_flag_value(args, "--status") {
        None => None,
        Some(raw) => {
            Some(TaskStatus::parse(&raw).ok_or_else(|| format!("unknown status '{raw}'"))?)
        }
    };
    let limit = parse_usize_arg(args, "--limit", 20)? as i64;
    let store = task_core::SqliteTaskStore::open(&tasks_db_path())
        .map_err(|err| format!("cannot open tasks.db: {err}"))?;
    let tasks = store
        .list(status, limit)
        .await
        .map_err(|err| format!("cannot list tasks: {err}"))?;
    if tasks.is_empty() {
        println!("no tasks");
        return Ok(());
    }
    for task in tasks {
        println!(
            "{}  {}  {}  step {}/{}  attempts {}",
            task.id,
            task.status.as_str(),
            task.title,
            task.current_step_index.min(task.steps.len()),
            task.steps.len(),
            task.attempts,
        );
    }
    Ok(())
}

async fn cmd_get(args: &[String]) -> Result<(), String> {
    let id = args
        .first()
        .ok_or_else(|| "usage: task-cli get <task-id>".to_string())?;
    let store = task_core::SqliteTaskStore::open(&tasks_db_path())
        .map_err(|err| format!("cannot open tasks.db: {err}"))?;
    let task = store
        .get(id)
        .await
        .map_err(|err| format!("cannot read task: {err}"))?
        .ok_or_else(|| format!("unknown task '{id}'"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&task).unwrap_or_else(|_| "{}".to_string())
    );
    Ok(())
}

/// Format finished steps not yet shown, tracking shown markers in
/// `seen` so each step prints exactly once, in order.
fn render_new_steps(task: &task_core::Task, seen: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, step) in task.steps.iter().enumerate() {
        let marker = format!("{index}:{}", step.id);
        if matches!(
            step.status,
            task_core::TaskStepStatus::Completed
                | task_core::TaskStepStatus::Failed
                | task_core::TaskStepStatus::Skipped
        ) && !seen.contains(marker.as_str())
        {
            lines.push(format!("[{index:>2}] {}", summarize_step(step)));
            seen.push_str(&marker);
            seen.push(';');
        }
    }
    lines
}

async fn cmd_watch(args: &[String]) -> Result<ExitCode, String> {
    let id = args
        .first()
        .ok_or_else(|| "usage: task-cli watch <task-id>".to_string())?
        .clone();
    let interval_ms =
        parse_usize_arg(args, "--interval-ms", DEFAULT_WATCH_INTERVAL_MS as usize)? as u64;
    let timeout_ms =
        parse_usize_arg(args, "--timeout-ms", DEFAULT_WATCH_TIMEOUT_MS as usize)? as u64;
    let store = task_core::SqliteTaskStore::open(&tasks_db_path())
        .map_err(|err| format!("cannot open tasks.db: {err}"))?;
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let mut last_seen = String::new();
    println!("watching {id} (app must be running to execute it)");
    loop {
        if started.elapsed() >= timeout {
            return Err(format!("timed out after {timeout_ms}ms waiting for {id}"));
        }
        let task = store
            .get(&id)
            .await
            .map_err(|err| format!("cannot read task: {err}"))?
            .ok_or_else(|| format!("unknown task '{id}'"))?;
        for line in render_new_steps(&task, &mut last_seen) {
            println!("{line}");
        }
        if task.status.is_terminal() {
            println!("{}: {}", task.id, task.status.as_str());
            if let Some(err) = task.last_error {
                println!("last error: {}", err.message);
            }
            return Ok(if task.status == TaskStatus::Completed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}

const KILO_BASE_URL: &str = "https://api.kilo.ai/api/gateway";
const KILO_FREE_MODEL: &str = "kilo-auto/free";
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 900;

#[derive(Debug, PartialEq, Eq)]
struct RunArgs {
    task_id: String,
    provider: String,
    model: String,
    base_url: String,
    api_key: Option<String>,
    auto_approve: bool,
    timeout: Duration,
    interval: Duration,
}

/// Provider flag resolution mirrors model-cli exactly: kilo/kilo-auto/free
/// by default (no key), explicit --model/--base-url required otherwise.
fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    // Usage is `run <task-id> [options]`: the id comes first, never after
    // flags (flag values also lack the -- prefix, so later positionals
    // are ambiguous by design).
    let task_id = args
        .first()
        .filter(|arg| !arg.starts_with("--"))
        .cloned()
        .ok_or_else(|| "usage: task-cli run <task-id> [options]".to_string())?;
    let flag_value = |flag: &str| -> Option<String> { parse_flag_value(args, flag) };
    let provider = flag_value("--provider").unwrap_or_else(|| "kilo".to_string());
    let model = flag_value("--model").unwrap_or_else(|| {
        if provider == "kilo" {
            KILO_FREE_MODEL.to_string()
        } else {
            String::new()
        }
    });
    if model.is_empty() {
        return Err("--model is required for non-kilo providers".to_string());
    }
    let base_url = flag_value("--base-url").unwrap_or_else(|| {
        if provider == "kilo" {
            KILO_BASE_URL.to_string()
        } else {
            String::new()
        }
    });
    if base_url.is_empty() {
        return Err("--base-url is required for non-kilo providers".to_string());
    }
    let api_key = flag_value("--api-key").or_else(|| std::env::var("UTSUWA_MODEL_API_KEY").ok());
    let auto_approve = args.iter().any(|arg| arg == "--yes");
    let timeout_secs = parse_usize_arg(args, "--timeout-secs", DEFAULT_RUN_TIMEOUT_SECS as usize)?;
    let interval_ms = parse_usize_arg(args, "--interval-ms", DEFAULT_WATCH_INTERVAL_MS as usize)?;
    Ok(RunArgs {
        task_id,
        provider,
        model,
        base_url,
        api_key,
        auto_approve,
        timeout: Duration::from_secs(timeout_secs.max(5) as u64),
        interval: Duration::from_millis(interval_ms.max(50) as u64),
    })
}

fn confirm(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

/// Drive one task to a terminal state: tick, print newly finished steps,
/// resolve reviews through `decide`. Shared by the live `run` command and
/// the offline unit test below (in-memory host, scripted backend).
async fn drive_to_terminal(
    host: &task_host::TaskHost,
    task_id: &str,
    decide: &dyn Fn(Option<&task_host::CapabilityReviewRequest>, &str) -> bool,
    interval: Duration,
    timeout: Duration,
) -> Result<task_core::Task, String> {
    let started = Instant::now();
    let mut seen = String::new();
    loop {
        if started.elapsed() >= timeout {
            return Err(format!(
                "timed out after {}s waiting for {task_id}",
                timeout.as_secs()
            ));
        }
        let report = host
            .tick()
            .await
            .map_err(|err| format!("scheduler tick failed: {err}"))?;
        for error in report.errors {
            eprintln!("tick error: {error}");
        }
        let task = host
            .get(task_id)
            .await
            .map_err(|err| format!("cannot read task: {err}"))?
            .ok_or_else(|| format!("unknown task '{task_id}'"))?;
        for line in render_new_steps(&task, &mut seen) {
            println!("{line}");
        }
        if task.status.is_terminal() {
            return Ok(task);
        }
        if task.status == TaskStatus::NeedsReview {
            let reason = task
                .last_error
                .as_ref()
                .map(|err| err.message.as_str())
                .unwrap_or("review required");
            let request = task_host::CapabilityReviewRequest::parse(reason);
            match &request {
                Some(req) => println!(
                    "\n[permission] {} needs {:?} on {:?}\n  detail: {}",
                    req.tool, req.capability, req.resource, req.detail
                ),
                None => println!("\n[review] {reason}"),
            }
            let approved = decide(request.as_ref(), reason);
            let note = if approved {
                "approved from task-cli run"
            } else {
                "rejected from task-cli run"
            };
            host.review(task_id, approved, Some(note.to_string()))
                .await
                .map_err(|err| format!("cannot resolve review: {err}"))?;
            continue;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Synchronous `run` prelude: parse flags and print capabilities. Runs
/// OUTSIDE the async runtime because the shared capability printer builds
/// its own runtime (nested block_on panics).
fn cmd_run_pre(args: &[String]) -> Result<RunArgs, String> {
    let opts = parse_run_args(args)?;
    // Capabilities first, exactly like model-cli: tool_calls is the crux.
    app_host::runtime::providers::print_resolved_capabilities(
        &opts.provider,
        &opts.model,
        &opts.base_url,
        opts.api_key.as_deref(),
        None,
    );
    println!("---");
    Ok(opts)
}

async fn cmd_run_exec(opts: RunArgs) -> Result<ExitCode, String> {
    // Isolated provider settings (model-cli approach): temp state.db holds
    // only the provider/model/base-url; the real tasks.db still carries
    // the task itself.
    let state_dir = std::env::temp_dir().join(format!("utsuwa-task-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_dir);
    std::fs::create_dir_all(&state_dir)
        .map_err(|err| format!("cannot create temp state dir: {err}"))?;
    let storage = Arc::new(Mutex::new(
        storage_core::Storage::open(&state_dir.join("state.db"))
            .map_err(|err| format!("cannot open temp state.db: {err}"))?,
    ));
    {
        let store = storage
            .lock()
            .map_err(|_| "temp storage lock failed".to_string())?;
        for (key, value) in [
            (app_host::runtime::SETTING_PROVIDER, opts.provider.as_str()),
            (app_host::runtime::SETTING_BASE_URL, opts.base_url.as_str()),
            (app_host::runtime::SETTING_MODEL_NAME, opts.model.as_str()),
        ] {
            store
                .set_setting(key, &serde_json::json!(value))
                .map_err(|err| format!("cannot write temp setting {key}: {err}"))?;
        }
    }
    let secrets: Arc<dyn secret_core::SecretStore> = Arc::new(secret_core::MemoryStore::default());
    if let Some(key) = &opts.api_key {
        secrets
            .set(secret_core::ACCOUNT_MODEL_API_KEY, key)
            .map_err(|err| format!("cannot stage API key: {err}"))?;
    }
    let approvals = Arc::new(Mutex::new(policy_core::ApprovalQueue::new()));
    let providers =
        app_host::runtime::providers::provider_factory_with_secrets(Some(storage), secrets);
    // Same focused registry the host wires for tasks.
    let mut registry = tool_core::ToolRegistry::new();
    let load_ctx = tool_sdk::ToolLoadContext::default();
    let system_pack =
        app_host::tooling::SystemToolPack::new(host_core::HostEnvironment::snapshot());
    for tool in system_pack
        .tools(&load_ctx)
        .into_iter()
        .chain(tool_notification::NotificationToolPack.tools(&load_ctx))
    {
        let _ = registry.register(tool);
    }
    let registry = Arc::new(registry);
    let services = Arc::new(task_host::HostServices::new(Arc::clone(&registry)));
    let emit: task_host::EmitFn = Arc::new(|event| {
        if event.event == task_host::AVATAR_ROUTINE_REQUESTED_EVENT {
            eprintln!(
                "task-cli: avatar routine requested — needs the app renderer; run the app instead"
            );
        }
    });
    let backend = Arc::new(app_host::runtime::task_agent::TaskAgentBackend::new(
        providers,
        registry,
        Arc::clone(&approvals),
        Arc::clone(&emit),
        Arc::clone(&services),
    ));
    let host =
        task_host::TaskHost::open_with_services(&tasks_db_path(), services, emit, Some(backend))
            .map_err(|err| format!("cannot open tasks.db: {err}"))?;
    println!(
        "driving {} with {}:{} (close the app first to avoid dual tick loops)",
        opts.task_id, opts.provider, opts.model
    );
    let decide = |request: Option<&task_host::CapabilityReviewRequest>, reason: &str| {
        if opts.auto_approve {
            return true;
        }
        match request {
            Some(req) => confirm(&format!("approve {} ({:?})?", req.tool, req.capability)),
            None => confirm(&format!("approve review ({reason})?")),
        }
    };
    let task =
        drive_to_terminal(&host, &opts.task_id, &decide, opts.interval, opts.timeout).await?;
    println!("{}: {}", task.id, task.status.as_str());
    if let Some(err) = task.last_error {
        println!("last error: {}", err.message);
    }
    let _ = std::fs::remove_dir_all(&state_dir);
    Ok(if task.status == TaskStatus::Completed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run(argv: Vec<String>) -> ExitCode {
    let command = argv.first().map(String::as_str).unwrap_or("");
    let rest = argv.get(1..).unwrap_or(&[]).to_vec();
    // `run` prelude executes before the async runtime exists (cmd_run_pre
    // builds its own runtime for capability discovery).
    let run_opts = if command == "run" {
        match cmd_run_pre(&rest) {
            Ok(opts) => Some(opts),
            Err(err) => {
                eprintln!("task-cli: {err}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("cannot start async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    runtime
        .block_on(async {
            match command {
                "demo-time" => cmd_demo_time(&rest).await.map(|()| ExitCode::SUCCESS),
                "list" => cmd_list(&rest).await.map(|()| ExitCode::SUCCESS),
                "get" => cmd_get(&rest).await.map(|()| ExitCode::SUCCESS),
                "watch" => cmd_watch(&rest).await,
                "run" => cmd_run_exec(run_opts.expect("run prelude must have parsed")).await,
                "-h" | "--help" | "help" | "" => {
                    println!("{}", usage());
                    Ok(ExitCode::SUCCESS)
                }
                unknown => Err(format!("unknown command '{unknown}'\n\n{}", usage())),
            }
        })
        .unwrap_or_else(|err| {
            eprintln!("task-cli: {err}");
            ExitCode::from(2)
        })
}

fn main() -> ExitCode {
    let mut argv: Vec<String> = std::env::args().collect();
    argv.remove(0);
    run(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use task_core::TaskStepType;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn run_args_default_to_kilo_free() {
        let args = parse_run_args(&argv(&["task-1"])).unwrap();
        assert_eq!(args.task_id, "task-1");
        assert_eq!(args.provider, "kilo");
        assert_eq!(args.model, "kilo-auto/free");
        assert_eq!(args.base_url, KILO_BASE_URL);
        assert!(!args.auto_approve);
    }

    #[test]
    fn run_args_validate_provider_and_id() {
        assert!(parse_run_args(&argv(&[])).is_err());
        assert!(parse_run_args(&argv(&["--yes"])).is_err());
        assert!(parse_run_args(&argv(&["t", "--provider", "ollama"])).is_err());
        let args = parse_run_args(&argv(&[
            "t",
            "--provider",
            "ollama",
            "--model",
            "m",
            "--base-url",
            "http://x/v1",
            "--yes",
            "--timeout-secs",
            "60",
        ]))
        .unwrap();
        assert_eq!(args.model, "m");
        assert!(args.auto_approve);
        assert_eq!(args.timeout, Duration::from_secs(60));
    }

    struct GatedTool;

    #[async_trait::async_trait]
    impl tool_core::Tool for GatedTool {
        fn metadata(&self) -> tool_core::ToolMetadata {
            tool_core::ToolMetadata {
                id: capability_core::ToolId::new("test.gated"),
                description: "gated".to_string(),
                input_schema: serde_json::json!({}),
                effects: vec![tool_core::ToolEffect::ExternalSideEffect],
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
            ctx: tool_core::ToolContext,
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

    fn gated_host() -> task_host::TaskHost {
        let mut registry = tool_core::ToolRegistry::new();
        registry.register(Arc::new(GatedTool)).unwrap();
        let services = Arc::new(task_host::HostServices::new(Arc::new(registry)));
        task_host::TaskHost::open_in_memory_with_services(services, Arc::new(|_| {}), None)
            .expect("in-memory host")
    }

    fn gated_task() -> task_core::NewTask {
        task_core::NewTask {
            title: "gated".to_string(),
            instruction: "g".to_string(),
            scheduled_at: None,
            priority: None,
            max_attempts: Some(3),
            verification: None,
            parent_task_id: None,
            steps: vec![task_core::NewTaskStep {
                step_type: TaskStepType::Tool,
                input: serde_json::json!({ "tool": "test.gated", "args": {} }),
                max_attempts: Some(3),
            }],
        }
    }

    #[tokio::test]
    async fn drive_approves_review_and_completes() {
        let host = gated_host();
        let created = host.create(gated_task()).await.expect("create");
        let task = drive_to_terminal(
            &host,
            &created.id,
            &|_, _| true,
            Duration::from_millis(5),
            Duration::from_secs(30),
        )
        .await
        .expect("drive");
        assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    }

    #[tokio::test]
    async fn drive_rejects_review_and_fails() {
        let host = gated_host();
        let created = host.create(gated_task()).await.expect("create");
        let task = drive_to_terminal(
            &host,
            &created.id,
            &|_, _| false,
            Duration::from_millis(5),
            Duration::from_secs(30),
        )
        .await
        .expect("drive");
        assert_eq!(task.status, TaskStatus::Failed, "{task:?}");
    }

    #[test]
    fn render_new_steps_prints_finished_steps_exactly_once() {
        let finished = TaskStep {
            id: "s0".to_string(),
            step_type: TaskStepType::Agent,
            status: task_core::TaskStepStatus::Completed,
            input: serde_json::json!({}),
            result: Some(serde_json::json!({ "text": "Son las tres." })),
            attempts: 1,
            max_attempts: 3,
            started_at: None,
            finished_at: None,
            error: None,
        };
        let mut running = finished.clone();
        running.id = "s1".to_string();
        running.status = task_core::TaskStepStatus::Running;
        running.result = None;
        let task = task_core::Task {
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
            attempts: 1,
            max_attempts: 3,
            lease_until: None,
            next_attempt_at: None,
            wait_for: None,
            steps: vec![finished, running],
            current_step_index: 1,
            result: None,
            last_error: None,
            verification: None,
            parent_task_id: None,
        };
        let mut seen = String::new();
        let first = render_new_steps(&task, &mut seen);
        assert_eq!(
            first,
            vec!["[ 0] agent:completed — Son las tres.".to_string()]
        );
        // Second poll shows nothing new.
        assert!(render_new_steps(&task, &mut seen).is_empty());
    }

    #[test]
    fn summarize_step_shows_agent_text_or_error() {
        let mut completed = TaskStep {
            id: "s".to_string(),
            step_type: TaskStepType::Agent,
            status: task_core::TaskStepStatus::Completed,
            input: serde_json::json!({}),
            result: Some(serde_json::json!({ "status": "success", "text": "  Son las tres.\n" })),
            attempts: 1,
            max_attempts: 3,
            started_at: None,
            finished_at: None,
            error: None,
        };
        assert_eq!(
            summarize_step(&completed),
            "agent:completed — Son las tres."
        );
        completed.status = task_core::TaskStepStatus::Failed;
        completed.result = None;
        completed.error = Some("boom".to_string());
        assert_eq!(summarize_step(&completed), "agent:failed — boom");
        let wait = TaskStep {
            step_type: TaskStepType::Wait,
            status: task_core::TaskStepStatus::Completed,
            error: None,
            ..completed
        };
        assert_eq!(summarize_step(&wait), "wait:completed");
    }
}
