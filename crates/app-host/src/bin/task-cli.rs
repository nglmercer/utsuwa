//! task-cli: verification CLI for durable tasks. It submits, inspects,
//! and headlessly executes tasks against the same `tasks.db` the app
//! uses — verification only, never a second product surface.
//!
//! Two execution modes:
//!
//! - App mode (default): the running app owns execution; the CLI only
//!   talks to the same `tasks.db` (`list`, `get`, `watch`, `timings`,
//!   and `--submit-only` submissions).
//! - Headless verify: `run`/`interval` drive a local tick loop with an
//!   explicit `--provider/--model` (same approach as model-cli,
//!   kilo-auto/free by default) to check whether a model can complete
//!   a task. Close the app first — two tick loops racing can
//!   double-execute steps.
//!
//! Full verify loop for an interval task:
//!
//! ```sh
//! cargo run -p app-host --bin task-cli -- interval --instruction "..." --yes
//! ```
//!
//! `interval` builds repetitions of any instruction (naming any existing
//! tool) separated by waits — the same builder the default agent uses
//! through `tasks.create_interval` — then awaits the model through every
//! repetition and prints a timing table decomposing model turns vs
//! waits, so the intervals are verifiable.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use task_core::{TaskStatus, TaskStep, TaskStore};
use task_host::interval::{numbered_prompts, IntervalOptions, DEFAULT_GAP_MS};
use tool_sdk::ToolPack as _;

const DEFAULT_WATCH_INTERVAL_MS: u64 = 500;
const DEFAULT_WATCH_TIMEOUT_MS: u64 = 600_000;
const MAX_INTERVAL_STEPS: usize = 30;

fn usage() -> &'static str {
    "usage: task-cli <command> [options]\n\
     \n\
     commands:\n\
     \x20 interval --instruction TEXT [--times N] [--gap-ms MS] [--title T]\n\
     \x20 \x20 \x20 [--submit-only] [--provider ID] [--model ID] [--base-url URL]\n\
     \x20 \x20 \x20 [--api-key KEY] [--yes] [--timeout-secs N] [--interval-ms MS]\n\
     \x20 \x20 \x20 generic do-X-with-interval task (default: 3 times, 3s gaps):\n\
     \x20 \x20 \x20 the instruction names any existing tool, no new tools.\n\
     \x20 \x20 \x20 Needs --yes to drive (instructions may request side effects).\n\
     \x20 list [--status STATUS] [--limit N]\n\
     \x20 get <task-id>\n\
     \x20 watch <task-id> [--interval-ms MS] [--timeout-ms MS]\n\
     \x20 timings <task-id>\n\
     \x20 \x20 \x20 print the timing table for any task (read-only: works for\n\
     \x20 \x20 \x20 app-driven tasks too, to compare against CLI-driven runs).\n\
     \x20 run <task-id> [--provider ID] [--model ID] [--base-url URL]\n\
     \x20 \x20 \x20 [--api-key KEY] [--yes] [--timeout-secs N] [--interval-ms MS]\n\
     \x20 \x20 \x20 [--timings]\n\
     \n\
     list/get/watch/timings assume the app is running (it executes tasks\n\
     from the shared tasks.db). interval and run execute headlessly\n\
     instead: close the app first. Both default to --provider kilo\n\
     --model kilo-auto/free (no API key); other providers need --base-url\n\
     plus --api-key or $UTSUWA_MODEL_API_KEY. Both need --yes to\n\
     auto-approve reviews. --timings prints the per-step timing table\n\
     after the run.\n\
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

const DEFAULT_INTERVAL_TIMES: usize = 3;

#[derive(Debug, PartialEq, Eq)]
struct IntervalArgs {
    instruction: String,
    times: usize,
    gap_ms: i64,
    title: String,
    submit_only: bool,
    provider: ProviderArgs,
}

fn parse_interval_args(args: &[String]) -> Result<IntervalArgs, String> {
    let instruction = parse_flag_value(args, "--instruction")
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "--instruction TEXT is required".to_string())?;
    let times = parse_usize_arg(args, "--times", DEFAULT_INTERVAL_TIMES)?;
    if times == 0 || times > MAX_INTERVAL_STEPS {
        return Err(format!("--times must be 1..={MAX_INTERVAL_STEPS}"));
    }
    let gap_ms = parse_ms_arg(args, "--gap-ms", DEFAULT_GAP_MS)?;
    if gap_ms < 0 {
        return Err("--gap-ms must be >= 0".to_string());
    }
    let title = parse_flag_value(args, "--title").unwrap_or_else(|| {
        format!(
            "Interval task ({}x, {}s gaps)",
            times,
            gap_ms as f64 / 1000.0
        )
    });
    let submit_only = args.iter().any(|arg| arg == "--submit-only");
    // A free instruction may ask for side effects (notifications,
    // messages), so driving one requires explicit --yes — same rule
    // as `run`.
    let auto_approve = args.iter().any(|arg| arg == "--yes");
    let timeout_default = times.saturating_mul(180).saturating_add(180);
    let provider = parse_provider_args(args, auto_approve, timeout_default)?;
    Ok(IntervalArgs {
        instruction,
        times,
        gap_ms,
        title,
        submit_only,
        provider,
    })
}

/// Synchronous `interval` prelude: same nested-runtime rationale as the
/// `run`/`interval` preludes; skipped for `--submit-only`.
fn cmd_interval_pre(args: &[String]) -> Result<IntervalArgs, String> {
    let opts = parse_interval_args(args)?;
    if !opts.submit_only {
        app_host::runtime::providers::print_resolved_capabilities(
            &opts.provider.provider,
            &opts.provider.model,
            &opts.provider.base_url,
            opts.provider.api_key.as_deref(),
            None,
        );
        println!("---");
    }
    Ok(opts)
}

async fn cmd_interval(opts: IntervalArgs) -> Result<ExitCode, String> {
    let options = IntervalOptions {
        title: opts.title.clone(),
        instruction: opts.instruction.clone(),
        prompts: numbered_prompts(&opts.instruction, opts.times),
        gap_ms: opts.gap_ms,
        max_iterations: 4,
        scheduled_at: None,
    };
    let store = task_core::SqliteTaskStore::open(&tasks_db_path())
        .map_err(|err| format!("cannot open tasks.db: {err}"))?;
    let created = store
        .create(task_host::interval::interval_task(&options), now_ms())
        .await
        .map_err(|err| format!("cannot create task: {err}"))?;
    println!("task: {} ({})", created.id, created.title);
    let intervals = opts.times.saturating_sub(1);
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    println!(
        "steps: {} ({} repetition{}, {} interval{} of {:.1}s)",
        created.steps.len(),
        opts.times,
        plural(opts.times),
        intervals,
        plural(intervals),
        options.gap_ms as f64 / 1000.0
    );
    if opts.submit_only {
        println!("watch: task-cli watch {}", created.id);
        println!("run:   task-cli run {} --yes --timings", created.id);
        return Ok(ExitCode::SUCCESS);
    }
    cmd_run_exec(RunArgs {
        task_id: created.id,
        provider: opts.provider.provider,
        model: opts.provider.model,
        base_url: opts.provider.base_url,
        api_key: opts.provider.api_key,
        auto_approve: opts.provider.auto_approve,
        timeout: opts.provider.timeout,
        interval: opts.provider.interval,
        show_timings: true,
    })
    .await
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

/// A wait is on time when it waited at least its full duration.
/// Expiry is tick-driven, so modest overshoot (poll interval plus tick
/// overhead) is expected; only under-waits and large overshoots fail.
fn wait_on_time(expected_ms: i64, actual_ms: i64) -> bool {
    actual_ms >= expected_ms && actual_ms <= expected_ms.saturating_add(2_500)
}

fn step_duration_ms(step: &TaskStep) -> Option<i64> {
    match (step.started_at, step.finished_at) {
        (Some(started), Some(finished)) => Some(finished.saturating_sub(started).max(0)),
        _ => None,
    }
}

fn secs(ms: i64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// Per-step timing table plus the interval proof: every wait checked
/// against its expected duration, and every gap between agent steps
/// decomposed into model turn + waits. This is what makes "3s apart"
/// verifiable — the gap is the turn AND the wait, never the wait alone.
fn timing_report(task: &task_core::Task) -> String {
    use task_core::TaskStepType;
    let mut out = format!("timings for {} ({}):\n", task.id, task.status.as_str());
    let agent_total = task
        .steps
        .iter()
        .filter(|step| step.step_type == TaskStepType::Agent)
        .count();
    let mut agent_seen = 0usize;
    for (index, step) in task.steps.iter().enumerate() {
        let duration = step_duration_ms(step)
            .map(secs)
            .unwrap_or_else(|| "—".to_string());
        let mut detail = String::new();
        if step.step_type == TaskStepType::Agent {
            agent_seen += 1;
            let text = step
                .result
                .as_ref()
                .and_then(|result| result.get("text"))
                .and_then(serde_json::Value::as_str)
                .map(|text| {
                    text.split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(80)
                        .collect::<String>()
                })
                .or_else(|| step.error.clone())
                .unwrap_or_default();
            detail = format!("agent {agent_seen}/{agent_total}: {text}");
        } else if step.step_type == TaskStepType::Wait {
            if let Some(expected) = step
                .input
                .get("duration_ms")
                .and_then(serde_json::Value::as_i64)
            {
                match step_duration_ms(step) {
                    Some(actual) => {
                        let mark = if wait_on_time(expected, actual) {
                            "✓"
                        } else {
                            "✗"
                        };
                        detail = format!("expected {} {mark}", secs(expected));
                    }
                    None => detail = format!("expected {}", secs(expected)),
                }
            }
        }
        out.push_str(&format!(
            "[{index:>2}] {:<9} {:<9} {duration:>7}  {detail}\n",
            step.step_type.as_str(),
            step.status.as_str(),
        ));
    }
    // Gaps between consecutive agent finishes, decomposed.
    let agent_idx: Vec<usize> = task
        .steps
        .iter()
        .enumerate()
        .filter(|(_, step)| step.step_type == TaskStepType::Agent)
        .map(|(index, _)| index)
        .collect();
    let mut gaps = Vec::new();
    for pair in agent_idx.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        let finishes = (task.steps[prev].finished_at, task.steps[next].finished_at);
        if let (Some(prev_fin), Some(next_fin)) = finishes {
            let gap = next_fin.saturating_sub(prev_fin).max(0);
            let turn = step_duration_ms(&task.steps[next]);
            let waits: i64 = task.steps[prev + 1..next]
                .iter()
                .filter(|step| step.step_type == TaskStepType::Wait)
                .filter_map(step_duration_ms)
                .sum();
            match turn {
                Some(turn) => gaps.push(format!(
                    "{} = turn {} + waits {}",
                    secs(gap),
                    secs(turn),
                    secs(waits)
                )),
                None => gaps.push(secs(gap)),
            }
        }
    }
    if !gaps.is_empty() {
        out.push_str(&format!(
            "gaps between agent steps (finish→finish): {}\n",
            gaps.join("; ")
        ));
    }
    let waits: Vec<&TaskStep> = task
        .steps
        .iter()
        .filter(|step| step.step_type == TaskStepType::Wait)
        .collect();
    if !waits.is_empty() {
        let on_time = waits
            .iter()
            .filter(|step| {
                match (
                    step.input
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_i64),
                    step_duration_ms(step),
                ) {
                    (Some(expected), Some(actual)) => wait_on_time(expected, actual),
                    _ => false,
                }
            })
            .count();
        out.push_str(&format!("waits: {on_time}/{} on time\n", waits.len()));
    }
    out
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

/// Read-only timing table for any task: the comparison tool for
/// app-driven runs (CLI-driven runs print it automatically). Exit 0
/// when completed, 1 for other terminal states, 2 when the task is
/// still running (table prints anyway, with partial durations).
async fn cmd_timings(args: &[String]) -> Result<ExitCode, String> {
    let id = args
        .first()
        .ok_or_else(|| "usage: task-cli timings <task-id>".to_string())?;
    let store = task_core::SqliteTaskStore::open(&tasks_db_path())
        .map_err(|err| format!("cannot open tasks.db: {err}"))?;
    let task = store
        .get(id)
        .await
        .map_err(|err| format!("cannot read task: {err}"))?
        .ok_or_else(|| format!("unknown task '{id}'"))?;
    print!("{}", timing_report(&task));
    if task.status == TaskStatus::Completed {
        Ok(ExitCode::SUCCESS)
    } else if task.status.is_terminal() {
        Ok(ExitCode::FAILURE)
    } else {
        println!("task is {} (not terminal yet)", task.status.as_str());
        Ok(ExitCode::from(2))
    }
}

const KILO_BASE_URL: &str = "https://api.kilo.ai/api/gateway";
const KILO_FREE_MODEL: &str = "kilo-auto/free";
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 900;

#[derive(Debug, PartialEq, Eq)]
struct ProviderArgs {
    provider: String,
    model: String,
    base_url: String,
    api_key: Option<String>,
    auto_approve: bool,
    timeout: Duration,
    interval: Duration,
}

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
    show_timings: bool,
}

/// Provider flag resolution mirrors model-cli exactly: kilo/kilo-auto/free
/// by default (no key), explicit --model/--base-url required otherwise.
/// Shared by `run` and `interval`.
fn parse_provider_args(
    args: &[String],
    auto_approve: bool,
    timeout_default_secs: usize,
) -> Result<ProviderArgs, String> {
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
    let timeout_secs = parse_usize_arg(args, "--timeout-secs", timeout_default_secs)?;
    let interval_ms = parse_usize_arg(args, "--interval-ms", DEFAULT_WATCH_INTERVAL_MS as usize)?;
    Ok(ProviderArgs {
        provider,
        model,
        base_url,
        api_key,
        auto_approve,
        timeout: Duration::from_secs(timeout_secs.max(5) as u64),
        interval: Duration::from_millis(interval_ms.max(50) as u64),
    })
}

fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    // Usage is `run <task-id> [options]`: the id comes first, never after
    // flags (flag values also lack the -- prefix, so later positionals
    // are ambiguous by design).
    let task_id = args
        .first()
        .filter(|arg| !arg.starts_with("--"))
        .cloned()
        .ok_or_else(|| "usage: task-cli run <task-id> [options]".to_string())?;
    let auto_approve = args.iter().any(|arg| arg == "--yes");
    let show_timings = args.iter().any(|arg| arg == "--timings");
    let provider = parse_provider_args(args, auto_approve, DEFAULT_RUN_TIMEOUT_SECS as usize)?;
    Ok(RunArgs {
        task_id,
        provider: provider.provider,
        model: provider.model,
        base_url: provider.base_url,
        api_key: provider.api_key,
        auto_approve: provider.auto_approve,
        timeout: provider.timeout,
        interval: provider.interval,
        show_timings,
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

/// Drive one task to a terminal state: scoped tick, print newly finished
/// steps, resolve reviews through `decide`. Shared by the live `run`
/// command and the offline unit test below (in-memory host, scripted
/// backend). The tick is scoped to this task only: executing strangers
/// would both surprise (`run <id>` completing other tasks) and pollute
/// wait timings with foreign model turns.
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
            .tick_one(task_id)
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
    if let Some(err) = &task.last_error {
        println!("last error: {}", err.message);
    }
    if opts.show_timings {
        print!("{}", timing_report(&task));
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
    // `run`/`interval` preludes execute before the async runtime
    // exists (capability discovery builds its own runtime).
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
    let interval_opts = if command == "interval" {
        match cmd_interval_pre(&rest) {
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
                "interval" => {
                    cmd_interval(interval_opts.expect("interval prelude must have parsed")).await
                }
                "list" => cmd_list(&rest).await.map(|()| ExitCode::SUCCESS),
                "get" => cmd_get(&rest).await.map(|()| ExitCode::SUCCESS),
                "watch" => cmd_watch(&rest).await,
                "timings" => cmd_timings(&rest).await,
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

    #[test]
    fn run_args_timings_flag() {
        let args = parse_run_args(&argv(&["task-1"])).unwrap();
        assert!(!args.show_timings);
        let args = parse_run_args(&argv(&["task-1", "--timings"])).unwrap();
        assert!(args.show_timings);
    }

    #[test]
    fn interval_args_require_instruction_and_default_to_three() {
        assert!(parse_interval_args(&argv(&[])).is_err());
        assert!(parse_interval_args(&argv(&["--instruction", "  "])).is_err());
        let args = parse_interval_args(&argv(&["--instruction", "get the date"])).unwrap();
        assert_eq!(args.instruction, "get the date");
        assert_eq!(args.times, 3);
        assert_eq!(args.gap_ms, DEFAULT_GAP_MS);
        assert_eq!(args.title, "Interval task (3x, 3s gaps)");
        assert!(!args.submit_only);
        // Free instructions may request side effects: no auto-approve
        // without explicit --yes.
        assert!(!args.provider.auto_approve);
        assert_eq!(args.provider.timeout, Duration::from_secs(720));
    }

    #[test]
    fn interval_args_yes_times_and_validation() {
        let args = parse_interval_args(&argv(&[
            "--instruction",
            "notify me",
            "--times",
            "2",
            "--gap-ms",
            "1000",
            "--title",
            "ping",
            "--yes",
        ]))
        .unwrap();
        assert_eq!(args.times, 2);
        assert_eq!(args.gap_ms, 1000);
        assert_eq!(args.title, "ping");
        assert!(args.provider.auto_approve);
        assert!(parse_interval_args(&argv(&["--instruction", "x", "--times", "0"])).is_err());
        assert!(parse_interval_args(&argv(&["--instruction", "x", "--times", "31"])).is_err());
        assert!(parse_interval_args(&argv(&["--instruction", "x", "--gap-ms", "-1"])).is_err());
    }

    fn timed_step(
        id: &str,
        step_type: TaskStepType,
        started: i64,
        finished: i64,
        input: serde_json::Value,
        text: Option<&str>,
    ) -> TaskStep {
        TaskStep {
            id: id.to_string(),
            step_type,
            status: task_core::TaskStepStatus::Completed,
            input,
            result: text.map(|text| serde_json::json!({ "text": text })),
            attempts: 1,
            max_attempts: 3,
            started_at: Some(started),
            finished_at: Some(finished),
            error: None,
        }
    }

    fn timed_task(steps: Vec<TaskStep>) -> task_core::Task {
        task_core::Task {
            id: "t".to_string(),
            title: "t".to_string(),
            instruction: "i".to_string(),
            status: TaskStatus::Completed,
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
            steps,
            current_step_index: 3,
            result: None,
            last_error: None,
            verification: None,
            parent_task_id: None,
        }
    }

    #[test]
    fn timing_report_decomposes_model_turns_and_waits() {
        // Saying 74.8s, wait 3.1s, saying 72.1s: the finish→finish gap
        // is 75.2s of turn plus wait — never the wait alone.
        let task = timed_task(vec![
            timed_step(
                "s0",
                TaskStepType::Agent,
                1_000_000,
                1_074_800,
                serde_json::json!({}),
                Some("Son las tres."),
            ),
            timed_step(
                "s1",
                TaskStepType::Wait,
                1_074_800,
                1_077_900,
                serde_json::json!({ "duration_ms": 3000 }),
                None,
            ),
            timed_step(
                "s2",
                TaskStepType::Agent,
                1_077_900,
                1_150_000,
                serde_json::json!({}),
                Some("Il est trois heures."),
            ),
        ]);
        let report = timing_report(&task);
        assert!(report.contains("agent 1/2: Son las tres."), "{report}");
        assert!(
            report.contains("agent 2/2: Il est trois heures."),
            "{report}"
        );
        assert!(report.contains("expected 3.0s ✓"), "{report}");
        assert!(
            report.contains("75.2s = turn 72.1s + waits 3.1s"),
            "{report}"
        );
        assert!(report.contains("waits: 1/1 on time"), "{report}");
    }

    #[test]
    fn timing_report_flags_short_waits() {
        let task = timed_task(vec![timed_step(
            "s0",
            TaskStepType::Wait,
            1_000_000,
            1_000_500,
            serde_json::json!({ "duration_ms": 3000 }),
            None,
        )]);
        let report = timing_report(&task);
        assert!(report.contains("expected 3.0s ✗"), "{report}");
        assert!(report.contains("waits: 0/1 on time"), "{report}");
    }
}
