# Utsuwa — Code Audit Report (Round 3)

Date: 2026-09-16 · Scope: remediation of all six Round-2 findings, plus
re-verification of the full tree and one new race found and fixed during
verification. Method: fix + focused regression test per finding (test-first,
failure observed before each fix), then the complete CI gate set. Working
tree on top of commit `e1ebac2` (uncommitted at report time).

## Verdict

All Round-2 findings are fixed and verified. No high-severity issues. One
new issue found during verification (a real spawn/store race behind the
Round-1 flaky test) — root-caused and fixed with a deterministic regression
test. No new vulnerabilities.

---

## M-1 · Approval label now names env vars and quotes args — fixed

`process.spawn` approvals render
`` `${executable} ${quoted args} (cwd …, env NAME, …)` ``
([permissions.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/services/native/permissions.ts:80)).
Env var *names* are listed (values never — they may be secrets), capped at 6
with a `(+N more)` overflow so a flooded env cannot hide the count, and args
containing whitespace or shell metacharacters are double-quoted
(`quoteArg`, `envVarNames`). A smuggled `NODE_OPTIONS` is now an instant red
flag instead of “1 env change”.

Tests: `permissions.test.ts` +2 (names + quoting, overflow cap). 10/10 pass.

## M-2 · MCP HTTP proxy: host allowlist + Origin check + docs — fixed

Three layers, all in the proxy path only (native Rust MCP untouched):

1. `MCP_HTTP_ALLOWED_HOSTS` operator allowlist
   ([shared.ts](/home/meme/Documentos/GitHub/utsuwa/src/routes/api/mcp/shared.ts:14)): exact,
   case-insensitive host match against the server URL, enforced in both
   `tools` and `call` routes before the HTTP client is built (off-list →
   403 `forbidden`). Unset/empty preserves the old any-host behavior.
2. `Origin` validation on non-GET `/api/*` in
   ([hooks.server.ts](/home/meme/Documentos/GitHub/utsuwa/src/hooks.server.ts:31)): a
   present-but-foreign origin is rejected (403) before rate-limit state is
   touched; missing origin (curl, server-to-server) still allowed.
3. [docs/deployment.md](/home/meme/Documentos/GitHub/utsuwa/docs/deployment.md) (new) + `.env.example`
   warnings: never expose the proxy beyond localhost without authenticated
   ingress; set the allowlist on any shared network.

Tests: `shared.test.ts` (new, 3), `hooks.server.test.ts` (new, 3: predicate,
403 on forged origin, pass-through same-origin). 6/6 pass.

Residual: an exposed deployment is still unauthenticated by design, and the
default allowlist is still open — the operator must opt into restriction.
The Origin check stops browsers, not direct callers; it is defense-in-depth,
not access control. Stated in the deployment doc.

## L-1 · Rate-limit proxy identity — documented

[docs/deployment.md](/home/meme/Documentos/GitHub/utsuwa/docs/deployment.md) covers it: `getClientAddress()`
returns the direct peer unless the adapter is configured for proxy headers
(single shared bucket behind a default reverse proxy); configure the
adapter (e.g. adapter-node `ADDRESS_HEADER`/`XFF_DEPTH`), strip the header
at the edge against spoofing, and use a shared limiter or authenticated
ingress for multi-instance (buckets are per-process).

## L-2 · Loader denylist extended — fixed

`is_injection_name`
([tool-process/src/lib.rs](/home/meme/Documentos/GitHub/utsuwa/crates/tool-process/src/lib.rs:468)) now
blocks `LD_PRELOAD`/`LD_AUDIT`/`LD_LIBRARY_PATH`, all four `DYLD_*` loader
vars, and the code-execution option vars `NODE_OPTIONS`,
`JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS`, `JDK_JAVA_OPTIONS`, `RUBYOPT`,
`PERL5OPT` — in both inherited env (stripped) and explicit deltas
(rejected). Deliberately still allowed: interpreter *module search* paths
(`PYTHONPATH`, `RUBYLIB`, `PERL5LIB`) — they need planted files to exploit,
have routine legitimate uses, and are now visible in the approval dialog
(M-1). Rationale is in the code comment.

Tests: `tool-process/tests/process.rs` +1 (all 11 new names rejected via a
ticket-covering invoke, so only the parser can refuse). Suite 11/11 pass.

## L-3 · Locked-vault edits merge on unlock + indicator — fixed

Previously, in-memory secret writes made while locked were silently dropped
(envelope preserved byte-for-byte; unlock overwrote memory). Now:

- `persist()` flags the state
  ([settings.svelte.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/stores/settings.svelte.ts:189)) whenever a
  locked save sees in-memory secrets it cannot persist;
- `unlockVault` snapshots them, decrypts, overlays them onto the vault
  contents (newer wins per provider / per server id), and re-saves;
- `VaultSettings` shows “Settings changed while the vault was locked.
  Unlock to merge and save them.”
  ([VaultSettings.svelte](/home/meme/Documentos/GitHub/utsuwa/src/lib/components/settings/VaultSettings.svelte:122)).

The merge is a pure, dependency-free helper with its own suite:
`settings-merge.test.ts` (new, 3). 3/3 pass. (The `.svelte.ts` store itself
cannot run under `node --test`; wiring is covered by `pnpm check` + builds.)

Residual: locked-time *deletions* cannot be represented in the overlay and
are still lost — documented in the helper. UI secret inputs stay disabled
while locked, so only programmatic writers hit this path.

## L-4 · Media CORS narrowed — fixed

The media handler's `Access-Control-Allow-Origin: *` is replaced with narrow
reflection ([audio.rs](/home/meme/Documentos/GitHub/utsuwa/crates/app-host/src/audio.rs:141)):
`companion://app` plus `http(s)` loopback (`localhost`, `127.0.0.1`, `::1`,
any port — keeps the `vite dev` page working). Missing, opaque, and foreign
origins — including `localhost.evil.com` / userinfo / fragment lookalikes —
get `null`, matching the asset server's semantics.

Tests: end-to-end through `MediaRegistry::handle` (5 reflected, 7 nulled).
app-host lib suite 101/101 pass (12 consecutive runs — see RACE-1).

## RACE-1 (new, fixed) · `spawn_turn` stored the worker handle after spawning

Found while verifying: `panicking_provider_factory_emits_exactly_one_turn_failed`
failed 3/14 pre-fix runs with `running.is_none()` false *after* exactly one
`turn_failed` was observed. Root cause, in
([turn.rs](/home/meme/Documentos/GitHub/utsuwa/crates/app-host/src/runtime/turn.rs:85)): the worker was
spawned, then `running = Some(handle)` was stored outside the lock — a
fast-failing worker could emit its terminal event (clearing `running`)
before the store landed, leaving a stale finished handle. Same shape could
let a superseded turn's store clobber the current turn's handle. Impact is
low (`abort()` on a finished handle is harmless; `cancel()`'s activity check
could misreport), but the invariant “terminal event ⇒ idle `running`” that
`emit_terminal` was carefully built to guarantee did not hold.

Fix: hold the state lock across spawn + store (spawn only schedules, both
callers hold no lock — no deadlock), and store only when the generation is
still current. Regression test `superseded_spawn_does_not_claim_running`
([runtime.rs](/home/meme/Documentos/GitHub/utsuwa/crates/app-host/src/runtime.rs:3652)) is fully
deterministic (stale-generation spawn, no timing): fails pre-fix, passes
post-fix. Stress: 12/12 full lib runs green (101 tests each).

---

## Round-1 findings — re-verification status

| ID | Finding | Status |
|----|---------|--------|
| HIGH-1 | stdio RCE via client argv/env | **Fixed, verified.** Both routes resolve through `pinnedStdioServer`; client command/args/env discarded; inventory parser tested; docs + UI hint updated. |
| HIGH-2 | SSRF bypass on models/chat routes | **Fixed, verified.** All 9 catalog helpers + `streamText` + tool-loop adapter use the guarded fetch; DNS + per-hop redirect validation tested. |
| MEDIUM-3 | plaintext keys in storage | **Fixed, verified** (native MCP tokens never touched `localStorage` — the vault covers web keys + native TTS/STT keys). Crypto reviewed again in Round 2: fresh salt+IV per envelope, PBKDF2-210k, AES-GCM auth, no passphrase/key persistence, wrong/short passphrase indistinguishable (`false`). |
| MEDIUM-4 | no rate/body limits | **Fixed, verified.** Hook + in-route backstops in place; proxy caveat now documented (L-1). |
| MEDIUM-5 | vulnerable JS deps | **Fixed, verified.** Audit re-run this round: gate passes (`no new high/critical advisories`). |
| L-3 | userinfo in error fallback | **Fixed, verified**, covered by `provider-errors.test.ts`. |
| L-4 | no IPC max request size | **Accepted** (trusted local WebView). Unchanged. |
| L-5 | Rust deps unaudited | **Fixed, verified.** `cargo audit` via the gate: no new high/critical advisories. |
| L-6 | flaky tests | **Fixed (root-caused).** The keyring race fix stands; the timing flake reproduced 3× this round and was root-caused to the RACE-1 spawn/store race — fixed, with a deterministic regression test. 12/12 lib runs + full workspace green since. |

## Threat model (updated)

Single-user localhost use remains the safe shape. Networked deployments are
still **fully unauthenticated** by design: with `MCP_ENABLED=server`, anyone
who can reach the server can drive HTTP MCP hosts (now operator-restrictable
via `MCP_HTTP_ALLOWED_HOSTS`), invoke pinned stdio tools by id, and use chat
with caller-supplied keys. Anything networked needs authenticated ingress
*and* an MCP exposure decision first — now stated in
[docs/deployment.md](/home/meme/Documentos/GitHub/utsuwa/docs/deployment.md) instead of living
only in this report.

## Evidence (observed 2026-09-16, working tree atop `e1ebac2`)

- `pnpm test`: **715/715** (704 baseline + 11 new: permissions 2, mcp-shared
  3, hooks 3, settings-merge 3).
- `pnpm check`: 0 errors, 0 warnings. `pnpm build` ✓, `pnpm build:native` ✓
  (`build/index.html` + `theme-init.js` present, CSP meta present).
- `cargo test --workspace --locked`: **622/622, 0 failed**
  (619 baseline + tool-process 1 + media CORS 1 + race 1).
- `cargo test -p app-host --locked`: 101 lib + 4 + 5, 0 failed; lib suite
  additionally stressed 12/12 green post-RACE-1-fix.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`:
  clean. `cargo fmt --all -- --check`: clean.
- `node scripts/audit-gate.mjs`: passes (“no new high/critical advisories”).
- `cargo build -p app-host --locked` (bundled frontend): ✓.
- Every new test was run before its fix and failed (import error, wrong
  label/header, allowed invoke, stored stale handle), then passed after.
