<script lang="ts">
	import { browser } from '$app/environment';
	import { onMount } from 'svelte';
	import { getBridge } from '$lib/services/native/bridge';
	import {
		getAutonomousFullAccess,
		grantHomeReadAccess,
		homeReadGranted,
		listGrants,
		revokeHomeReadAccess,
		setAutonomousFullAccess
	} from '$lib/services/native/permissions';

	let enabled = $state(false);
	let autonomousFullAccess = $state(false);
	let home = $state<string | null>(null);
	let busy = $state(false);
	let autonomousBusy = $state(false);
	let nativeAvailable = $state<boolean | null>(null);
	let notice = $state<string | null>(null);
	let autonomousNotice = $state<string | null>(null);
	let error = $state<string | null>(null);

	async function refresh() {
		if (!browser) return;
		const bridge = getBridge();
		if (!bridge) {
			nativeAvailable = false;
			error = null;
			return;
		}
		nativeAvailable = true;
		try {
			const invoke = (m: string, p?: Record<string, unknown>) => bridge.invoke(m, p);
			const [snapshot, fullAccess] = await Promise.all([
				listGrants(invoke),
				getAutonomousFullAccess(invoke)
			]);
			enabled = homeReadGranted(snapshot);
			home = snapshot.home;
			autonomousFullAccess = fullAccess;
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : 'native access settings failed';
		}
	}

	onMount(() => {
		void refresh();
	});

	async function toggle() {
		if (!browser || busy) return;
		const bridge = getBridge();
		if (!bridge) {
			error = 'native host is not attached';
			return;
		}
		if (!enabled) {
			const ok = window.confirm(
				'Let the assistant read any file under your home folder?\n\nKey directories (.ssh, .aws, …) always need per-file approval and are never included silently.'
			);
			if (!ok) return;
		}
		busy = true;
		notice = null;
		try {
			if (enabled) {
				const removed = await revokeHomeReadAccess((m, p) => bridge.invoke(m, p));
				notice = removed > 0 ? 'Home folder access revoked.' : 'No grant was active.';
			} else {
				const path = await grantHomeReadAccess((m, p) => bridge.invoke(m, p));
				notice = `The assistant can now read files under ${path}.`;
			}
			await refresh();
		} catch (e) {
			error = e instanceof Error ? e.message : 'update failed';
		} finally {
			busy = false;
		}
	}

	async function toggleAutonomousFullAccess() {
		if (!browser || autonomousBusy || nativeAvailable !== true) return;
		const bridge = getBridge();
		if (!bridge) {
			nativeAvailable = false;
			error = null;
			return;
		}
		const nextState = !autonomousFullAccess;
		if (!autonomousFullAccess) {
			const ok = window.confirm(
				'Enable Autonomous Full Access?\n\nThe assistant will be able to read and modify files, execute programs, control supported desktop applications, and use other native tools without asking for permission each time.\n\nActions will run with the permissions of your current operating-system user.'
			);
			if (!ok) return;
		}
		autonomousBusy = true;
		autonomousNotice = null;
		error = null;
		try {
			await setAutonomousFullAccess((m, p) => bridge.invoke(m, p), nextState);
			await refresh();
			autonomousNotice = nextState
				? 'Autonomous Full Access is on for the AI agent.'
				: 'Autonomous Full Access is off. Normal Utsuwa permission policy restored.';
		} catch (e) {
			error = e instanceof Error ? e.message : 'could not update Autonomous Full Access';
		} finally {
			autonomousBusy = false;
		}
	}
</script>

<div class="panel">
	<div class="panel-head">
		<div>
			<h2>File access</h2>
			<p class="sub">
				Let the assistant read any file under your home folder without asking every time.
				For broader native tools, use Autonomous Full Access below.
			</p>
		</div>
	</div>

	{#if error}
		<p class="error" role="alert">{error}</p>
	{/if}

	<div class="card">
		<div class="card-head">
			<div>
				<p class="title">Read home folder</p>
				<p class="meta">{home ?? 'resolving…'} · persistent grant</p>
			</div>
			<button
				class="service-toggle"
				class:enabled
				onclick={toggle}
				disabled={busy}
				aria-label="Toggle home folder read access"
				aria-pressed={enabled}
			>
				<span class="toggle-track">
					<span class="toggle-thumb"></span>
				</span>
			</button>
		</div>
		<p class="note">
			Key directories (<code>.ssh</code>, <code>.aws</code>, <code>.gnupg</code>, …) are excluded from
			this broad read grant. In normal mode, the assistant asks for those files individually;
			Autonomous Full Access can authorize the Agent's exact requests automatically. Every
			granted read is still audit-logged under Activity.
		</p>
		{#if notice}
			<p class="notice">{notice}</p>
		{/if}
		{#if busy}
			<p class="busy">Working…</p>
		{/if}
	</div>

	<div class="card autonomous-card">
		<div class="card-head">
			<div>
				<p class="title">Autonomous Full Access</p>
				<p class="meta">Native desktop only · persistent</p>
			</div>
			<button
				class="service-toggle"
				class:enabled={autonomousFullAccess}
				onclick={toggleAutonomousFullAccess}
				disabled={autonomousBusy || nativeAvailable !== true}
				aria-label="Toggle Autonomous Full Access"
				aria-pressed={autonomousFullAccess}
			>
				<span class="toggle-track">
					<span class="toggle-thumb"></span>
				</span>
			</button>
		</div>
		<p class="note">
			Allow the assistant to use filesystem, process, desktop, and other native tools without
			individual permission prompts.
		</p>
		<p class="details">
			<strong>Applies to the AI agent</strong><br />
			Runs with the permissions of your operating-system user.
		</p>
		{#if autonomousFullAccess}
			<p class="state-on">Agent tool requests are automatically authorized.</p>
			<p class="warning" role="alert">
				⚠ The assistant can modify files and execute programs using your operating-system account.
			</p>
		{:else}
			<p class="state-off">Normal Utsuwa permission policy.</p>
		{/if}
		{#if nativeAvailable === false}
			<p class="unavailable">Autonomous Full Access is available only in the native desktop app.</p>
		{/if}
		{#if autonomousNotice}
			<p class="notice">{autonomousNotice}</p>
		{/if}
		{#if autonomousBusy}
			<p class="busy">Working…</p>
		{/if}
	</div>
</div>

<style>
	.panel {
		max-width: 640px;
		display: flex;
		flex-direction: column;
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
	.error {
		font-size: 0.9rem;
		color: #d45a5a;
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
		gap: 1rem;
	}
	.title {
		margin: 0;
		font-weight: 600;
	}
	.meta {
		margin: 0.15rem 0 0;
		font-size: 0.8rem;
		opacity: 0.65;
	}
	.note {
		margin: 0.75rem 0 0;
		font-size: 0.82rem;
		opacity: 0.75;
	}
	.autonomous-card {
		border-color: color-mix(in srgb, var(--accent) 35%, var(--border, #2a2a2e));
	}
	.details,
	.state-on,
	.state-off,
	.warning,
	.unavailable {
		margin: 0.65rem 0 0;
		font-size: 0.82rem;
	}
	.details {
		opacity: 0.75;
	}
	.state-on {
		color: var(--accent);
	}
	.state-off {
		opacity: 0.7;
	}
	.warning {
		color: #d49a45;
	}
	.unavailable {
		opacity: 0.7;
	}
	.note code {
		font-size: 0.78rem;
	}
	.notice,
	.busy {
		margin: 0.6rem 0 0;
		font-size: 0.85rem;
	}
	.busy {
		opacity: 0.6;
	}

	/* Same switch as the LLM / TTS / STT pages */
	.service-toggle {
		position: relative;
		width: 40px;
		height: 22px;
		background: transparent;
		border: none;
		padding: 0;
		cursor: pointer;
		flex-shrink: 0;
	}
	.service-toggle:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.toggle-track {
		display: block;
		width: 100%;
		height: 100%;
		background: var(--bg-tertiary);
		border-radius: var(--radius-full);
		transition: background 0.2s ease;
	}
	.service-toggle.enabled .toggle-track {
		background: var(--accent);
	}
	.toggle-thumb {
		position: absolute;
		top: 2px;
		left: 2px;
		width: 18px;
		height: 18px;
		background: #fff;
		border-radius: var(--radius-full);
		transition: transform 0.2s ease;
		box-shadow: var(--shadow-xs);
	}
	.service-toggle.enabled .toggle-thumb {
		transform: translateX(18px);
	}
	@media (prefers-reduced-motion: reduce) {
		.toggle-track,
		.toggle-thumb {
			transition: none;
		}
	}
</style>
