# Utsuwa Native Rust Host Migration Plan

## Purpose

This document is the implementation plan for converting the **already-cloned Utsuwa project** into a native Rust desktop application with:

- `winit` for the event loop and window management
- `wry` for the embedded WebView
- Svelte / Three.js / VRM retained as the frontend and avatar layer
- a custom Rust IPC protocol
- a Rust-native agent runtime
- a capability-based permission system
- a unified tool registry
- MCP tool integration
- sandboxed WASM plugins
- optional out-of-process native plugins
- filesystem, process, search, accessibility, screen, keyboard, mouse, and computer-use tools

The project is already cloned.

**Do not clone another copy of the repository.**

Work in the existing repository only.

---

# 1. Non-Negotiable Architecture Rules

These rules override implementation convenience.

## 1.1 Do not use Tauri

Remove Tauri as a runtime dependency.

Do not introduce:

- `tauri`
- `tauri-plugin-*`
- Tauri commands
- Tauri capabilities
- Tauri IPC
- Tauri updater
- Tauri process APIs
- Tauri filesystem APIs

The final application must use direct Rust libraries.

Preferred stack:

```text
winit
wry
tokio
serde
serde_json
thiserror
tracing
uuid
schemars
```

Platform-specific libraries may be added where necessary.

---

## 1.2 The WebView must not receive direct OS authority

The frontend must never directly obtain:

- arbitrary filesystem access
- arbitrary process execution
- unrestricted network access
- shell execution
- plugin loading
- native library loading
- accessibility control
- mouse control
- keyboard control
- screenshot access

All privileged operations must go through Rust.

The frontend is presentation and interaction only.

---

## 1.3 Every privileged operation must pass through the permission kernel

Enforce this invariant:

```text
request
→ principal identity
→ capability requirement
→ resource scope
→ policy evaluation
→ optional user approval
→ capability ticket
→ capability broker
→ operating system
```

No model, tool, plugin, MCP server, or WebView code may bypass this flow.

---

## 1.4 Plugins must not directly receive privileged OS handles

Plugins must request capabilities through host APIs.

Default plugin permissions:

```text
filesystem = none
network = none
process = none
desktop = none
screen = none
clipboard = none
```

Permissions must be explicitly granted.

---

# 2. High-Level Target Architecture

```text
┌─────────────────────────────────────────────┐
│               Frontend / Body               │
│                                             │
│ Svelte                                      │
│ Three.js                                    │
│ VRM                                         │
│ Chat                                        │
│ Settings                                    │
│ Permissions UI                              │
│ Tool activity UI                            │
│                                             │
│                 WRY                         │
└──────────────────┬──────────────────────────┘
                   │
              strict JSON-RPC
                   │
                   ▼
┌─────────────────────────────────────────────┐
│              Native Rust Host               │
│                                             │
│ winit                                       │
│ wry                                         │
│ tokio                                       │
│                                             │
│ ┌─────────────────────────────────────────┐ │
│ │ Agent Runtime                           │ │
│ │                                         │ │
│ │ Model Router                            │ │
│ │ Conversation                            │ │
│ │ Agent Loop                              │ │
│ │ Tool Calling                            │ │
│ │ Memory                                  │ │
│ └──────────────────┬──────────────────────┘ │
│                    │                        │
│              Tool Registry                  │
│                    │                        │
│             Permission Kernel               │
│                    │                        │
│             Capability Broker               │
│          ┌─────────┼─────────┐              │
│          ▼         ▼         ▼              │
│      Built-ins     MCP      WASM             │
│                                         │   │
│               Native Plugin Host            │
│                  optional                   │
└──────────────────┬──────────────────────────┘
                   │
                   ▼
             Operating System

filesystem
processes
network
windows
accessibility
screen
mouse
keyboard
clipboard
applications
```

---

# 3. Repository Strategy

The current repository is already cloned.

Do not create a second repository.

Do not rewrite the frontend from scratch.

Reuse as much of the current Utsuwa frontend as practical:

- Svelte components
- Three.js scene
- VRM loading
- avatar rendering
- animations
- settings UI
- chat UI
- character system
- assets
- styles

Replace the desktop/runtime layer.

---

# 4. Target Repository Layout

Migrate toward this structure:

```text
repo/
├── frontend/
│   ├── src/
│   ├── static/
│   ├── package.json
│   └── ...
│
├── crates/
│   ├── app-host/
│   ├── ipc-core/
│   ├── agent-core/
│   ├── model-core/
│   ├── model-openai/
│   ├── model-anthropic/
│   ├── model-openai-compatible/
│   ├── tool-core/
│   ├── policy-core/
│   ├── capability-core/
│   ├── audit-core/
│   ├── tool-filesystem/
│   ├── tool-process/
│   ├── tool-network/
│   ├── tool-search/
│   ├── tool-desktop/
│   ├── desktop-windows/
│   ├── desktop-macos/
│   ├── desktop-linux/
│   ├── mcp-runtime/
│   ├── plugin-core/
│   ├── plugin-wasm/
│   ├── plugin-native-protocol/
│   └── memory/
│
├── plugin-host/
│
├── src/
│   └── main.rs
│
├── Cargo.toml
└── PLAN.md
```

Do not force this entire structure in the first commit.

Create crates only when their responsibility becomes real.

---

# 5. Phase 0 — Baseline and Safety

## Goal

Understand the existing cloned project before modifying architecture.

## Tasks

- inspect current repository structure
- identify frontend entrypoint
- identify VRM / Three.js integration
- identify current chat provider code
- identify all Tauri usage
- identify current `src-tauri`
- identify build scripts
- identify frontend assumptions about `window.__TAURI__`
- identify filesystem/process/plugin usage
- run the existing project before migration if practical
- record baseline behavior

## Create

```text
docs/migration-baseline.md
```

Document:

- current build command
- current dev command
- current desktop behavior
- current frontend-to-native calls
- current provider architecture
- current asset paths
- current VRM loading path

## Acceptance Criteria

- repository builds or existing failure is documented
- all Tauri integration points are known
- all frontend dependencies on Tauri are listed

---

# 6. Phase 1 — Rust Workspace

## Goal

Create the native Rust workspace without yet removing the existing app.

## Root `Cargo.toml`

Create a workspace.

Initial members:

```text
crates/app-host
crates/ipc-core
crates/capability-core
crates/policy-core
```

Recommended baseline dependencies:

```toml
tokio
winit
wry
serde
serde_json
thiserror
tracing
tracing-subscriber
uuid
schemars
async-trait
```

Avoid speculative dependencies.

## Acceptance Criteria

```bash
cargo check --workspace
```

passes.

---

# 7. Phase 2 — Native Window Host

## Goal

Launch a desktop window without Tauri.

## Implement

`crates/app-host`

Responsibilities:

- create `winit` event loop
- create application window
- create Wry WebView
- load development URL in dev mode
- load bundled frontend in production
- manage main window
- prepare support for avatar overlay window later
- forward Rust events to frontend
- receive IPC from frontend

Suggested interfaces:

```rust
pub trait WindowHost {
    fn emit(&self, event: HostEvent) -> Result<(), HostError>;
}
```

Do not expose raw window handles to other crates unless required.

## Acceptance Criteria

- native Rust executable opens
- frontend renders in Wry
- VRM avatar renders
- no Tauri executable is used

---

# 8. Phase 3 — Custom Frontend Protocol

## Goal

Stop depending on an embedded localhost production server.

Use a custom scheme such as:

```text
companion://app/
```

Production frontend assets should be served by Rust.

Allowed:

```text
companion://app/*
```

Navigation to external pages must not replace the application WebView.

External URL requests should become explicit host operations.

## Navigation Policy

Default:

```text
companion://app/*   allow
http://localhost:*  dev only
https://*           block navigation
http://*            block navigation
file://*            deny
unknown schemes     deny
```

External links should emit an IPC request such as:

```text
host.open_external_url
```

which Rust validates.

## Acceptance Criteria

Production build works without a local HTTP server.

---

# 9. Phase 4 — IPC Protocol

## Goal

Replace Tauri commands with one auditable protocol.

Use JSON-RPC-inspired envelopes.

## Request

```json
{
  "id": "uuid",
  "method": "agent.send_message",
  "params": {}
}
```

## Response

```json
{
  "id": "uuid",
  "result": {}
}
```

## Error

```json
{
  "id": "uuid",
  "error": {
    "code": "permission_denied",
    "message": "..."
  }
}
```

## Event

```json
{
  "event": "agent.text_delta",
  "data": {}
}
```

Create:

```text
crates/ipc-core
```

Define strongly typed Rust enums.

Frontend may use generated or manually mirrored TypeScript types.

## Initial Allowed IPC Methods

Only expose application-level methods:

```text
app.version
app.ready

agent.send_message
agent.cancel

permission.approve
permission.deny

settings.get
settings.set
```

Do not expose raw:

```text
fs.read
fs.write
process.spawn
shell.exec
```

to JavaScript.

## Acceptance Criteria

Frontend can send a ping/request and receive typed responses/events.

---

# 10. Phase 5 — Principal Identity Model

## Goal

Every privileged request must identify who is asking.

Create:

```text
crates/capability-core
```

Principal model:

```rust
pub enum Principal {
    User,
    Frontend,
    Agent(AgentId),
    BuiltinTool(ToolId),
    WasmPlugin(PluginId),
    McpServer(ServerId),
    NativePlugin(PluginId),
}
```

Do not use strings internally for trusted identity.

IDs should be typed wrappers.

## Acceptance Criteria

No privileged broker API can be called without a `Principal`.

---

# 11. Phase 6 — Capability Model

## Goal

Represent authority explicitly.

Initial capabilities:

```text
filesystem.read
filesystem.write
filesystem.create
filesystem.delete
filesystem.move

process.spawn
process.signal

network.connect

desktop.observe
desktop.control

screen.capture

clipboard.read
clipboard.write

application.launch
```

Represent capabilities with enums / structured values.

Do not build the entire future permission catalog immediately.

---

# 12. Phase 7 — Resource Scopes

Capabilities must have resource constraints.

Examples:

## Filesystem

```text
filesystem.read
  ~/Projects/**
```

## Network

```text
network.connect
  api.github.com:443
```

## Process

```text
process.spawn
  cargo
```

## Desktop

```text
desktop.control
  application:com.microsoft.VSCode
```

Create a `Resource` type:

```rust
pub enum Resource {
    Path(PathBuf),
    HostPort { host: String, port: u16 },
    Executable(PathBuf),
    Application(String),
    Window(WindowRef),
}
```

---

# 13. Phase 8 — Policy Engine

Create:

```text
crates/policy-core
```

The API should look conceptually like:

```rust
pub async fn authorize(
    principal: &Principal,
    request: &CapabilityRequest,
    context: &AuthorizationContext,
) -> AuthorizationDecision;
```

Possible decisions:

```rust
Allow
Deny
RequireUserApproval
```

## Grant Lifetimes

Support:

```text
once
task
session
persistent
```

Suggested enum:

```rust
pub enum GrantLifetime {
    Once,
    Task,
    Session,
    Persistent,
}
```

## User-facing options

```text
Deny

Allow once

Allow for this task

Allow for this session

Always allow for this folder
```

Avoid "Always allow everything" as the default UX.

---

# 14. Phase 9 — Capability Tickets

## Goal

A policy decision is not itself OS authority.

When a request is allowed, mint a short-lived ticket.

Concept:

```rust
pub struct CapabilityTicket {
    pub id: TicketId,
    pub principal: Principal,
    pub capability: Capability,
    pub scope: ResourceScope,
    pub invocation_id: InvocationId,
    pub expires_at: Instant,
}
```

Capability brokers require valid tickets.

Tickets should:

- expire
- be scoped
- be non-transferable where practical
- be traceable to an invocation
- be auditable

---

# 15. Phase 10 — Audit Log

Create:

```text
crates/audit-core
```

Record:

```text
timestamp
principal
tool
capability requested
resource
decision
grant lifetime
approval source
execution result
duration
```

For mutations additionally record:

```text
file path
before hash
after hash
```

Do not log secrets or full private file contents by default.

Frontend should eventually expose an Activity view.

---

# 16. Phase 11 — Tool Core

Create:

```text
crates/tool-core
```

Unified trait:

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn metadata(&self) -> ToolMetadata;

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError>;
}
```

Metadata:

```rust
pub struct ToolMetadata {
    pub id: ToolId,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub effects: Vec<ToolEffect>,
}
```

Possible effects:

```text
read_only
filesystem_write
destructive
network
process
desktop_control
external_side_effect
```

Tool permission requirements must be resolved before broker execution.

---

# 17. Phase 12 — Tool Registry

Implement a runtime registry.

Concept:

```rust
ToolRegistry
    builtin tools
    MCP tools
    WASM plugin tools
    native plugin tools
```

Required operations:

```text
register
unregister
list
resolve
invoke
```

Tool IDs must be namespaced.

Examples:

```text
filesystem.read
filesystem.patch
process.spawn

mcp.github.search_code

plugin.spotify.play
```

No duplicate IDs.

---

# 18. Phase 13 — Rust Model Provider Layer

Create:

```text
crates/model-core
```

Canonical internal types:

```rust
ModelRequest
ModelMessage
ModelResponse
ModelStreamEvent
ToolDefinition
ToolCall
ToolResult
```

Provider abstraction:

```rust
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
    ) -> Result<ModelStream, ModelError>;
}
```

Initial adapters:

```text
OpenAI-compatible
Ollama
LM Studio
```

Then:

```text
OpenAI
Anthropic
```

Do not allow provider-specific tool-call types to spread into `agent-core`.

---

# 19. Phase 14 — Agent Runtime

Create:

```text
crates/agent-core
```

Responsibilities:

- conversation state
- model invocation
- streaming output
- tool call parsing
- tool permission flow
- tool execution
- tool result insertion
- cancellation
- invocation IDs

Basic loop:

```text
User message
    ↓
Model
    ↓
assistant text ──→ frontend
    │
    └─ tool call
          ↓
      ToolRegistry
          ↓
      Policy Engine
          ↓
        allow?
       /      \
    yes       approval
     │           │
 execute      frontend
     │           │
 result ◀────────┘
     ↓
   Model
```

Do not implement uncontrolled autonomous infinite loops.

Include limits:

```text
max tool calls per turn
max agent iterations
max wall-clock task duration
max tool output size
```

---

# 20. Phase 15 — Move Chat Logic Out of TypeScript

Current frontend provider logic should progressively be replaced.

Frontend responsibility:

```text
render messages
render streaming text
render tool state
render permissions
render avatar reactions
send user input
```

Rust responsibility:

```text
provider API calls
stream parsing
tool calling
agent loop
permissions
memory
tool execution
```

Remove direct provider secrets from frontend storage where practical.

---

# 21. Phase 16 — Read-Only Filesystem Tools

Create:

```text
crates/tool-filesystem
```

Implement first:

```text
filesystem.list
filesystem.stat
filesystem.read
filesystem.read_range
filesystem.glob
filesystem.find
filesystem.search_text
```

Use path canonicalization.

Prevent scope escapes.

Handle symlinks deliberately.

Never perform:

```text
allowed_root.join(user_input)
```

without canonicalized scope validation.

## Acceptance Criteria

Agent can inspect an explicitly granted project directory but cannot read outside it.

---

# 22. Phase 17 — File Mutation Tools

Implement:

```text
filesystem.create
filesystem.write
filesystem.patch
filesystem.copy
filesystem.move
filesystem.delete
```

Prioritize:

```text
filesystem.patch
```

over whole-file rewrite.

Mutation request should carry an expected file hash when modifying existing content.

Concept:

```json
{
  "path": "src/main.rs",
  "expected_hash": "...",
  "patch": "..."
}
```

If current hash differs:

```text
reject with stale_file
```

## Permission UI

Show:

```text
tool
path
operation
diff
risk
requested scope
```

Actions:

```text
Allow once
Allow for task
Always allow this folder
Deny
```

---

# 23. Phase 18 — Search Tools

Separate simple file search from semantic memory.

Initial:

```text
search.files
search.filename
search.text
search.glob
```

Prefer native/ripgrep-like search before embeddings.

Later optional:

```text
search.semantic
```

Do not block initial implementation on embeddings.

---

# 24. Phase 19 — Process Tools

Create:

```text
crates/tool-process
```

Start with structured process spawning:

```rust
ProcessRequest {
    executable,
    args,
    cwd,
    env,
    timeout,
}
```

Tools:

```text
process.spawn
process.status
process.kill
```

Do not start with an unrestricted shell.

Sanitize inherited environment.

Secrets should not automatically reach spawned child processes.

Apply:

```text
timeout
stdout limit
stderr limit
process count limit
```

---

# 25. Phase 20 — Optional Shell Tool

Only after structured process execution is stable.

Add:

```text
shell.execute
```

Treat as high risk.

Require explicit permission unless constrained by a trusted task policy.

Always audit.

---

# 26. Phase 21 — MCP Runtime

Create:

```text
crates/mcp-runtime
```

Use the Rust MCP SDK.

MCP is a tool source, not the permission system.

Flow:

```text
MCP server
    ↓
discover tools
    ↓
MCP adapter
    ↓
ToolRegistry
    ↓
Policy Engine
```

External MCP tools must receive host-side risk metadata.

Do not automatically trust a tool because an MCP server advertises it.

MCP configuration should contain:

```text
server ID
transport
command / endpoint
environment allowlist
enabled state
trust level
```

Do not pass the entire parent environment to child MCP processes.

---

# 27. Phase 22 — WASM Plugin Runtime

Create:

```text
crates/plugin-core
crates/plugin-wasm
```

Use:

```text
Wasmtime
WebAssembly Component Model
WIT
```

WASM is the default third-party plugin format.

Plugin layout:

```text
plugins/
└── dev.example.plugin/
    ├── plugin.toml
    └── plugin.wasm
```

Manifest example:

```toml
[plugin]
id = "dev.example.plugin"
name = "Example"
version = "1.0.0"
api = 1

[runtime]
type = "wasm"

[permissions.filesystem]
read = []
write = []

[permissions.network]
hosts = []

[permissions.process]
commands = []
```

---

# 28. Phase 23 — WASM Host API

Do not expose unrestricted WASI.

Default:

```text
no filesystem
no process
no environment
no network
```

Expose narrow host interfaces.

Conceptually:

```text
host.log
host.tool.register

host.filesystem.read
host.filesystem.write

host.network.fetch

host.process.spawn

host.desktop.observe
host.desktop.control
```

Every privileged host call must route through the capability kernel.

Plugin identity must remain attached to the call.

---

# 29. Phase 24 — Plugin Lifecycle

Support:

```text
discover
validate
load
enable
disable
unload
reload
update
remove
```

Plugin activation must be independent from permission grants.

Example:

```text
installed = true
enabled = true
filesystem.write permission = false
```

The plugin can exist without having authority.

---

# 30. Phase 25 — Plugin Trust Levels

Recommended:

```text
Official
Verified
UnsignedWasm
LocalDevelopment
TrustedNative
```

Unsigned WASM may run sandboxed.

Unsigned native libraries should be disabled by default.

---

# 31. Phase 26 — Native Plugins

Native plugins are optional and later-stage.

Do not load arbitrary native libraries into the main process.

Preferred:

```text
main application
    ↓ IPC
plugin-host process
    ↓
native plugin
```

Create:

```text
plugin-host/
```

Use either:

```text
C ABI
```

or:

```text
abi_stable
```

Use `libloading` only inside the plugin-host process.

A plugin-host crash must not crash the main application.

---

# 32. Phase 27 — Desktop / Computer Use Core

Create:

```text
crates/tool-desktop
```

High-level interfaces:

```rust
trait DesktopBackend {
    async fn list_windows(...);
    async fn accessibility_tree(...);
    async fn invoke_element(...);
    async fn set_value(...);
    async fn screenshot(...);
    async fn click(...);
    async fn type_text(...);
}
```

Preferred strategy:

```text
accessibility APIs
    ↓
native element actions
    ↓
vision/screenshot fallback
```

Do not default to coordinate clicking.

---

# 33. Phase 28 — Windows Desktop Backend

Create:

```text
crates/desktop-windows
```

Implement:

```text
window enumeration
UI Automation tree
find controls
invoke controls
set control value
focus
screen capture
mouse
keyboard
```

Keep platform types inside the Windows crate.

---

# 34. Phase 29 — macOS Desktop Backend

Create:

```text
crates/desktop-macos
```

Use native accessibility APIs.

Eventually support:

```text
Accessibility API
ScreenCaptureKit
CGEvent or accessibility actions
```

Respect macOS TCC permissions.

Internal permission approval does not replace OS permission.

Both must allow.

---

# 35. Phase 30 — Linux Desktop Backend

Create:

```text
crates/desktop-linux
```

Wayland:

```text
XDG Desktop Portal
ScreenCast
Screenshot
RemoteDesktop
```

X11 may use separate native APIs.

Do not design around bypassing Wayland security.

---

# 36. Phase 31 — Browser / Web Automation

Do not grant the embedded application WebView general automation authority.

Browser tooling should be a separate capability.

Possible future strategies:

```text
browser automation protocol
accessibility
external browser process
CDP-compatible tooling
MCP browser server
```

Register browser actions as normal tools.

Examples:

```text
browser.open
browser.navigate
browser.read_page
browser.click
browser.type
```

All remain permissioned.

---

# 37. Phase 32 — Memory

Create:

```text
crates/memory
```

Start simple.

Use SQLite for:

```text
conversations
tool activity
persistent grants
plugin state
task state
memory entries
```

Do not require a vector database initially.

Semantic retrieval can be added later.

---

# 38. Phase 33 — Secret Storage

Secrets must not live in frontend JS local storage if avoidable.

Create a Rust secret abstraction.

Examples:

```text
OpenAI API key
Anthropic API key
MCP credentials
plugin OAuth tokens
```

Prefer platform keychain integration later.

Never expose all stored secrets to model context.

Never automatically pass all secrets to plugins or child processes.

---

# 39. Phase 34 — Approval UI

Frontend must support blocking permission requests.

Required views:

```text
permission dialog
diff preview
tool name
plugin/model identity
resource scope
risk level
grant duration
```

Example:

```text
The assistant wants to modify:

~/Projects/example/src/main.rs

Tool:
filesystem.patch

Requested by:
Agent / Coding Tool

[View Diff]

[Deny]
[Allow Once]
[Allow For Task]
[Always Allow This Folder]
```

---

# 40. Phase 35 — Tool Activity UI

Create an Activity panel.

Show:

```text
time
tool
status
resource
principal
duration
```

Examples:

```text
14:01  Read src/main.rs
14:02  Ran cargo check
14:02  Modified src/main.rs
14:03  Ran cargo check
```

Expandable details may show sanitized arguments and result metadata.

---

# 41. Phase 36 — Limits and Safety Controls

Agent runtime must enforce hard limits.

Recommended initial controls:

```text
maximum agent iterations per turn
maximum tool calls per turn
maximum process runtime
maximum tool output bytes
maximum file read size
maximum search results
maximum screenshot frequency
maximum concurrent tools
```

Add cancellation tokens everywhere long-running work can occur.

---

# 42. Phase 37 — Concurrency Rules

Main thread:

```text
winit
wry
window lifecycle
WebView callbacks
```

Tokio runtime:

```text
model calls
agent loop
filesystem
processes
MCP
WASM
database
network
```

Use channels between UI and async runtime.

Suggested:

```text
tokio::mpsc
tokio::oneshot
tokio::broadcast
winit EventLoopProxy
```

Do not block the window event loop.

---

# 43. Phase 38 — Logging and Observability

Use `tracing`.

Create spans for:

```text
agent turn
model request
tool invocation
permission evaluation
plugin call
MCP call
process
filesystem mutation
```

Never log secrets by default.

---

# 44. Phase 39 — Testing

Required test categories:

## Unit

```text
scope matching
permission decisions
ticket validation
tool metadata
path normalization
plugin manifests
IPC parsing
```

## Security regression

Test:

```text
../ path traversal
symlink escapes
scope bypass
expired tickets
ticket reuse
plugin identity spoofing
frontend direct privileged IPC
network wildcard mistakes
process allowlist bypass
```

## Integration

```text
frontend → IPC → agent
agent → tool
tool → approval
approval → broker
MCP registration
WASM registration
```

---

# 45. First Vertical Slice

Do not implement every phase before seeing the architecture work.

The first end-to-end vertical slice should be:

```text
Svelte UI
   ↓
Wry IPC
   ↓
Rust Agent
   ↓
OpenAI-compatible/Ollama model
   ↓
filesystem.read
   ↓
Policy Engine
   ↓
approved project directory
   ↓
tool result
   ↓
model
   ↓
frontend response
```

Then add:

```text
filesystem.patch
```

with approval and diff preview.

This proves the architecture.

---

# 46. Recommended First Tool Set

Start with only:

```text
filesystem.list
filesystem.stat
filesystem.read
filesystem.read_range
filesystem.search_text
filesystem.glob
```

Then:

```text
filesystem.patch
filesystem.create
```

Then:

```text
process.spawn
```

Do not start implementation with:

```text
shell.execute
mouse.click
keyboard.type
native plugin loading
```

---

# 47. LLM Implementation Rules

The LLM implementing this plan must follow these rules.

## Repository

- work in the existing cloned repository
- do not reclone
- do not initialize another repository
- do not replace project history
- do not delete existing frontend code unless migration requires it

## Architecture

- do not use Tauri
- do not restore Tauri for convenience
- use `winit` and `wry`
- use direct Rust crates
- keep privileged OS operations behind brokers
- keep the WebView unprivileged

## Changes

- make small compilable changes
- run relevant checks after structural changes
- prefer incremental migration over large rewrites
- preserve VRM/avatar functionality
- preserve existing visual design unless required

## Security

Never implement:

```text
frontend → raw shell
frontend → raw filesystem
model → raw OS API
plugin → raw OS API
MCP server → unrestricted OS API
```

Every privileged request must use the capability system.

## Dependencies

Before adding a dependency:

1. verify it is necessary
2. prefer actively maintained crates
3. avoid framework-level dependencies that recreate Tauri
4. document why unusually privileged dependencies are needed

## Code Quality

- avoid giant modules
- use typed IDs
- use structured errors
- no `unwrap()` in permission/security boundaries
- no unbounded channels for large data
- avoid global mutable state
- keep platform-specific code isolated

---

# 48. LLM Task Execution Format

For each implementation task:

## Before editing

State:

```text
Goal
Files likely affected
Architectural invariant involved
```

## During implementation

Keep the current phase scoped.

Do not opportunistically implement unrelated future phases.

## After editing

Report:

```text
Files changed
What was implemented
Checks run
Known remaining work
Next recommended task
```

---

# 49. Suggested Initial Task Sequence for an LLM

Execute in this order.

## Task 1

Inspect repository and create:

```text
docs/migration-baseline.md
```

Do not modify runtime behavior yet.

---

## Task 2

Create root Rust workspace and minimal:

```text
crates/app-host
crates/ipc-core
crates/capability-core
crates/policy-core
```

Run:

```bash
cargo check --workspace
```

---

## Task 3

Build a minimal `winit + wry` desktop executable showing a simple HTML page.

Do not integrate full frontend yet.

---

## Task 4

Load existing Svelte frontend in development mode.

Verify VRM rendering.

---

## Task 5

Implement custom production protocol:

```text
companion://app
```

---

## Task 6

Implement typed IPC:

```text
app.ready
app.version
```

---

## Task 7

Implement:

```text
Principal
Capability
Resource
CapabilityRequest
AuthorizationDecision
```

with unit tests.

---

## Task 8

Implement policy grants and capability tickets.

Add unit tests for:

```text
allow
deny
expiration
scope mismatch
```

---

## Task 9

Create `tool-core` and `ToolRegistry`.

Register a harmless test tool:

```text
system.echo
```

---

## Task 10

Create model-core and OpenAI-compatible provider.

Support Ollama / LM Studio-compatible chat.

---

## Task 11

Create minimal `agent-core`.

Support one model turn without tools.

---

## Task 12

Add tool calling.

Expose only:

```text
system.echo
```

---

## Task 13

Implement read-only filesystem broker and:

```text
filesystem.list
filesystem.read
```

---

## Task 14

Add permission approval UI.

Test denied and approved reads.

---

## Task 15

Implement:

```text
filesystem.search_text
filesystem.glob
```

---

## Task 16

Implement:

```text
filesystem.patch
```

with:

```text
expected hash
diff preview
approval
audit record
```

At this point the first meaningful coding-agent workflow exists.

---

# 50. Definition of Milestone 1

Milestone 1 is complete when:

- application runs without Tauri
- Winit owns the event loop
- Wry owns the WebView
- Utsuwa Svelte/VRM UI renders
- custom IPC works
- model interaction runs in Rust
- model can call registered tools
- policy engine can allow/deny tools
- user can approve a filesystem read/write
- model can read a granted project
- model can patch a granted project file
- all tool activity is audited
- frontend has no raw OS authority

---

# 51. Definition of Milestone 2

Milestone 2 is complete when:

- structured process execution works
- coding workflows can run build/test commands
- MCP servers can register tools
- MCP tools pass through host policy
- WASM plugins can register tools
- WASM plugins have zero authority by default
- WASM host calls pass through capability brokers
- plugin enable/disable/reload works

---

# 52. Definition of Milestone 3

Milestone 3 is complete when:

- native accessibility backend exists on at least one OS
- application/window enumeration works
- assistant can inspect UI elements
- assistant can invoke accessible controls
- screen capture fallback works
- mouse/keyboard fallback exists
- computer-use actions are permissioned and audited

---

# 53. Long-Term Goal

The desired final behavior is:

```text
User:
"Find the Rust project I worked on yesterday,
run the tests, fix the compile error,
and show me the changes."
```

The system should be able to:

```text
search files
    ↓
identify project
    ↓
request project read scope
    ↓
read code
    ↓
run cargo check
    ↓
inspect error
    ↓
request scoped write permission
    ↓
patch file
    ↓
run cargo check again
    ↓
show diff
    ↓
report result
```

Another example:

```text
User:
"Open the app and change the setting for me."
```

The system should prefer:

```text
accessibility tree
    ↓
find control
    ↓
request desktop.control
    ↓
invoke element
```

and use screenshot/vision/coordinate clicking only as fallback.

---

# 54. Core Design Principle

Keep this principle visible during all implementation work:

> The model decides what it wants to do.  
> The tool layer describes how it can be done.  
> The permission kernel decides whether it may be done.  
> The capability broker is the only code allowed to actually do it.

If a proposed implementation violates this separation, redesign it before merging.
