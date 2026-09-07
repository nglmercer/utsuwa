//! Structured process tools (plan Phase 19): `process.spawn`,
//! `process.status`, `process.kill`.
//!
//! No shell, ever: the executable and its arguments travel as separate
//! strings into [`std::process::Command`], so shell metacharacters are
//! inert data. Spawning requires a `ProcessSpawn` ticket scoped to the
//! resolved executable; `status`/`kill` act on unguessable handle ids
//! (knowledge of the handle is the authority — handles are minted per
//! spawn and never derived from user input).
//!
//! Defense in depth for children:
//!
//! ```text
//! env_clear + sanitized inheritance (secret-looking names and code
//! injection variables are stripped) + explicit caller deltas only
//! timeout (default 120 s, clamped to 10 min) enforced by a watchdog
//! stdout/stderr captured with per-stream caps (64 KiB)
//! at most `max_processes` live handles; finished ones are evicted first
//! ```

use capability_core::{Capability, CapabilityRequest, Resource};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};

/// Broker limits (plan Phase 36).
#[derive(Debug, Clone)]
pub struct ProcessLimits {
    /// Maximum tracked processes (running + finished, unreaped).
    pub max_processes: usize,
    /// Captured bytes kept per stream (stdout / stderr).
    pub max_output_bytes: usize,
    /// Timeout applied when the caller passes none.
    pub default_timeout: Duration,
    /// Timeout ceiling; larger requests are clamped, never rejected.
    pub max_timeout: Duration,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            max_processes: 16,
            max_output_bytes: 64 * 1024,
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillKind {
    Killed,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcLifecycle {
    Running,
    Exited,
    Killed,
    TimedOut,
}

impl ProcLifecycle {
    fn as_str(self) -> &'static str {
        match self {
            ProcLifecycle::Running => "running",
            ProcLifecycle::Exited => "exited",
            ProcLifecycle::Killed => "killed",
            ProcLifecycle::TimedOut => "timed_out",
        }
    }
}

struct ManagedProc {
    child: Option<Child>,
    started: Instant,
    timeout: Duration,
    kill: Option<KillKind>,
    /// Set once the child has been reaped. `exit_code` stays `None` for
    /// signal deaths (`ExitStatus::code` is `None` there) — finished-ness
    /// must never be derived from the code alone, or kills and timeouts
    /// would look "running" forever.
    reaped: bool,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_truncated: bool,
}

impl ManagedProc {
    fn lifecycle(&self) -> ProcLifecycle {
        if self.reaped {
            match self.kill {
                Some(KillKind::Killed) => ProcLifecycle::Killed,
                Some(KillKind::TimedOut) => ProcLifecycle::TimedOut,
                None => ProcLifecycle::Exited,
            }
        } else {
            ProcLifecycle::Running
        }
    }

    fn finished(&self) -> bool {
        self.reaped
    }

    /// Record a reaped child. Signal deaths carry no exit code.
    fn record_exit(&mut self, status: std::process::ExitStatus) {
        self.reaped = true;
        self.exit_code = status.code();
    }
}

/// What to spawn, after tool-arg parsing and ticket validation.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Explicit environment deltas applied over sanitized inheritance.
    pub env: HashMap<String, String>,
    pub timeout: Duration,
}

/// Snapshot returned by `process.status`.
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub handle: String,
    pub pid: u32,
    pub state: &'static str,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// Owns child handles. `ProcessManager::new` returns an `Arc` because
/// reader/watchdog threads share it.
pub struct ProcessManager {
    inner: Mutex<ManagerInner>,
    limits: ProcessLimits,
}

struct ManagerInner {
    next_id: u64,
    procs: HashMap<String, ManagedProc>,
    /// Insertion order for oldest-finished eviction.
    order: Vec<String>,
}

impl ProcessManager {
    pub fn new(limits: ProcessLimits) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(ManagerInner {
                next_id: 1,
                procs: HashMap::new(),
                order: Vec::new(),
            }),
            limits,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ManagerInner>, ToolError> {
        self.inner
            .lock()
            .map_err(|_| failed("process.spawn", "process table lock failed"))
    }

    /// Spawn a validated child. Returns its handle and OS pid.
    pub fn spawn(self: &Arc<Self>, spec: &SpawnSpec) -> Result<(String, u32), ToolError> {
        let mut inner = self.lock()?;
        if inner.procs.len() >= self.limits.max_processes {
            if !Self::evict_finished(&mut inner) {
                return Err(failed(
                    "process.spawn",
                    format!("process table full ({} live)", self.limits.max_processes),
                ));
            }
        }
        let handle = format!("proc-{}-{}", std::process::id(), inner.next_id);
        inner.next_id += 1;

        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(sanitized_inherited_env())
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| {
            failed(
                "process.spawn",
                format!("cannot execute '{}': {e}", spec.executable.display()),
            )
        })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        inner.order.push(handle.clone());
        inner.procs.insert(
            handle.clone(),
            ManagedProc {
                child: Some(child),
                started: Instant::now(),
                timeout: spec.timeout,
                kill: None,
                reaped: false,
                exit_code: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                output_truncated: false,
            },
        );
        drop(inner);

        // Drain pipes in the background so a verbose child can never
        // block on a full pipe while nobody polls `process.status`.
        let cap = self.limits.max_output_bytes;
        if let Some(stream) = stdout {
            let manager = Arc::clone(self);
            let id = handle.clone();
            std::thread::Builder::new()
                .name(format!("utsuwa-proc-io-{id}-out"))
                .spawn(move || drain_stream(&manager, &id, stream, true, cap))
                .map_err(|e| failed("process.spawn", format!("cannot drain child I/O: {e}")))?;
        }
        if let Some(stream) = stderr {
            let manager = Arc::clone(self);
            let id = handle.clone();
            std::thread::Builder::new()
                .name(format!("utsuwa-proc-io-{id}-err"))
                .spawn(move || drain_stream(&manager, &id, stream, false, cap))
                .map_err(|e| failed("process.spawn", format!("cannot drain child I/O: {e}")))?;
        }
        // Watchdog: force-kill past the deadline even if nobody polls.
        // Sleeps in slices so an early exit stops the thread instead of
        // holding it for the whole timeout.
        {
            let manager = Arc::clone(self);
            let id = handle.clone();
            let timeout = spec.timeout;
            std::thread::Builder::new()
                .name(format!("utsuwa-proc-watch-{id}"))
                .spawn(move || {
                    let start = Instant::now();
                    loop {
                        if manager.is_finished(&id) {
                            break;
                        }
                        if start.elapsed() >= timeout {
                            let _ = manager.enforce_timeout(&id);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })
                .map_err(|e| failed("process.spawn", format!("cannot arm timeout: {e}")))?;
        }
        Ok((handle, pid))
    }

    /// Remove the oldest finished entry. Returns false when everything
    /// tracked is still running.
    fn evict_finished(inner: &mut ManagerInner) -> bool {
        if let Some(pos) = inner
            .order
            .iter()
            .position(|id| inner.procs.get(id).is_some_and(|p| p.finished()))
        {
            let id = inner.order.remove(pos);
            inner.procs.remove(&id);
            true
        } else {
            false
        }
    }

    fn with_proc<T>(
        &self,
        tool: &str,
        handle: &str,
        f: impl FnOnce(&mut ManagedProc) -> T,
    ) -> Result<T, ToolError> {
        let mut inner = self.lock()?;
        match inner.procs.get_mut(handle) {
            Some(proc_) => Ok(f(proc_)),
            None => Err(invalid(tool, format!("unknown process handle '{handle}'"))),
        }
    }

    /// Poll the child (non-blocking) and record a natural exit.
    fn poll(&self, handle: &str) -> Result<(), ToolError> {
        self.with_proc("process.status", handle, |proc_| {
            if !proc_.reaped {
                if let Some(child) = proc_.child.as_mut() {
                    match child.try_wait() {
                        Ok(Some(status)) => proc_.record_exit(status),
                        Ok(None) => {}
                        // Lost the child outside our watch (ECHILD):
                        // finished with an unknown code, never "running"
                        // forever.
                        Err(_) => {
                            proc_.reaped = true;
                        }
                    }
                }
            }
        })
    }

    fn enforce_timeout(&self, handle: &str) -> Result<(), ToolError> {
        let mut inner = self.lock()?;
        let Some(proc_) = inner.procs.get_mut(handle) else {
            return Ok(());
        };
        if proc_.reaped || proc_.started.elapsed() < proc_.timeout {
            return Ok(());
        }
        proc_.kill = Some(KillKind::TimedOut);
        if let Some(child) = proc_.child.as_mut() {
            // Already-exited races report an error here; the next poll
            // records the real code. Never fail the watchdog on that.
            let _ = child.kill();
        }
        Ok(())
    }

    /// Snapshot a handle, polling for a natural exit first.
    pub fn status(&self, handle: &str) -> Result<StatusSnapshot, ToolError> {
        self.poll(handle)?;
        let inner = self.lock()?;
        let proc_ = inner
            .procs
            .get(handle)
            .ok_or_else(|| invalid("process.status", format!("unknown process handle '{handle}'")))?;
        let pid = proc_.child.as_ref().map(|c| c.id()).unwrap_or(0);
        Ok(StatusSnapshot {
            handle: handle.to_string(),
            pid,
            state: proc_.lifecycle().as_str(),
            exit_code: proc_.exit_code,
            stdout: String::from_utf8_lossy(&proc_.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&proc_.stderr).into_owned(),
            truncated: proc_.output_truncated,
        })
    }

    /// True once the handle reached a terminal state.
    fn is_finished(&self, handle: &str) -> bool {
        self.lock()
            .ok()
            .and_then(|inner| inner.procs.get(handle).map(|p| p.finished()))
            .unwrap_or(true)
    }

    /// Force-kill a running child (SIGKILL semantics, no graceful phase —
    /// use `process.status` to confirm). Idempotent on finished handles.
    pub fn kill(&self, handle: &str) -> Result<StatusSnapshot, ToolError> {
        self.poll(handle)?;
        self.with_proc("process.kill", handle, |proc_| {
            if !proc_.reaped {
                proc_.kill = Some(KillKind::Killed);
                if let Some(child) = proc_.child.as_mut() {
                    match child.kill() {
                        Ok(()) => {}
                        Err(_) => {
                            // Lost the race with a natural exit; record
                            // whatever the OS reports.
                            if let Ok(status) = child.wait() {
                                proc_.record_exit(status);
                            } else {
                                proc_.reaped = true;
                            }
                        }
                    }
                }
            }
        })?;
        // Give the OS a moment to deliver the signal and reap the
        // zombie, so `kill` reports the terminal state instead of a
        // transient "running".
        let start = Instant::now();
        while !self.is_finished(handle) && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
            let _ = self.poll(handle);
        }
        self.status(handle)
    }
}

/// Copy one pipe into the handle's bounded buffer.
fn drain_stream(
    manager: &ProcessManager,
    handle: &str,
    mut stream: impl Read + Send + 'static,
    is_stdout: bool,
    cap: usize,
) {
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let Ok(mut inner) = manager.inner.lock() else {
                    break;
                };
                let Some(proc_) = inner.procs.get_mut(handle) else {
                    break;
                };
                let target = if is_stdout { &mut proc_.stdout } else { &mut proc_.stderr };
                let room = cap.saturating_sub(target.len());
                if room == 0 {
                    proc_.output_truncated = true;
                    continue;
                }
                let take = n.min(room);
                target.extend_from_slice(&buf[..take]);
                if take < n {
                    proc_.output_truncated = true;
                }
            }
            Err(_) => break,
        }
    }
}

// -- environment ---------------------------------------------------------

/// Names that must never flow into a child implicitly. Case-insensitive
/// fragments: real secret env vars (`GITHUB_TOKEN`, `OPENAI_API_KEY`,
/// `AWS_SECRET_ACCESS_KEY`, …) all contain one.
fn is_secret_name(name: &str) -> bool {
    const FRAGMENTS: [&str; 6] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"];
    let upper = name.to_ascii_uppercase();
    FRAGMENTS.iter().any(|frag| upper.contains(frag))
}

/// Injection vectors with no legitimate per-child use.
fn is_injection_name(name: &str) -> bool {
    matches!(name, "LD_PRELOAD" | "DYLD_INSERT_LIBRARIES")
}

/// Inherited environment minus secrets and injection variables. Explicit
/// caller deltas (`SpawnSpec::env`) are applied on top — passing a
/// credential there is always a deliberate act, never ambient leakage.
pub fn sanitized_inherited_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(name, _)| !is_secret_name(name) && !is_injection_name(name))
        .collect()
}

// -- executable resolution -------------------------------------------------

/// Resolve the requested program without a shell: a path containing a
/// separator must exist (canonicalized, so scope checks see through
/// symlinks); a bare name is looked up on `PATH`. Fails closed.
pub fn resolve_executable(requested: &str) -> Result<PathBuf, ToolError> {
    if requested.contains('/') || requested.contains('\\') {
        let path = Path::new(requested);
        path.canonicalize().map_err(|_| {
            failed(
                "process.spawn",
                format!("executable '{requested}' does not exist or is unreadable"),
            )
        })
    } else {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(requested);
            if candidate.is_file() {
                return candidate.canonicalize().map_err(|_| {
                    failed(
                        "process.spawn",
                        format!("executable '{requested}' is unreadable"),
                    )
                });
            }
        }
        Err(failed(
            "process.spawn",
            format!("executable '{requested}' not found on PATH"),
        ))
    }
}

// -- ticket validation -----------------------------------------------------

/// The spawn ticket must cover `ProcessSpawn` on the resolved executable.
/// Mirrors the filesystem broker: the ticket — never the decision — opens
/// the OS.
fn authorized_executable(ctx: &ToolContext, executable: &Path) -> Result<(), ToolError> {
    let ticket = ctx.ticket.as_ref().ok_or_else(|| {
        denied(
            "no capability ticket: route process spawns through the agent + policy engine",
        )
    })?;
    let request = CapabilityRequest {
        principal: ctx.principal.clone(),
        capability: Capability::ProcessSpawn,
        resource: Resource::Executable(executable.to_path_buf()),
    };
    ticket
        .check(&ctx.principal, &request, &ctx.invocation_id)
        .map_err(|err| {
            denied(format!(
                "ticket does not authorize this spawn: {}",
                match err {
                    capability_core::TicketError::Expired => "capability ticket expired",
                    capability_core::TicketError::PrincipalMismatch =>
                        "ticket bound to a different principal",
                    capability_core::TicketError::InvocationMismatch =>
                        "ticket bound to a different invocation",
                    capability_core::TicketError::CapabilityMismatch
                    | capability_core::TicketError::ScopeMismatch =>
                        "ticket does not cover this executable",
                }
            ))
        })
}

// -- tools -----------------------------------------------------------------

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn denied(reason: impl Into<String>) -> ToolError {
    ToolError::Denied {
        tool: "process".to_string(),
        reason: reason.into(),
    }
}

fn failed(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::Failed {
        tool: tool.to_string(),
        message: message.into(),
    }
}

/// Reject NUL bytes: `Command` panics on them, and a panic is never an
/// acceptable answer to untrusted input.
fn check_no_nul(tool: &str, what: &str, value: &str) -> Result<(), ToolError> {
    if value.contains('\0') {
        return Err(invalid(tool, format!("{what} must not contain NUL bytes")));
    }
    Ok(())
}

fn capped_str(tool: &str, what: &str, value: &str, max: usize) -> Result<String, ToolError> {
    check_no_nul(tool, what, value)?;
    if value.len() > max {
        return Err(invalid(tool, format!("{what} exceeds {max} bytes")));
    }
    Ok(value.to_string())
}

fn parse_timeout_ms(value: Option<&serde_json::Value>, limits: &ProcessLimits) -> Result<Duration, ToolError> {
    let Some(value) = value else {
        return Ok(limits.default_timeout);
    };
    let ms = value.as_u64().ok_or_else(|| {
        invalid("process.spawn", "timeout_ms must be a non-negative integer")
    })?;
    Ok(Duration::from_millis(ms).clamp(Duration::from_secs(1), limits.max_timeout))
}

/// Spawn a structured child process. High risk: the agent runtime routes
/// this through `RequireUserApproval` unless a standing grant covers the
/// executable.
pub struct SpawnTool {
    pub manager: Arc<ProcessManager>,
    pub limits: ProcessLimits,
}

#[async_trait::async_trait]
impl Tool for SpawnTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("process.spawn"),
            description: "Spawn an executable with separate argv (no shell). Returns a handle for process.status / process.kill.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "executable": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string" },
                    "env": { "type": "object", "additionalProperties": { "type": "string" } },
                    "timeout_ms": { "type": "integer", "minimum": 0 },
                },
                "required": ["executable"],
            }),
            effects: vec![tool_core::ToolEffect::Process],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let executable = args.get("executable")?.as_str()?;
        // Resolve best-effort for the policy request; the broker
        // re-resolves and re-validates before touching the OS.
        let resource = match resolve_executable(executable) {
            Ok(path) => Resource::Executable(path),
            Err(_) => Resource::Executable(PathBuf::from(executable)),
        };
        Some(CapabilityRequirement {
            capability: Capability::ProcessSpawn,
            resource,
        })
    }

    async fn invoke(&self, ctx: ToolContext, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        const TOOL: &str = "process.spawn";
        let executable_raw = args
            .get("executable")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid(TOOL, "missing required 'executable'"))?;
        let executable_raw = capped_str(TOOL, "executable", executable_raw, 1024)?;
        if executable_raw.is_empty() {
            return Err(invalid(TOOL, "'executable' must not be empty"));
        }
        let executable = resolve_executable(&executable_raw)?;
        authorized_executable(&ctx, &executable)?;

        let mut call_args = Vec::new();
        if let Some(args_value) = args.get("args") {
            let list = args_value
                .as_array()
                .ok_or_else(|| invalid(TOOL, "'args' must be an array of strings"))?;
            if list.len() > 256 {
                return Err(invalid(TOOL, "'args' exceeds 256 entries"));
            }
            for (i, item) in list.iter().enumerate() {
                let text = item
                    .as_str()
                    .ok_or_else(|| invalid(TOOL, format!("'args[{i}]' must be a string")))?;
                call_args.push(capped_str(TOOL, &format!("'args[{i}]'"), text, 8192)?);
            }
        }

        let cwd = match args.get("cwd").and_then(|v| v.as_str()) {
            None => std::env::current_dir()
                .map_err(|e| failed(TOOL, format!("cannot determine working directory: {e}")))?,
            Some(raw) => {
                let raw = capped_str(TOOL, "cwd", raw, 1024)?;
                let path = PathBuf::from(&raw);
                if !path.is_absolute() {
                    return Err(invalid(TOOL, "'cwd' must be an absolute path"));
                }
                if !path.is_dir() {
                    return Err(invalid(TOOL, format!("'cwd' is not a directory: {raw}")));
                }
                path
            }
        };

        let mut env = HashMap::new();
        if let Some(env_value) = args.get("env") {
            let map = env_value
                .as_object()
                .ok_or_else(|| invalid(TOOL, "'env' must be an object of strings"))?;
            if map.len() > 64 {
                return Err(invalid(TOOL, "'env' exceeds 64 entries"));
            }
            for (name, value) in map {
                let value = value
                    .as_str()
                    .ok_or_else(|| invalid(TOOL, format!("'env.{name}' must be a string")))?;
                check_no_nul(TOOL, &format!("'env.{name}'"), name)?;
                check_no_nul(TOOL, &format!("'env.{name}'"), value)?;
                if name.is_empty() || name.len() > 256 || value.len() > 8192 {
                    return Err(invalid(TOOL, format!("'env.{name}' has an oversized name or value")));
                }
                env.insert(name.clone(), value.to_string());
            }
        }

        let timeout = parse_timeout_ms(args.get("timeout_ms"), &self.limits)?;
        let (handle, pid) = self.manager.spawn(&SpawnSpec {
            executable,
            args: call_args,
            cwd,
            env,
            timeout,
        })?;
        Ok(ToolOutput::new(serde_json::json!({
            "handle": handle,
            "pid": pid,
            "timeout_ms": timeout.as_millis() as u64,
        })))
    }
}

/// Poll a spawned child. Pure: knowledge of the unguessable handle is the
/// authority, so no ticket is required.
pub struct StatusTool {
    pub manager: Arc<ProcessManager>,
}

#[async_trait::async_trait]
impl Tool for StatusTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("process.status"),
            description: "Poll a spawned process: state, exit code, captured stdout/stderr.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "handle": { "type": "string" } },
                "required": ["handle"],
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        None
    }

    async fn invoke(&self, _ctx: ToolContext, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        const TOOL: &str = "process.status";
        let handle = args
            .get("handle")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid(TOOL, "missing required 'handle'"))?;
        let snapshot = self.manager.status(handle)?;
        Ok(ToolOutput::new(serde_json::json!({
            "handle": snapshot.handle,
            "pid": snapshot.pid,
            "state": snapshot.state,
            "exit_code": snapshot.exit_code,
            "stdout": snapshot.stdout,
            "stderr": snapshot.stderr,
            "truncated": snapshot.truncated,
        })))
    }
}

/// Force-kill a spawned child. Pure like `status` (handle knowledge is
/// the authority); killing only affects processes this host spawned.
pub struct KillTool {
    pub manager: Arc<ProcessManager>,
}

#[async_trait::async_trait]
impl Tool for KillTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("process.kill"),
            description: "Force-kill a process spawned by process.spawn. Idempotent.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "handle": { "type": "string" } },
                "required": ["handle"],
            }),
            effects: vec![tool_core::ToolEffect::Process],
        }
    }

    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        None
    }

    async fn invoke(&self, _ctx: ToolContext, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        const TOOL: &str = "process.kill";
        let handle = args
            .get("handle")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid(TOOL, "missing required 'handle'"))?;
        let snapshot = self.manager.kill(handle)?;
        Ok(ToolOutput::new(serde_json::json!({
            "handle": snapshot.handle,
            "state": snapshot.state,
            "exit_code": snapshot.exit_code,
        })))
    }
}
