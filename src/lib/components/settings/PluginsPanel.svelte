<script lang="ts">
	import { onMount } from 'svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import {
		getPluginError,
		getPlugins,
		refreshPlugins,
		runPluginOp
	} from '$lib/services/native/plugins.svelte';
	import { isServing, type PluginInfo, type PluginOp } from '$lib/services/native/plugins';

	let busy = $state<string | null>(null);
	let notice = $state<string | null>(null);

	onMount(() => {
		void refreshPlugins();
	});

	async function op(id: string, operation: PluginOp, confirmText?: string) {
		if (confirmText && !window.confirm(confirmText)) return;
		busy = `${operation}:${id}`;
		notice = null;
		try {
			const error = await runPluginOp(id, operation);
			notice = error;
		} finally {
			busy = null;
		}
	}

	function stateClass(state: string): string {
		if (state === 'enabled') return 'ok';
		if (state === 'failed') return 'bad';
		return 'idle';
	}

	function trustNote(plugin: PluginInfo): string {
		switch (plugin.trust) {
			case 'official':
				return 'Ships with the app.';
			case 'verified':
				return 'Signed by a verified author.';
			case 'trusted-native':
				return 'Native code the user explicitly trusts.';
			case 'local-dev':
				return 'Built locally for development.';
			default:
				return 'Unsigned guest code: sandboxed, no ambient authority.';
		}
	}
</script>

<div class="panel">
	<div class="panel-head">
		<div>
			<h2>Plugins</h2>
			<p class="sub">
				WASM plugins extend the assistant with new tools. Enabling loads guest code —
				every call still needs a policy approval and runs sandboxed.
			</p>
		</div>
		<Button variant="secondary" size="sm" onclick={() => void refreshPlugins()} disabled={busy !== null}>
			Refresh
		</Button>
	</div>

	{#if getPluginError()}
		<p class="error" role="alert">{getPluginError()}</p>
	{:else if getPlugins().length === 0}
		<p class="empty">
			No plugins installed. Drop a plugin folder (plugin.toml + plugin.wasm) into the
			configured plugin directory to get started.
		</p>
	{:else}
		<ul class="list">
			{#each getPlugins() as plugin (plugin.id)}
				<li class="card">
					<div class="card-head">
						<div>
							<strong>{plugin.name}</strong>
							<span class="version">v{plugin.version}</span>
						</div>
						<span class="badge {stateClass(plugin.state)}">{plugin.state}</span>
					</div>
					<p class="meta">{plugin.id} · trust: {plugin.trust}</p>
					<p class="trust">{trustNote(plugin)}</p>
					{#if plugin.tools.length > 0}
						<p class="tools">Tools: <code>{plugin.tools.join(', ')}</code></p>
					{/if}
					<div class="actions">
						{#if isServing(plugin)}
							<Button
								variant="secondary"
								size="sm"
								onclick={() => void op(plugin.id, 'disable')}
								disabled={busy !== null}
							>
								Disable
							</Button>
						{:else}
							<Button
								variant="primary"
								size="sm"
								onclick={() => void op(plugin.id, 'enable')}
								disabled={busy !== null}
							>
								Enable
							</Button>
						{/if}
						<Button
							variant="ghost"
							size="sm"
							onclick={() => void op(plugin.id, 'update')}
							disabled={busy !== null}
						>
							Update
						</Button>
						<Button
							variant="danger"
							size="sm"
							onclick={() =>
								void op(plugin.id, 'remove', `Remove plugin "${plugin.name}"? This unregisters it; files stay on disk.`)}
							disabled={busy !== null}
						>
							Remove
						</Button>
						{#if busy === `update:${plugin.id}` || busy === `enable:${plugin.id}` || busy === `disable:${plugin.id}` || busy === `remove:${plugin.id}`}
							<span class="busy">Working…</span>
						{/if}
					</div>
				</li>
			{/each}
		</ul>
	{/if}

	{#if notice}
		<p class="error" role="alert">{notice}</p>
	{/if}
</div>

<style>
	.panel {
		max-width: 640px;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.panel-head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 1rem;
	}
	.panel-head h2 {
		margin: 0;
		font-size: 1.25rem;
	}
	.sub {
		margin: 0.25rem 0 0;
		font-size: 0.85rem;
		opacity: 0.7;
	}
	.empty,
	.error {
		font-size: 0.9rem;
	}
	.error {
		color: #d45a5a;
	}
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}
	.card {
		border: 1px solid var(--border, #2a2a2e);
		border-radius: 8px;
		padding: 0.875rem 1rem;
	}
	.card-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.5rem;
	}
	.version {
		margin-left: 0.5rem;
		font-size: 0.75rem;
		opacity: 0.6;
	}
	.badge {
		flex: none;
		font-size: 0.7rem;
		padding: 0.15rem 0.5rem;
		border-radius: 999px;
		border: 1px solid currentColor;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.badge.ok {
		color: #4caf7d;
	}
	.badge.bad {
		color: #d45a5a;
	}
	.badge.idle {
		opacity: 0.6;
	}
	.meta,
	.trust,
	.tools {
		margin: 0.375rem 0 0;
		font-size: 0.8rem;
		opacity: 0.75;
	}
	.tools code {
		font-family: ui-monospace, monospace;
		font-size: 0.75rem;
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin-top: 0.75rem;
	}
	.busy {
		font-size: 0.8rem;
		opacity: 0.6;
	}
</style>
