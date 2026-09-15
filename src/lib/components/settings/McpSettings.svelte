<script lang="ts">
	import { browser } from '$app/environment';
	import { onMount } from 'svelte';
	import { settingsStore } from '$lib/stores/settings.svelte';
	import { Icon } from '$lib/components/ui';
	import type { McpServerConfig } from '$lib/services/mcp/types';
	import './ai-services-settings.css';

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
	let proxyEnabled = $state<boolean | null>(null);
	let testingId = $state<string | null>(null);
	let testResult = $state<{ id: string; ok: boolean; message: string } | null>(null);

	const servers = $derived(settingsStore.mcpServers);
	const mcpEnabled = $derived(settingsStore.mcpEnabled);

	onMount(() => {
		if (!browser) return;
		void refreshProxyStatus();
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

	function startAdd() {
		editingId = '__new__';
		draft = { ...EMPTY_DRAFT };
		formError = null;
		showToken = false;
	}

	function startEdit(server: McpServerConfig) {
		editingId = server.id;
		draft = {
			id: server.id,
			name: server.name ?? '',
			transport: server.transport,
			url: server.transport === 'http' ? server.url : '',
			bearerToken: server.transport === 'http' ? (server.bearerToken ?? '') : '',
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

	function saveDraft() {
		const config = draftToConfig();
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
	}

	function removeServer(id: string) {
		if (!window.confirm(`Remove MCP server '${id}'?`)) return;
		settingsStore.removeMcpServer(id);
		if (testResult?.id === id) testResult = null;
	}

	async function testServer(server: McpServerConfig) {
		if (testingId) return;
		testingId = server.id;
		testResult = null;
		try {
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
	</div>

	<p class="mcp-blurb">
		Lets chat use tools from your own MCP servers (for example Home Assistant). Off by default;
		tools run at most 5 rounds per turn and results are capped.
	</p>

	{#if proxyEnabled === false}
		<p class="provider-note provider-warning">
			<Icon name="alert-circle" size={14} />
			The server proxy is disabled, so web chat cannot reach MCP servers. Desktop builds can
			still use HTTP servers directly.
		</p>
	{/if}

	{#if mcpEnabled}
		{#if servers.length === 0 && editingId === null}
			<p class="provider-note provider-info">
				<Icon name="info" size={14} />
				No servers configured yet. Add one below to give chat its first tools.
			</p>
		{/if}

		<ul class="server-list">
			{#each servers as server (server.id)}
				<li class="server-row">
					<div class="server-main">
						<span class="server-name">{server.name ?? server.id}</span>
						<span class="transport-badge">{server.transport}</span>
						{#if !server.enabled}
							<span class="disabled-badge">disabled</span>
						{/if}
					</div>
					<div class="server-sub">
						{server.transport === 'http' ? server.url : server.command}
					</div>
					{#if testResult?.id === server.id}
						<p class="test-line" class:ok={testResult.ok} class:fail={!testResult.ok}>
							{testResult.message}
						</p>
					{/if}
					<div class="server-actions">
						<button
							class="mini-btn"
							onclick={() => settingsStore.setMcpServerEnabled(server.id, !server.enabled)}
						>
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
						<button class="mini-btn danger" onclick={() => removeServer(server.id)}>Remove</button>
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
						<option value="stdio">Stdio (server proxy only)</option>
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
								placeholder="Long-lived access token"
								autocomplete="off"
								spellcheck={false}
							/>
							<button class="mini-btn" onclick={() => (showToken = !showToken)}>
								{showToken ? 'Hide' : 'Show'}
							</button>
						</div>
					</label>
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
						Must be on the server's <code>MCP_STDIO_ALLOWED_COMMANDS</code> allowlist, or the
						proxy refuses to run it.
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
					<button class="mini-btn primary" onclick={saveDraft}>Save server</button>
					<button class="mini-btn" onclick={cancelEdit}>Cancel</button>
				</div>
			</div>
		{/if}

		<p class="provider-note provider-info">
			<Icon name="info" size={14} />
			Tools named in <code>PUBLIC_MCP_CONFIRM_TOOLS</code> are never executed automatically — chat
			reports them instead.
		</p>
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
