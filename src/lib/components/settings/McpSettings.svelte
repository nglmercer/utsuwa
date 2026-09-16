<script lang="ts">
	import { browser } from '$app/environment';
	import { onMount } from 'svelte';
	import { settingsStore } from '$lib/stores/settings.svelte';
	import { Icon } from '$lib/components/ui';
	import { getBridge } from '$lib/services/native/bridge';
	import { isNativeRuntimeAvailable } from '$lib/services/platform';
	import {
		connectNativeMcpServer,
		getNativeMcpServers,
		getNativeMcpStatus,
		setNativeMcpServers,
		setNativeMcpServerToken,
		type NativeMcpStatus
	} from '$lib/services/native/mcp';
	import {
		parseMcpServerConfigs,
		type McpServerConfig
	} from '$lib/services/mcp/types';
	import './ai-services-settings.css';

	// Runtime boundary: on native builds this panel configures the Rust
	// MCP runtime over IPC (Rust executes; the UI never touches MCP
	// networking). On web builds it drives the local settings store and
	// the /api/mcp proxy. `mounted` gates the branch so SSR never
	// hydration-mismatches against the native branch.
	let mounted = $state(false);
	let native = $state(false);

	interface Draft {
		id: string;
		name: string;
		transport: 'http' | 'stdio';
		url: string;
		bearerToken: string;
		command: string;
		argsText: string;
		envText: string;
		enabled: boolean;
	}

	const EMPTY_DRAFT: Draft = {
		id: '',
		name: '',
		transport: 'http',
		url: '',
		bearerToken: '',
		command: '',
		argsText: '',
		envText: '',
		enabled: true
	};

	let editingId = $state<string | null>(null);
	let draft = $state<Draft>({ ...EMPTY_DRAFT });
	let formError = $state<string | null>(null);
	let showToken = $state(false);
	let tokenTouched = $state(false);
	let proxyEnabled = $state<boolean | null>(null);
	let testingId = $state<string | null>(null);
	let testResult = $state<{ id: string; ok: boolean; message: string } | null>(null);
	let saving = $state(false);

	// Native backend state (Rust-owned; never mirrored to localStorage).
	let nativeServers = $state<McpServerConfig[]>([]);
	let nativeStatus = $state<NativeMcpStatus[]>([]);
	let nativeLoading = $state(false);
	let nativeError = $state<string | null>(null);

	const servers = $derived(native ? nativeServers : settingsStore.mcpServers);
	// Native has no global kill switch: per-server `enabled` gates execution.
	const mcpEnabled = $derived(native ? true : settingsStore.mcpEnabled);

	function invoke(method: string, params?: Record<string, unknown>): Promise<unknown> {
		const bridge = getBridge();
		if (!bridge) return Promise.reject(new Error('Native bridge is unavailable'));
		return bridge.invoke(method, params);
	}

	function statusFor(id: string): NativeMcpStatus | undefined {
		return nativeStatus.find((s) => s.id === id);
	}

	onMount(() => {
		if (!browser) return;
		native = isNativeRuntimeAvailable();
		mounted = true;
		if (native) {
			void refreshNative();
		} else {
			void refreshProxyStatus();
		}
	});

	async function refreshProxyStatus() {
		try {
			const res = await fetch('/api/mcp/status');
			if (!res.ok) {
				proxyEnabled = false;
				return;
			}
			const data = (await res.json()) as { enabled?: unknown };
			proxyEnabled = data.enabled === true;
		} catch {
			proxyEnabled = false;
		}
	}

	async function refreshNative() {
		if (nativeLoading) return;
		nativeLoading = true;
		nativeError = null;
		try {
			const [servers, status] = await Promise.all([
				getNativeMcpServers(invoke),
				getNativeMcpStatus(invoke).catch(() => [] as NativeMcpStatus[])
			]);
			nativeServers = servers;
			nativeStatus = status;
		} catch (e) {
			nativeError = e instanceof Error ? e.message : 'Could not load native MCP settings';
		} finally {
			nativeLoading = false;
		}
	}

	function startAdd() {
		editingId = '__new__';
		draft = { ...EMPTY_DRAFT };
		formError = null;
		showToken = false;
		tokenTouched = false;
	}

	function startEdit(server: McpServerConfig) {
		editingId = server.id;
		draft = {
			id: server.id,
			name: server.name ?? '',
			transport: server.transport,
			url: server.transport === 'http' ? server.url : '',
			// Tokens are write-only on native (keychain) and are never read
			// back; on web the stored value pre-fills for convenience.
			bearerToken: !native && server.transport === 'http' ? (server.bearerToken ?? '') : '',
			command: server.transport === 'stdio' ? server.command : '',
			argsText: server.transport === 'stdio' ? (server.args ?? []).join('\n') : '',
			envText:
				server.transport === 'stdio'
					? Object.entries(server.env ?? {}).map(([k, v]) => `${k}=${v}`).join('\n')
					: '',
			enabled: server.enabled
		};
		formError = null;
		showToken = false;
		tokenTouched = false;
	}

	function cancelEdit() {
		editingId = null;
		formError = null;
	}

	function draftToConfig(): Record<string, unknown> {
		const base: Record<string, unknown> = {
			id: draft.id.trim(),
			transport: draft.transport,
			enabled: draft.enabled
		};
		if (draft.name.trim()) base.name = draft.name.trim();
		if (draft.transport === 'http') {
			base.url = draft.url.trim();
			if (draft.bearerToken) base.bearerToken = draft.bearerToken;
		} else {
			base.command = draft.command.trim();
			const args = draft.argsText.split('\n').map((a) => a.trim()).filter(Boolean);
			if (args.length > 0) base.args = args;
			const env: Record<string, string> = {};
			for (const line of draft.envText.split('\n')) {
				const trimmed = line.trim();
				if (!trimmed || trimmed.startsWith('#')) continue;
				const eq = trimmed.indexOf('=');
				if (eq <= 0) continue;
				env[trimmed.slice(0, eq).trim()] = trimmed.slice(eq + 1).trim();
			}
			if (Object.keys(env).length > 0) base.env = env;
		}
		return base;
	}

	async function saveDraft() {
		if (saving) return;
		if (!native && settingsStore.vaultLocked) {
			formError = 'Unlock the vault below before editing servers.';
			return;
		}
		const config = draftToConfig();
		if (!native) {
			const error =
				editingId === '__new__'
					? settingsStore.addMcpServer(config)
					: settingsStore.updateMcpServer(editingId ?? '', config);
			if (error) {
				formError = error;
				return;
			}
			editingId = null;
			formError = null;
			return;
		}
		// Native: validate the merged list, write it to the host, then store
		// a touched token separately (tokens never enter settings JSON).
		if (editingId === '__new__' && nativeServers.some((s) => s.id === config.id)) {
			formError = `A server with id '${config.id}' already exists`;
			return;
		}
		const merged = [
			...nativeServers.filter((s) => s.id !== (editingId === '__new__' ? config.id : editingId)),
			config
		];
		const parsed = parseMcpServerConfigs(merged);
		if (parsed.servers.length !== merged.length) {
			formError = parsed.dropped[0] ?? 'Invalid server config';
			return;
		}
		saving = true;
		try {
			await setNativeMcpServers(invoke, parsed.servers);
			const id = String(config.id);
			if (draft.transport === 'http' && tokenTouched) {
				await setNativeMcpServerToken(invoke, id, draft.bearerToken);
			}
			nativeServers = parsed.servers;
			editingId = null;
			formError = null;
			void refreshNative();
		} catch (e) {
			formError = e instanceof Error ? e.message : 'Could not save server';
		} finally {
			saving = false;
		}
	}

	async function toggleServerEnabled(server: McpServerConfig) {
		if (saving) return;
		if (!native) {
			settingsStore.setMcpServerEnabled(server.id, !server.enabled);
			return;
		}
		saving = true;
		try {
			const next = nativeServers.map((s) => (s.id === server.id ? { ...s, enabled: !s.enabled } : s));
			await setNativeMcpServers(invoke, next);
			nativeServers = next;
			void refreshNative();
		} catch (e) {
			nativeError = e instanceof Error ? e.message : 'Could not update server';
		} finally {
			saving = false;
		}
	}

	async function removeServer(id: string) {
		if (!window.confirm(`Remove MCP server '${id}'?`)) return;
		if (testResult?.id === id) testResult = null;
		if (!native) {
			settingsStore.removeMcpServer(id);
			return;
		}
		saving = true;
		try {
			await setNativeMcpServers(
				invoke,
				nativeServers.filter((s) => s.id !== id)
			);
			// Drop any stored token with the server (best-effort).
			await setNativeMcpServerToken(invoke, id, '').catch(() => false);
			nativeServers = nativeServers.filter((s) => s.id !== id);
			void refreshNative();
		} catch (e) {
			nativeError = e instanceof Error ? e.message : 'Could not remove server';
		} finally {
			saving = false;
		}
	}

	async function testServer(server: McpServerConfig) {
		if (testingId) return;
		testingId = server.id;
		testResult = null;
		try {
			if (native) {
				const tools = await connectNativeMcpServer(invoke, server.id);
				testResult = {
					id: server.id,
					ok: true,
					message:
						tools.length === 0
							? 'Connected — no tools advertised.'
							: tools.length === 1
								? `Connected — 1 tool: ${tools[0]}.`
								: `Connected — ${tools.length} tools: ${tools.slice(0, 5).join(', ')}${tools.length > 5 ? '…' : ''}.`
				};
				void refreshNative();
				return;
			}
			const res = await fetch('/api/mcp/tools', {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ server })
			});
			const data = (await res.json().catch(() => null)) as {
				tools?: unknown[];
				error?: unknown;
			} | null;
			if (!res.ok || !data || !Array.isArray(data.tools)) {
				const message = typeof data?.error === 'string' ? data.error : `Request failed (${res.status})`;
				testResult = { id: server.id, ok: false, message: message.slice(0, 300) };
			} else {
				testResult = {
					id: server.id,
					ok: true,
					message:
						data.tools.length === 1
							? 'Connected — 1 tool available.'
							: `Connected — ${data.tools.length} tools available.`
				};
			}
		} catch (e) {
			testResult = {
				id: server.id,
				ok: false,
				message: e instanceof Error ? e.message.slice(0, 300) : 'Connection failed'
			};
		} finally {
			testingId = null;
		}
	}
</script>

<div class="service-group">
	<div class="service-header">
		<Icon name="link" size={14} />
		<span>Model Context Protocol</span>
		{#if !native}
			<button
				class="service-toggle"
				class:enabled={mcpEnabled}
				onclick={() => settingsStore.setMcpEnabled(!mcpEnabled)}
				aria-label="Toggle MCP tool use"
			>
				<span class="toggle-track">
					<span class="toggle-thumb"></span>
				</span>
			</button>
		{:else}
			<span class="transport-badge">native host</span>
		{/if}
	</div>

	{#if !mounted}
		<p class="provider-note provider-info">
			<Icon name="info" size={14} />
			Loading MCP settings…
		</p>
	{:else if native}
		<p class="mcp-blurb">
			MCP servers for the native agent, executed by the Rust host (stdio and Streamable HTTP).
			Per-server switches gate execution; tool calls ask for approval unless pre-granted.
		</p>
	{:else}
		<p class="mcp-blurb">
			Lets chat use tools from your own MCP servers (for example Home Assistant). Off by default;
			tools run at most 5 rounds per turn and results are capped.
		</p>
	{/if}

	{#if !native && proxyEnabled === false}
		<p class="provider-note provider-warning">
			<Icon name="alert-circle" size={14} />
			The server proxy is disabled, so web chat cannot reach MCP servers.
		</p>
	{/if}

	{#if native && nativeLoading && nativeServers.length === 0}
		<p class="provider-note provider-info">
			<Icon name="info" size={14} />
			Loading servers from the native host…
		</p>
	{/if}

	{#if native && nativeError}
		<p class="provider-note error">
			<Icon name="alert-circle" size={14} />
			{nativeError}
		</p>
	{/if}

	{#if mcpEnabled && mounted}
		{#if servers.length === 0 && editingId === null}
			<p class="provider-note provider-info">
				<Icon name="info" size={14} />
				No servers configured yet. Add one below to give chat its first tools.
			</p>
		{/if}

		<ul class="server-list">
			{#each servers as server (server.id)}
				{@const status = native ? statusFor(server.id) : undefined}
				<li class="server-row">
					<div class="server-main">
						<span class="server-name">{server.name ?? server.id}</span>
						<span class="transport-badge">{server.transport}</span>
						{#if !server.enabled}
							<span class="disabled-badge">disabled</span>
						{:else if status?.connected}
							<span class="connected-badge">connected · {status.tools} tools</span>
						{/if}
						{#if native && server.transport === 'http' && status?.has_token}
							<span class="transport-badge">token saved</span>
						{/if}
					</div>
					<div class="server-sub">
						{server.transport === 'http' ? server.url : server.command}
					</div>
					{#if native && status?.last_error}
						<p class="test-line fail">{status.last_error}</p>
					{/if}
					{#if testResult?.id === server.id}
						<p class="test-line" class:ok={testResult.ok} class:fail={!testResult.ok}>
							{testResult.message}
						</p>
					{/if}
					<div class="server-actions">
						<button class="mini-btn" onclick={() => void toggleServerEnabled(server)}>
							{server.enabled ? 'Disable' : 'Enable'}
						</button>
						<button
							class="mini-btn"
							onclick={() => void testServer(server)}
							disabled={testingId !== null}
						>
							{testingId === server.id ? 'Testing…' : 'Test'}
						</button>
						<button class="mini-btn" onclick={() => startEdit(server)}>Edit</button>
						<button class="mini-btn danger" onclick={() => void removeServer(server.id)}>
							Remove
						</button>
					</div>
				</li>
			{/each}
		</ul>

		{#if editingId === null}
			<button class="add-btn" onclick={startAdd}>
				<Icon name="plus" size={14} />
				Add server
			</button>
		{:else}
			<div class="server-form">
				<h3>{editingId === '__new__' ? 'Add MCP server' : `Edit '${editingId}'`}</h3>

				<label class="field">
					<span>Transport</span>
					<select bind:value={draft.transport} disabled={editingId !== '__new__'}>
						<option value="http">Streamable HTTP</option>
						{#if native}
							<option value="stdio">Stdio (spawned by the native host)</option>
						{:else}
							<option value="stdio">Stdio (server proxy only)</option>
						{/if}
					</select>
				</label>

				<label class="field">
					<span>Server id</span>
					<input
						class="api-key-input"
						type="text"
						bind:value={draft.id}
						disabled={editingId !== '__new__'}
						placeholder="home"
						autocomplete="off"
						spellcheck={false}
					/>
				</label>

				<label class="field">
					<span>Display name (optional)</span>
					<input
						class="api-key-input"
						type="text"
						bind:value={draft.name}
						placeholder="Home Assistant"
						autocomplete="off"
					/>
				</label>

				{#if draft.transport === 'http'}
					<label class="field">
						<span>Endpoint URL</span>
						<input
							class="api-key-input"
							type="url"
							bind:value={draft.url}
							placeholder="https://ha.example.com/mcp"
							autocomplete="off"
							spellcheck={false}
						/>
					</label>

					<label class="field">
						<span>Bearer [REDACTED] (optional)</span>
						<div class="api-key-row">
							<input
								class="api-key-input"
								type={showToken ? 'text' : 'password'}
								bind:value={draft.bearerToken}
								oninput={() => (tokenTouched = true)}
								placeholder={native ? 'Leave blank to keep the saved token' : 'Long-lived access token'}
								autocomplete="off"
								spellcheck={false}
							/>
							<button class="mini-btn" onclick={() => (showToken = !showToken)}>
								{showToken ? 'Hide' : 'Show'}
							</button>
						</div>
					</label>
					{#if native}
						<p class="field-hint">
							Stored in the OS keychain via the native host, never in settings. Leave blank
							to keep the saved token.
						</p>
						{#if editingId !== '__new__' && editingId && statusFor(editingId)?.has_token}
							<div>
								<button
									class="mini-btn danger"
									onclick={() => {
										draft.bearerToken = '';
										tokenTouched = true;
									}}
								>
									Clear saved token on save
								</button>
							</div>
						{/if}
					{/if}
				{:else}
					<label class="field">
						<span>Command</span>
						<input
							class="api-key-input"
							type="text"
							bind:value={draft.command}
							placeholder="uvx mcp-server-time"
							autocomplete="off"
							spellcheck={false}
						/>
					</label>
					<p class="field-hint">
						{#if native}
							Spawned by the native host with a default-deny environment (only the
							variables below are passed).
						{:else}
							Web stdio servers must be pinned by the server operator in
							<code>MCP_STDIO_SERVERS</code> (matched by server id) — command,
							arguments, and environment from the browser are ignored by the
							proxy.
						{/if}
					</p>

					<label class="field">
						<span>Arguments (one per line)</span>
						<textarea class="api-key-input" rows="2" bind:value={draft.argsText} spellcheck={false}
						></textarea>
					</label>

					<label class="field">
						<span>Environment (KEY=value per line)</span>
						<textarea class="api-key-input" rows="2" bind:value={draft.envText} spellcheck={false}
						></textarea>
					</label>
				{/if}

				{#if formError}
					<p class="provider-note error">
						<Icon name="alert-circle" size={14} />
						{formError}
					</p>
				{/if}

				<div class="form-actions">
					<button class="mini-btn primary" onclick={() => void saveDraft()} disabled={saving}>
						{saving ? 'Saving…' : 'Save server'}
					</button>
					<button class="mini-btn" onclick={cancelEdit}>Cancel</button>
				</div>
			</div>
		{/if}

		{#if !native}
			<p class="provider-note provider-info">
				<Icon name="info" size={14} />
				Tools named in <code>PUBLIC_MCP_CONFIRM_TOOLS</code> are never executed automatically —
				chat reports them instead.
			</p>
		{/if}
	{/if}
</div>

<style>
	.mcp-blurb {
		margin: 0;
		font-size: 0.8125rem;
		color: var(--text-secondary);
	}

	.provider-note {
		display: flex;
		align-items: flex-start;
		gap: 0.5rem;
		margin: 0;
		padding: 0.625rem 0.75rem;
		font-size: 0.8125rem;
		border-radius: var(--radius-md);
		line-height: 1.4;
	}

	.provider-note :global(svg) {
		flex-shrink: 0;
		margin-top: 0.125rem;
	}

	.provider-info {
		background: var(--accent-muted);
		color: var(--text-secondary);
	}

	.provider-warning {
		background: color-mix(in srgb, var(--color-warning, #f59e0b) 12%, transparent);
		color: var(--text-secondary);
	}

	.provider-note.error {
		background: color-mix(in srgb, var(--color-error) 12%, transparent);
		color: var(--color-error);
	}

	.provider-note code {
		font-family: var(--font-mono);
		font-size: 0.75rem;
	}

	.server-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.server-row {
		background: var(--bg-secondary);
		border-radius: var(--radius-md);
		padding: 0.75rem;
		display: flex;
		flex-direction: column;
		gap: 0.375rem;
	}

	.server-main {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.server-name {
		font-weight: 600;
		font-size: 0.875rem;
		color: var(--text-primary);
	}

	.transport-badge {
		font-size: 0.6875rem;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		padding: 0.125rem 0.4375rem;
		border-radius: var(--radius-full);
		background: var(--bg-tertiary);
		color: var(--text-secondary);
	}

	.disabled-badge {
		font-size: 0.6875rem;
		font-weight: 600;
		padding: 0.125rem 0.4375rem;
		border-radius: var(--radius-full);
		background: color-mix(in srgb, var(--color-error) 12%, transparent);
		color: var(--color-error);
	}

	.connected-badge {
		font-size: 0.6875rem;
		font-weight: 600;
		padding: 0.125rem 0.4375rem;
		border-radius: var(--radius-full);
		background: color-mix(in srgb, var(--color-success, #22c55e) 12%, transparent);
		color: var(--color-success, #22c55e);
	}

	.server-sub {
		font-family: var(--font-mono);
		font-size: 0.75rem;
		color: var(--text-tertiary);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.test-line {
		margin: 0;
		font-size: 0.8125rem;
	}

	.test-line.ok {
		color: var(--color-success, #22c55e);
	}

	.test-line.fail {
		color: var(--color-error);
	}

	.server-actions {
		display: flex;
		gap: 0.375rem;
		flex-wrap: wrap;
	}

	.mini-btn {
		padding: 0.375rem 0.75rem;
		font-size: 0.75rem;
		font-weight: 600;
		border-radius: var(--radius-md);
		border: 1px solid transparent;
		background: var(--bg-tertiary);
		color: var(--text-secondary);
		cursor: pointer;
		transition: background 0.15s ease, color 0.15s ease;
	}

	.mini-btn:hover:not(:disabled) {
		background: color-mix(in srgb, var(--bg-tertiary), var(--text-primary) 8%);
		color: var(--text-primary);
	}

	.mini-btn:disabled {
		opacity: 0.5;
		cursor: default;
	}

	.mini-btn.danger {
		color: var(--color-error);
	}

	.mini-btn.primary {
		background: var(--accent);
		color: #fff;
	}

	.add-btn {
		display: inline-flex;
		align-items: center;
		gap: 0.5rem;
		align-self: flex-start;
		padding: 0.5rem 1rem;
		font-size: 0.8125rem;
		font-weight: 600;
		border-radius: var(--radius-md);
		border: 1px dashed var(--border-light, var(--text-tertiary));
		background: transparent;
		color: var(--text-secondary);
		cursor: pointer;
	}

	.add-btn:hover {
		color: var(--accent);
		border-color: var(--accent);
	}

	.server-form {
		background: var(--bg-secondary);
		border-radius: var(--radius-md);
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 0.625rem;
	}

	.server-form h3 {
		margin: 0;
		font-size: 0.9375rem;
		color: var(--text-primary);
	}

	.field {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		font-size: 0.75rem;
		font-weight: 600;
		color: var(--text-secondary);
	}

	.field select {
		padding: 0.5rem 0.75rem;
		background: var(--bg-secondary);
		border: 1px solid transparent;
		border-radius: var(--radius-lg);
		font-size: 0.8rem;
		color: var(--text-primary);
	}

	.field select:focus {
		outline: none;
		border-color: var(--accent);
	}

	.field textarea {
		resize: vertical;
		min-height: 2.5rem;
	}

	.field-hint {
		margin: -0.25rem 0 0;
		font-size: 0.75rem;
		color: var(--text-tertiary);
	}

	.field-hint code {
		font-family: var(--font-mono);
	}

	.form-actions {
		display: flex;
		gap: 0.5rem;
	}
</style>
