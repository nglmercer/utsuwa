//! Process tool integration tests: real children, real pipes.

use capability_core::{
    AgentId, Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tool_core::{Tool, ToolContext};
use tool_process::{
    sanitized_inherited_env, KillTool, ProcessLimits, ProcessManager, SpawnSpec, SpawnTool,
    StatusTool,
};

fn manager() -> std::sync::Arc<ProcessManager> {
    ProcessManager::new(ProcessLimits::default())
}

/// Context carrying a fresh ticket for `ProcessSpawn` on `executable`.
fn ticketed_ctx(executable: &PathBuf) -> ToolContext {
    let principal = Principal::Agent(AgentId::new("test-agent"));
    let invocation = InvocationId::fresh();
    let ticket = CapabilityTicket::mint(
        principal.clone(),
        Capability::ProcessSpawn,
        ResourceScope::new(vec![Resource::Executable(executable.clone())]),
        invocation.clone(),
        Duration::from_secs(120),
    );
    let mut ctx = ToolContext::new(principal).with_ticket(ticket);
    ctx.invocation_id = invocation;
    ctx
}

fn spawn_args(executable: &str, args: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "executable": executable,
        "args": args,
        "timeout_ms": 15_000,
    })
}

async fn spawn_ok(
    spawn: &SpawnTool,
    ctx: ToolContext,
    args: serde_json::Value,
) -> (String, String) {
    let out = spawn.invoke(ctx, args).await.unwrap();
    let handle = out.content["handle"].as_str().unwrap().to_string();
    (handle, out.content["pid"].as_u64().unwrap().to_string())
}

/// Poll `process.status` until `state` reaches `want` (or the deadline).
async fn wait_for_state(
    status: &StatusTool,
    handle: &str,
    want: &str,
    deadline: Duration,
) -> serde_json::Value {
    let ctx = ToolContext::new(Principal::Agent(AgentId::new("poller")));
    let start = Instant::now();
    loop {
        let out = status
            .invoke(ctx.clone(), serde_json::json!({ "handle": handle }))
            .await
            .unwrap();
        if out.content["state"] == want {
            return out.content.clone();
        }
        assert!(
            start.elapsed() < deadline,
            "timed out waiting for state '{want}'; last: {}",
            out.content
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn echo_runs_and_reports_output() {
    let manager = manager();
    let echo = tool_process::resolve_executable("echo").unwrap();
    let spawn = SpawnTool {
        manager: manager.clone(),
        limits: ProcessLimits::default(),
    };
    let status = StatusTool { manager };
    let (handle, _pid) = spawn_ok(
        &spawn,
        ticketed_ctx(&echo),
        spawn_args("echo", &["hello", "world"]),
    )
    .await;
    let content = wait_for_state(&status, &handle, "exited", Duration::from_secs(10)).await;
    assert_eq!(content["exit_code"], 0);
    assert_eq!(content["stdout"], "hello world\n");
    assert_eq!(content["truncated"], false);
}

#[tokio::test]
async fn shell_metacharacters_are_inert_data() {
    let manager = manager();
    // A program name with shell syntax is not on PATH — and must never
    // be interpreted by a shell.
    let spawn = SpawnTool {
        manager: manager.clone(),
        limits: ProcessLimits::default(),
    };
    let echo = tool_process::resolve_executable("echo").unwrap();
    let err = spawn
        .invoke(
            ticketed_ctx(&echo),
            spawn_args("echo; touch /tmp/pwned", &[]),
        )
        .await
        .unwrap_err();
    // Rejected at resolution — never handed to a shell. (The string
    // contains a slash, so it fails as a path rather than a PATH lookup.)
    let message = err.to_string();
    assert!(
        message.contains("not found on PATH") || message.contains("does not exist"),
        "{message}"
    );
    assert!(!std::path::Path::new("/tmp/pwned").exists());

    // Arguments travel as argv: no expansion, no substitution.
    let status = StatusTool { manager };
    let (handle, _) = spawn_ok(
        &spawn,
        ticketed_ctx(&echo),
        spawn_args("echo", &["$(whoami)", "`id`", "a;b"]),
    )
    .await;
    let content = wait_for_state(&status, &handle, "exited", Duration::from_secs(10)).await;
    assert_eq!(content["stdout"], "$(whoami) `id` a;b\n");
}

#[tokio::test]
async fn secrets_do_not_reach_children_implicitly() {
    std::env::set_var("UTSUWA_TEST_SECRET_TOKEN", "super-secret-value");
    std::env::set_var("UTSUWA_TEST_PLAIN_MARKER", "plain-value");
    let names: Vec<String> = sanitized_inherited_env()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        !names.contains(&"UTSUWA_TEST_SECRET_TOKEN".to_string()),
        "secret-looking env var leaked"
    );
    assert!(names.contains(&"UTSUWA_TEST_PLAIN_MARKER".to_string()));
    std::env::remove_var("UTSUWA_TEST_SECRET_TOKEN");
    std::env::remove_var("UTSUWA_TEST_PLAIN_MARKER");

    // End to end: the child environment really lacks the secret, while an
    // explicit delta is delivered.
    std::env::set_var("UTSUWA_TEST_SECRET_TOKEN", "super-secret-value");
    let manager = manager();
    let env_bin = tool_process::resolve_executable("env").unwrap();
    let spawn = SpawnTool {
        manager: manager.clone(),
        limits: ProcessLimits::default(),
    };
    let status = StatusTool { manager };
    let out = spawn
        .invoke(
            ticketed_ctx(&env_bin),
            serde_json::json!({
                "executable": "env",
                "args": [],
                "env": { "UTSUWA_TEST_EXPLICIT_DELTA": "delta-value" },
                "timeout_ms": 15_000,
            }),
        )
        .await
        .unwrap();
    let handle = out.content["handle"].as_str().unwrap();
    let content = wait_for_state(&status, handle, "exited", Duration::from_secs(10)).await;
    std::env::remove_var("UTSUWA_TEST_SECRET_TOKEN");
    let stdout = content["stdout"].as_str().unwrap();
    assert!(!stdout.contains("super-secret-value"), "inherited secret reached child");
    assert!(stdout.contains("UTSUWA_TEST_EXPLICIT_DELTA=delta-value"));
}

#[tokio::test]
async fn timeout_kills_long_children() {
    let manager = manager();
    let sleep = tool_process::resolve_executable("sleep").unwrap();
    let spawn = SpawnTool {
        manager: manager.clone(),
        limits: ProcessLimits::default(),
    };
    let status = StatusTool { manager };
    let out = spawn
        .invoke(
            ticketed_ctx(&sleep),
            serde_json::json!({
                "executable": "sleep",
                "args": ["30"],
                "timeout_ms": 1_000,
            }),
        )
        .await
        .unwrap();
    let content = wait_for_state(
        &status,
        out.content["handle"].as_str().unwrap(),
        "timed_out",
        Duration::from_secs(10),
    )
    .await;
    assert!(content["exit_code"].is_null() || content["exit_code"].as_i64() != Some(0));
}

#[tokio::test]
async fn kill_is_forceful_and_idempotent() {
    let manager = manager();
    let sleep = tool_process::resolve_executable("sleep").unwrap();
    let spawn = SpawnTool {
        manager: manager.clone(),
        limits: ProcessLimits::default(),
    };
    let status = StatusTool { manager: manager.clone() };
    let kill = KillTool { manager };
    let out = spawn
        .invoke(ticketed_ctx(&sleep), spawn_args("sleep", &["30"]))
        .await
        .unwrap();
    let handle = out.content["handle"].as_str().unwrap().to_string();
    let snapshot = kill
        .invoke(
            ToolContext::new(Principal::Agent(AgentId::new("k"))),
            serde_json::json!({ "handle": handle }),
        )
        .await
        .unwrap();
    assert_eq!(snapshot.content["state"], "killed");
    // Second kill is a no-op reporting the same terminal state.
    let again = kill
        .invoke(
            ToolContext::new(Principal::Agent(AgentId::new("k"))),
            serde_json::json!({ "handle": handle }),
        )
        .await
        .unwrap();
    assert_eq!(again.content["state"], "killed");
    let _ = status;
}

#[tokio::test]
async fn spawn_requires_a_covering_ticket() {
    let manager = manager();
    let echo = tool_process::resolve_executable("echo").unwrap();
    let spawn = SpawnTool {
        manager: manager.clone(),
        limits: ProcessLimits::default(),
    };
    let plain = ToolContext::new(Principal::Agent(AgentId::new("no-ticket")));
    let err = spawn
        .invoke(plain, spawn_args("echo", &["hi"]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no capability ticket"), "{err}");

    // Ticket for another executable does not authorize this one.
    let other = PathBuf::from("/usr/bin/sleep");
    let err = spawn
        .invoke(ticketed_ctx(&other), spawn_args("echo", &["hi"]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not authorize"), "{err}");
    let _ = echo;
}

#[tokio::test]
async fn unknown_handles_and_bad_args_fail_closed() {
    let manager = manager();
    let echo = tool_process::resolve_executable("echo").unwrap();
    let status = StatusTool {
        manager: manager.clone(),
    };
    let kill = KillTool {
        manager: manager.clone(),
    };
    let ctx = ToolContext::new(Principal::Agent(AgentId::new("x")));
    for tool_name in ["process.status", "process.kill"] {
        let tool: Box<dyn Tool> = if tool_name == "process.status" {
            Box::new(StatusTool { manager: status.manager.clone() })
        } else {
            Box::new(KillTool { manager: kill.manager.clone() })
        };
        let err = tool
            .invoke(ctx.clone(), serde_json::json!({ "handle": "proc-0-999" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown process handle"), "{err}");
    }
    let _ = status;
    let _ = kill;

    let spawn = SpawnTool {
        manager: manager.clone(),
        limits: ProcessLimits::default(),
    };
    // Missing executable, NUL bytes, relative cwd, unknown binary.
    assert!(spawn
        .invoke(ticketed_ctx(&echo), serde_json::json!({}))
        .await
        .is_err());
    assert!(spawn
        .invoke(ticketed_ctx(&echo), spawn_args("echo\0", &[]))
        .await
        .is_err());
    assert!(spawn
        .invoke(
            ticketed_ctx(&echo),
            serde_json::json!({"executable": "echo", "cwd": "relative/path"}),
        )
        .await
        .is_err());
    assert!(spawn
        .invoke(
            ticketed_ctx(&echo),
            spawn_args("definitely-not-a-real-binary-xyz", &[]),
        )
        .await
        .is_err());
}

#[tokio::test]
async fn table_full_evicts_finished_first() {
    let limits = ProcessLimits {
        max_processes: 1,
        ..ProcessLimits::default()
    };
    let manager = ProcessManager::new(limits.clone());
    let sleep = tool_process::resolve_executable("sleep").unwrap();
    let echo = tool_process::resolve_executable("echo").unwrap();
    let status = StatusTool {
        manager: manager.clone(),
    };
    let spec = |exe: &PathBuf, args: Vec<String>| SpawnSpec {
        executable: exe.clone(),
        args,
        cwd: std::env::temp_dir(),
        env: HashMap::new(),
        timeout: Duration::from_secs(15),
    };
    // Occupy the single slot with a sleeper spawned directly.
    let (sleeper, _) = manager
        .spawn(&spec(&sleep, vec!["30".to_string()]))
        .unwrap();
    // Table is full of a live process: a second spawn fails.
    assert!(manager.spawn(&spec(&echo, vec![])).is_err());
    // Kill the sleeper, wait for it to settle, then the slot frees.
    manager.kill(&sleeper).unwrap();
    let start = Instant::now();
    loop {
        let snapshot = manager.status(&sleeper).unwrap();
        if snapshot.state != "running" {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "sleeper never died");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Finished entries are evicted to make room.
    let (handle, _) = manager.spawn(&spec(&echo, vec![])).unwrap();
    assert_ne!(handle, sleeper);
    let _ = status;
}
