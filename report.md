# Utsuwa — Code Audit Report

Date: 2026-09-16 · Scope: full tree (`src/`, `src/routes/`, `crates/`) with emphasis on
security boundaries (web API routes, SSRF guards, secret handling, chat HTML sinks,
Rust IPC) and the native-MCP/WebView-separation work. Method: manual code reading +
targeted scans + test/build evidence. Every finding below cites the file and line
I read; severity assumes the hosted web deployment (the native app keeps most of
this server-side code unreachable).

## Verdict

Two high-severity, internet-reachable issues in the web API layer need attention
before any public hosting: unauthenticated RCE through the MCP stdio proxy under
the documented configuration, and SSRF bypasses on the provider routes. Everything
else is medium or lower. The Rust host code, the MCP *HTTP* proxy guards, and the
chat HTML sanitizer are in genuinely good shape.

## Remediation (2026-09-16, same session)

All findings below are resolved except where a status says otherwise:

- **HIGH-1 FIXED** — stdio servers are now pinned to an operator inventory
  (`MCP_STDIO_SERVERS`, matched by id; client command/args/env discarded),
  with the command allowlist kept as a backstop; `.env.example` rewritten with
  a secure example + interpreter warning; web UI hint updated. Basename
  matching was deliberately kept (harmless once argv is operator-pinned).
- **HIGH-2 FIXED** — provider routes use a shared redirect-guarded fetch with
  DNS-resolution checks (`assertSafeProviderUrlResolved`); chat passes it to
  xsai via the runtime `fetch` option and to the tool-loop adapter.
- **MEDIUM-3 FIXED** — opt-in passphrase vault (PBKDF2 + AES-256-GCM) encrypts
  `providerConfigs` + `mcpServers` at rest, with lock/unlock/change/remove UI
  on all four key-holding settings pages, locked-state input guards, and a
  vault-aware chat error. Correction to the original finding: native MCP
  tokens were verified to never touch `localStorage` (Rust-owned + keychain);
  the vault additionally covers native TTS/STT keys.
- **MEDIUM-4 FIXED** — per-IP token-bucket rate limits on `/api/*` via
  `src/hooks.server.ts`, content-length caps, parsed-body backstops in all
  four POST routes, and a 500-message chat cap.
- **MEDIUM-5 FIXED** — svelte → 5.57.0 (8 SSR-XSS advisories), lodash-es →
  4.18.1 and sharp → 0.35.4 via `pnpm-workspace.yaml` overrides; `pnpm audit`
  went 30 → 14 findings. Remaining 6 high/critical are the protobufjs-6
  cluster (onnx-proto pins `^6`, unreachable for upgrade without breaking
  onnxruntime-web); exposure is supply-chain-only (pinned first-party
  models), allowlisted in `scripts/audit-gate.mjs`, which now runs in CI and
  fails on any *new* high/critical.
- **L-3 FIXED** — userinfo stripped in the unparseable-URL fallback.
- **L-4 ACCEPTED** — no max IPC request size (trusted local WebView);
  unchanged by design.
- **L-5 FIXED** — advisory DB wiped; audit now runs clean: wasmtime 34.0.2 →
  36.0.15 (2 critical sandbox escapes + 16 more) and rustls → 0.23.45.
  Four remaining warnings triaged as accepted: `paste`/`proc-macro-error`
  (build-time proc-macros, no runtime exposure), `ttf-parser` (needs an
  upstream winit/sctk-adwaita migration), `glib` Variant unsoundness (our
  crates only use MainContext/timeout; fix needs a gtk-rs major migration).
- **L-6 FIXED (keyring race)** — the secret-core flake was a real race (two
  tests sharing one keyring account); accounts are now unique per test (6/6
  green, was ~1-in-3 flaky). The app-host timing flake did not reproduce in
  8 isolation + 2 workspace runs; failure diagnostics (event timeline dump)
  were added so the next occurrence is actionable.

Verification after remediation: 704/704 frontend tests, `pnpm check` 0/0,
web + native builds pass, `cargo test` 619/619 (twice), clippy clean,
`cargo audit` 0 vulnerabilities, JS audit gate passes.

---

## HIGH-1 · Unauthenticated RCE via MCP stdio proxy (documented config)

Chain, all verified by reading:

1. No authentication exists on any `/api/*` route — there is no `hooks.server`
   file ([src/hooks.ts](/home/meme/Documentos/GitHub/utsuwa/src/hooks.ts) only reroutes
   subdomains) and no session/cookie checks in any route handler.
2. `POST /api/mcp/tools` and `POST /api/mcp/call` accept a **complete
   `McpServerConfig` from the request body** — command, args, and env included
   ([tools/+server.ts](/home/meme/Documentos/GitHub/utsuwa/src/routes/api/mcp/tools/+server.ts:14)) —
   and pass it to `runStdioMethod`, which does
   `spawn(server.command, server.args ?? [], { env: { ...BASE_ENV, ...(server.env ?? {}) } })`
   ([stdio.ts](/home/meme/Documentos/GitHub/utsuwa/src/routes/api/mcp/stdio.ts:66)).
3. The only gate is the command allowlist. But `args` get only NUL-filtering and
   `env` only a 64-entry cap ([types.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/services/mcp/types.ts:149)).
4. `.env.example` documents `MCP_STDIO_ALLOWED_COMMANDS=uvx,npx` — both are
   package runners: `uvx <attacker-package>` / `npx -y <attacker-package>` is
   arbitrary code execution with fully client-controlled argv.

So: proxy enabled (`MCP_ENABLED=server`) + documented allowlist + reachable
deployment = **unauthenticated remote code execution**. Two aggravating details:
basename matching means an allowlisted `uvx` also permits `/any/path/uvx`
([stdio.ts](/home/meme/Documentos/GitHub/utsuwa/src/routes/api/mcp/stdio.ts:36)), and the
client-supplied `env` is spread *after* `BASE_ENV`, so `PATH` (and
`LD_PRELOAD`-class variables) can be overridden per request.

Mitigating facts: MCP is off by default and the allowlist defaults to refuse-all.
The spawn itself is argv-based (no shell), the base env is minimal, and there is
no process pooling. The design is fail-closed; the hole is *args/env passthrough
combined with an interpreter-class documented default*.

**Fix (in order of strength):** (a) change the documented example to fixed,
non-interpreter binaries and add a warning that `uvx`/`npx`/`python*`/`node`
must never be allowlisted on a networked deployment; (b) restrict `args` for
allowlisted commands (exact-arg templates or deny `--`/`-c`-style flags);
(c) strip/allowlist `env` keys instead of merging client env over the base;
(d) require absolute allowlist paths and drop basename matching.

## HIGH-2 · SSRF bypass on `/api/providers/models` and `/api/chat`

Both routes validate the client-supplied base URL with `assertSafeProviderUrl`
([url-guard.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/services/providers/url-guard.ts:88)), which
only inspects the URL **string**: scheme + literal-IP/hostname blocklist. The
literal-IP coverage itself is thorough (decimal/hex/octal/short-form IPv4,
IPv6, `localhost`). But the fetch that follows is a plain redirect-following
`fetch`, so the guard is bypassed two independent ways:

- **DNS rebinding.** `evil.com` passes the string check; at fetch time it
  resolves to `169.254.169.254` (or any internal host). Nothing re-checks DNS.
- **Redirects.** `fetch` follows 3xx by default (including downgrades and
  cross-origin hops); the `Location` target is never validated. The models route
  returns the fetched JSON to the caller, giving **full read-SSRF** (cloud
  metadata, localhost services). The chat route's `streamText({ baseURL })`
  ([chat/+server.ts](/home/meme/Documentos/GitHub/utsuwa/src/routes/api/chat/+server.ts:208)) uses the
  library default fetch, same exposure (noted as inferred for the xsai internals;
  the DNS-rebinding half needs no redirect support at all).

No auth (see HIGH-1) makes this internet-reachable on a hosted deployment.

The frustrating part: the codebase already contains the correct solution. The MCP
proxy's `createGuardedFetch` pins DNS per hop, re-validates every redirect (max 3),
and drops `Authorization` across origins
([guarded-fetch.ts](/home/meme/Documentos/GitHub/utsuwa/src/routes/api/mcp/guarded-fetch.ts:21),
[ssrf-guard.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/services/web/mcp/ssrf-guard.ts:69)).

**Fix:** route the models/chat provider fetches through `createGuardedFetch` +
the DNS resolver (adapting its LAN-allowing policy to the stricter
`ALLOW_LOCAL_PROVIDER_HOSTS` default), or extract a shared guarded fetch. One
guard implementation, used everywhere, is the actual requirement.

---

## MEDIUM-3 · Plaintext API keys and MCP tokens in `localStorage`

On web builds, `providerConfigs` — including every provider `apiKey` — persist to
`localStorage` as plain JSON ([settings.svelte.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/stores/settings.svelte.ts:91)).
Desktop builds strip LLM keys (keychain) but still persist `mcpServers`, which
carry HTTP `bearerToken`s ([types.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/services/mcp/types.ts:20)).
Any XSS in the app origin (or any local attacker reading the profile) exfiltrates
all keys. The chat sanitizer held up under audit (see “Verified good”), so this
is defense-in-depth, not an active hole — but the asymmetry (keychain on native,
plaintext on web) should be a conscious, documented decision.

**Fix:** document the tradeoff; consider WebCrypto-encrypted storage with a
user-supplied passphrase for the web vault, and strip `bearerToken` from the
persisted native draft (it already travels via `mcp.set_server_token`).

## MEDIUM-4 · No rate limiting or body limits on `/api/*`

No throttling, no `429` paths, and unbounded `request.json()` on chat, models,
and both MCP routes. Unauthenticated callers can: spawn one stdio child per
request (fork-amplified DoS), stream arbitrarily long LLM turns through the
server (cost/SSRF amplifier combined with HIGH-2), and POST huge chat histories.
SvelteKit/adapter defaults are the only backstop.

**Fix:** per-IP rate limits on `/api/*` (tightest on chat + MCP call), and cap
request bodies / message-history length before the provider call.

## MEDIUM-5 · Transitive JS dependencies with high/critical advisories

`pnpm audit`: 30 findings — 1 critical (protobufjs arbitrary code execution),
8 high (protobufjs codegen/DoS, lodash-es `_.template` code injection,
sharp/libvips + libheif CVEs). Reachability analysis: **no direct imports** of
lodash in `src/`; the protobufjs/sharp chains arrive via `@xenova/transformers`,
used only from browser pages ([embeddings.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/services/embeddings.ts:1),
browser-gated). So the critical RCE is *client-side* (exploitable via malicious
model files from the CDN supply chain, not remotely), and `_.template` injection
needs app code that doesn't exist. Downgraded to medium, but:

**Fix:** upgrade `@xenova/transformers` (or pin `pnpm.overrides` for
protobufjs/sharp) and re-run audit; add `pnpm audit --audit-level=high` to CI.

---

## LOW (fix opportunistically)

- **L-1 · JSON-LD `{@html JSON.stringify(...)}`** in `+page.svelte`, `blog/[slug]`,
  `docs/[...slug]`, `download/`: payload is static strings / author frontmatter —
  safe today. Keep it that way: never interpolate user- or CMS-attacker-controlled
  text here, since `</script>` inside the JSON would break out of the block.
- **L-2 · `{@html result.excerpt}`** ([DocsSearch.svelte](/home/meme/Documentos/GitHub/utsuwa/src/lib/components/docs/DocsSearch.svelte:179)):
  Pagefind excerpt HTML generated from first-party docs. Self-content only; no action
  unless docs ever embed third-party content.
- **L-3 · `safeEndpointReference` fallback** ([provider-errors.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/services/providers/provider-errors.ts:8)):
  the `URL`-parseable path strips userinfo via `origin`, but the unparseable
  fallback keeps `user:pass@host`. Strip `[^/@]*@` there too.
- **L-4 · Rust IPC has no max request size.** The WebView is trusted local UI, so
  this is acceptable — noted only so a future remote-debug bridge doesn't inherit
  the assumption silently.
- **L-5 · `cargo audit` could not run** (local advisory-DB clone is corrupt:
  `expected RUSTSEC-0000-0000 to be in gettext-sys directory`). Rust
  dependencies are unaudited — wipe `~/.cargo/advisory-db` and re-run.
- **L-6 · Pre-existing flaky tests** (environmental, fail intermittently in this
  sandbox, pass in isolation; untouched by recent work): `secret-core` keyring
  round-trip (D-Bus) and one `app-host` timing-sensitive event test.

## Verified good (checked, no finding)

- **Chat HTML rendering.** `renderMarkdown` escapes `&<>` first and injects only
  `<strong>/<em>/<code>` ([render-markdown.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/components/chat/render-markdown.ts:8));
  `wrapWordsInHtml` only re-tokenizes that output and interpolates into element
  content, never attributes ([reveal-markup.ts](/home/meme/Documentos/GitHub/utsuwa/src/lib/components/chat/reveal-markup.ts:9)).
  No XSS path found; regexes are backtracking-linear; 18 markup tests pass.
- **Other HTML sinks.** Tool receipts use escaped `{...}` interpolation; icon
  bodies and copy-button SVGs are static constants; no other `innerHTML` sinks.
- **Production Rust IPC/MCP.** Zero `unwrap`/`expect`/`panic!` outside test
  modules in `ipc/*`, `ipc-core`, `mcp-runtime`; the one `unsafe` block is
  documented WebKit version getters; no secret values in tracing (URLs
  sanitized); typed `IpcMethod` enum rejects unknown methods.
- **MCP HTTP proxy guards** are exemplary (DNS pinning, per-hop redirect
  validation, cross-origin auth drop) — HIGH-2 is about the provider routes not
  reusing them.
- **Recent WebView-separation work** (re-audited with fresh eyes): the chat-model
  denylist was tested against 20 known chat-model IDs with zero false positives;
  no dangling references to removed executor modes/types; native bundle audit
  confirmed web runtimes are lazy-only chunks absent from entry; LMStudio
  fallback chain is now identical across web, Rust, and health paths; Rust
  keychain fallback is Kilo-scoped on both sides of the IPC.
- **Hygiene.** No hardcoded secrets/tokens found; `console.log` output is
  dev-gated; no TODO/FIXME debt in `src/lib` or `src/routes`.

## Threat-model note

Utsuwa's web deployment currently has **no authentication layer at all**. HIGH-1
and HIGH-2 are both “anyone who can reach the server.” If the web app is meant
for single-user/self-hosted use, the cheapest global mitigation is to bind it to
localhost / put it behind authenticated ingress and say so in the docs; if it is
meant to be multi-user, API auth must come before any other hardening.
