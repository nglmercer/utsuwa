# Utsuwa Migration Baseline (Phase 0)

Recorded before any runtime/architecture changes. The repo is the existing
`utsuwa` SvelteKit + Tauri v2 project — no reclone, work in place.

## Build / dev commands

- `pnpm dev` → `vite dev` (SvelteKit dev server, Tauri dev URL `http://localhost:5173`)
- `pnpm build` → `vite build` (SvelteKit static output; `frontendDist: ../build`)
- `pnpm tauri dev` / `pnpm tauri build` → desktop shell (requires Rust)
- `pnpm test` → `node --test` over `src/**/*.test.ts`
- `pnpm lint` / `pnpm check` → `svelte-check`
- No root `Cargo.toml`; the only Rust crate is `src-tauri` (`utsuwa_lib`, ed. 2021).

## Current desktop behavior (Tauri v2)

- `src-tauri/src/main.rs` → `utsuwa_lib::run()` (`src-tauri/src/lib.rs`).
- Two windows (`src-tauri/tauri.conf.json`): `main` (`/app`, 1200x800) and
  `overlay` (`/overlay`, transparent, decorations off, always-on-top,
  hidden by default).
- Tauri commands: `show_overlay`, `toggle_overlay` (used by
  `HotkeyHandler.svelte`, `TopRightButtons.svelte` via `@tauri-apps/api/core` `invoke`).
- Plugins: `global-shortcut`, `fs`, `opener`, plus desktop-only `updater`
  (signed GitHub releases) and `process` (restart).
- Capabilities (`src-tauri/capabilities/default.json`): broad
  `fs:allow-read-file` (`$HOME/**`, `$DESKTOP/**`, `$DOCUMENT/**`,
  `$DOWNLOAD/**`, `$PICTURE/**`, `$RESOURCE/**`, `$TEMP/**`, `/Volumes/**`,
  `/media/**`, `/mnt/**`; deny `.ssh`, `.aws`, `.gnupg`, `.config`) and
  `fs:allow-write-file` limited to `$DOWNLOAD/**`.

## Frontend → native calls (all Tauri today)

| Call site | Tauri API |
|---|---|
| `BottomChatBar.svelte` | `window.getCurrentWindow`, `plugin-fs.readFile` (attach files) |
| `VrmUploader.svelte` | `window.getCurrentWindow`, `plugin-fs.readFile` (load VRM) |
| `TopRightButtons.svelte`, `HotkeyHandler.svelte` | `core.invoke` (`show/toggle_overlay`), `window` |
| `InfoModal.svelte`, `LlmSettings.svelte`, `ServicesStep.svelte` | `plugin-opener.openUrl` (external docs links) |
| `services/platform/hotkeys.ts` | `plugin-global-shortcut` register/unregister(-all) |
| `services/platform/window.ts` | `window` position/visibility/always-on-top/click-through |
| `services/platform/platform.ts` | `__TAURI_INTERNALS__` / `__TAURI__` runtime detection |

No `invoke('...')` besides the two overlay commands; no shell/process/fs-write
usage from the frontend beyond the table above.

## Provider architecture (all TypeScript today — migration target for Rust)

- `src/lib/services/providers/`: `registry.ts`, `model-fetcher.ts`,
  `provider-defaults.ts`, `provider-errors.ts`, `local-endpoints.ts`,
  `health-check.ts`, `url-guard.ts`, `client-models.ts`, `use-model-fetch.ts`.
- Chat: `services/chat/` (`client-chat.ts`, `companion-chat.ts`,
  `companion-turn.ts`), prompt/response handling in `src/lib/ai/`.
- Local LLM: Ollama / LM Studio-compatible endpoints. Desktop origin notes in
  `src/content/docs/guides/local-llm-setup.md`: macOS `tauri://localhost`,
  Windows/Linux `http://tauri.localhost` (must be added to `OLLAMA_ORIGINS`).
  These origins disappear with the wry host and its custom scheme.
- API keys live in frontend storage today — must move to the Rust secret
  abstraction (plan Phase 33).

## Asset / VRM paths

- Routes: `/app` (main: `VrmScene` + chat + settings), `/overlay` (companion),
  plus marketing/docs/blog routes (web-only surface).
- VRM rendering: `three` + `@pixiv/three-vrm` in
  `src/lib/components/vrm/` (`Scene.svelte`, `VrmScene.svelte`,
  `VrmModel.svelte`, `ArPlacement.svelte`, `VrmUploader.svelte`);
  entrypoint `src/routes/app/+page.svelte`.
- Static assets: `static/`; SvelteKit adapter config in `svelte.config.js`;
  `__IS_DESKTOP__` compile-time flag wired via `vite.config.ts`
  (see `platform.ts`: preferred over `isTauri()` for routing — races first
  paint on macOS WKWebView).

## Baseline status

- `cargo` 1.97.1 available; system webview libs present
  (webkit2gtk-4.1 2.52.6, gtk+-3.0, libsoup-3.0) so `wry` can link.
- Existing Rust code untouched (`src-tauri/` still builds the Tauri app);
  the new native host grows in parallel under `crates/` + root workspace
  until it can replace it. Tauri remains runtime-only until Phase 2+ cuts over.
- Frontend VRM/chat/settings code is preserved as-is; only the
  desktop/runtime layer (`src-tauri/`, `@tauri-apps/*` imports,
  `__TAURI__` detection) is subject to replacement.
