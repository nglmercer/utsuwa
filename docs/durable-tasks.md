# Durable tasks: reliability layer

How the native task authority survives crashes, avoids double-firing side
effects, schedules generic work, and runs with the app closed.

## Receipts and idempotency

Every tool-step attempt records an `ExecutionReceipt` in `tasks.db`
(`execution_receipts`, schema v2 — v1 databases migrate on open):

- one row per attempt, keyed by a deterministic idempotency key
  (`task:step:operation:attempt-N`), so retries never overwrite history;
- `success` receipts carry the full step output and are replayed: when a
  retry finds a prior success for the same (task, step, operation), the
  runner returns the stored output instead of re-invoking the tool;
- `failed` receipts are audit only and never replay; `unknown_outcome`
  marks timeouts, where the effect may or may not have happened.

The crash this defeats: effect fires, host dies before the step
completion persists, recovery requeues the step. Without receipts the
retry re-fires the side effect; with receipts it replays. Receipt loss
never fails a step — the step result stays the source of truth.

## Crash recovery

All scheduling state lives in `tasks.db`, so a restart converges:

- `Running` tasks with an expired lease requeue to `Ready` (or `Failed`
  when attempts are exhausted); the interrupted step resets to `Pending`.
- `Waiting` duration waits resume and expire on their original timeout.
- `needs_review` tasks stay parked with their review reason intact.

The `crates/task-core/tests/crash_recovery.rs` and
`crates/task-host/tests/fault_injection.rs` suites pin this by dropping
file-backed stores mid-state and asserting recovery after reopen —
including a tool effect that fires exactly once across a simulated
crash. Exactly one driver must run at a time (app, `task-cli run`, or
the daemon below): two tick loops racing can double-execute steps.

## Model-facing task tools

The default agent manages durable work through `tasks.*`:

- `tasks.create_interval` — one instruction repeated N times with gaps
  (and optional scheduled start). Prefer this for plain repetitions.
- `tasks.create` — any explicit step list: one-shot work, custom
  multi-step plans, scheduled runs. Agent steps need `input.prompt`,
  tool steps need `input.tool` plus `input.args`; malformed plans fail
  at creation with `invalid_steps` instead of mid-run. Bounded to 30
  steps and 10 whole-task attempts.
- `tasks.list` / `tasks.get` — summaries and full detail (never poll in
  a loop); `tasks.cancel` — final; `tasks.edit` — only tasks that never
  started (pending/scheduled, 0 attempts).

Side-effecting steps still authorize at execution time: ungated calls
park in `needs_review` with a structured capability request, and
approving mints a single-use ticket for exactly that step.

## Closed-app execution

Tasks normally run while the app runs. `task-cli daemon` is the
closed-app driver: it opens the shared `tasks.db`, runs the same tick
loop plus worker pool as the app, prints terminal results, and reports
parked reviews every 30s until Ctrl-C:

```sh
cargo run -p app-host --bin task-cli -- daemon --yes
```

Rules: close the app first (one driver only), and pass `--yes` only
when unattended approval is acceptable — otherwise review tasks park
until approved in the app (or the Task Center). Avatar-routine steps
need the app renderer and cannot complete headlessly.

## Model-gate fairness

Interactive chat and background agent steps share one model gate.
Interactive turns keep priority, but background turns age: a waiter is
admitted despite queued interactive turns after 8 consecutive
interactive admissions or 30s of waiting (see `GateConfig`), so
background work stalls briefly, never forever. `snapshot()` exposes
per-kind admissions plus total/max queueing waits for diagnostics.

## Task Center

Settings → Tasks lists every durable task with lifecycle filters,
per-step progress, attempts, errors, and review actions (approve /
reject / cancel). It talks to the same `task.*` IPC as the daemon and
the model tools — one authority, three surfaces.
