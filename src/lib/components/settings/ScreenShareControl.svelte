<script lang="ts">
	import { onMount } from 'svelte';
	import {
		attachScreenShareListener,
		screenShareBusy,
		screenShareError,
		screenShareStatus,
		refreshScreenShareStatus,
		resumeScreenShare,
		pauseScreenShare,
		setDesktopControl,
		startScreenShare,
		stopScreenShare
	} from '$lib/services/native/screen-share.svelte';

	let { compact = false } = $props<{ compact?: boolean }>();
	let target = $state('desktop');

	onMount(() => {
		attachScreenShareListener();
		const timer = window.setInterval(() => void refreshScreenShareStatus(), 5000);
		const emergencyRevoke = (event: KeyboardEvent) => {
			if (
				(screenShareStatus()?.sharing ?? false) &&
				(event.ctrlKey || event.metaKey) &&
				event.altKey &&
				event.shiftKey &&
				event.key.toLowerCase() === 'x'
			) {
				event.preventDefault();
				void setDesktopControl(false);
			}
		};
		window.addEventListener('keydown', emergencyRevoke);
		return () => {
			window.clearInterval(timer);
			window.removeEventListener('keydown', emergencyRevoke);
		};
	});

	const status = $derived(screenShareStatus());
	const busy = $derived(screenShareBusy());
	const error = $derived(screenShareError());
	const portalOwnsSelection = $derived(status?.backend === 'desktop.linux-portal');

	async function share() {
		let selected: Record<string, unknown>;
		if (portalOwnsSelection || target === 'desktop') {
			selected = { type: 'desktop' };
		} else if (target.startsWith('display:')) {
			selected = { type: 'display', display_id: target.slice('display:'.length) };
		} else {
			selected = { type: 'window', window_id: target.slice('window:'.length) };
		}
		await startScreenShare(selected);
	}

	function targetLabel(value: unknown) {
		if (value === 'Desktop') return 'entire desktop';
		if (typeof value !== 'object' || value === null) return 'selected target';
		const entry = value as Record<string, unknown>;
		if ('Desktop' in entry) return 'entire desktop';
		if (typeof entry.Display === 'string') return `display ${entry.Display}`;
		if (typeof entry.Window === 'string') return `window ${entry.Window}`;
		return 'selected target';
	}
</script>

{#if compact}
	{#if status?.sharing}
		<div class="share-indicator" role="status" aria-live="polite">
			<span class="dot" aria-hidden="true"></span>
				<span>Sharing {targetLabel(status.target)}</span>
				<span class="control-state">Control {status.control_enabled ? 'enabled' : 'disabled'}</span>
			<span class="shortcut" title="Ctrl or Command + Alt or Option + Shift + X">Emergency revoke: ⌘/Ctrl⌥⇧X</span>
			<button onclick={() => (status.paused ? resumeScreenShare() : pauseScreenShare())} disabled={busy}>
				{status.paused ? 'Resume' : 'Pause'}
			</button>
			<button class="stop" onclick={() => stopScreenShare()} disabled={busy}>Stop</button>
		</div>
	{/if}
{:else}
	<section class="share-panel" aria-labelledby="share-screen-heading">
		<div class="panel-heading">
			<div>
				<h2 id="share-screen-heading">Share Screen</h2>
				<p>Let the native agent observe a selected display or window. Sharing and desktop control are separate permissions.</p>
			</div>
			<span class="status-pill" data-active={status?.sharing === true}>
				{status?.sharing ? 'Sharing' : status?.available ? 'Ready' : 'Unavailable'}
			</span>
		</div>

		{#if status?.available && !status.sharing}
			{#if portalOwnsSelection}
				<p class="portal-copy">The Wayland screen-sharing portal will ask you to choose the exact monitor or application window.</p>
			{:else}
				<label>
					<span>Capture target</span>
						<select bind:value={target}>
							<option value="desktop">Entire desktop</option>
							{#each status.displays as display}
								<option value={`display:${display.id}`}>{display.name} ({display.width}×{display.height})</option>
							{/each}
							{#each status.windows as window}
								<option value={`window:${window.id}`}>
									{window.app ? `${window.app}: ` : ''}{window.title}
								</option>
							{/each}
						</select>
				</label>
			{/if}
			<button class="primary" onclick={share} disabled={busy}>Share Screen</button>
		{:else if status?.sharing}
			<div class="active-copy">
				<strong>Sharing {targetLabel(status.target)}</strong>
				<span>The model receives sampled observations only; native capture is not sent at video rate.</span>
			</div>
			<div class="actions">
				<button onclick={() => (status.paused ? resumeScreenShare() : pauseScreenShare())} disabled={busy}>
					{status.paused ? 'Resume sharing' : 'Pause sharing'}
				</button>
				<button class="stop" onclick={() => stopScreenShare()} disabled={busy}>Stop sharing</button>
			</div>
			<label class="control-row">
				<span>
					<strong>Allow Control</strong>
					<small>Keep this off to allow visibility without pointer or keyboard control.</small>
				</span>
				<input
					type="checkbox"
					checked={status.control_enabled}
					onchange={(event) => setDesktopControl((event.currentTarget as HTMLInputElement).checked)}
				/>
			</label>
		{:else}
			<p class="muted">The native desktop host is not available in this browser session.</p>
		{/if}

		{#if error}<p class="error" role="alert">{error}</p>{/if}
	</section>
{/if}

<style>
	.share-panel {
		max-width: 600px;
		padding: 1.25rem;
		border: 1px solid var(--border-light, #2a2e37);
		border-radius: var(--radius-lg, 0.75rem);
		background: var(--bg-primary, #16181d);
		color: var(--text-primary, #f2f4f7);
	}
	.panel-heading {
		display: flex;
		gap: 1rem;
		align-items: flex-start;
		justify-content: space-between;
	}
	h2 { margin: 0 0 0.35rem; font-size: 1.05rem; }
	p { margin: 0; color: var(--text-secondary, #9aa3b2); font-size: 0.82rem; line-height: 1.45; }
	.status-pill { border-radius: 999px; padding: 0.25rem 0.6rem; font-size: 0.72rem; background: var(--bg-tertiary, #252a33); }
	.status-pill[data-active='true'] { color: #86efac; background: rgba(34, 197, 94, 0.15); }
	label { display: grid; gap: 0.4rem; margin: 1rem 0; font-size: 0.8rem; }
	select { padding: 0.65rem; border-radius: 0.45rem; border: 1px solid var(--border-light, #2a2e37); background: var(--bg-secondary, #1f232b); color: inherit; }
	button { border: 0; border-radius: 0.45rem; padding: 0.6rem 0.8rem; cursor: pointer; color: inherit; background: var(--bg-tertiary, #252a33); }
	button:disabled { opacity: 0.55; cursor: wait; }
	.primary { background: #0ea5e9; color: white; font-weight: 600; }
	.stop { background: #7f1d1d; color: #fee2e2; }
	.active-copy { display: grid; gap: 0.25rem; margin: 1rem 0; }
	.active-copy span, small { color: var(--text-secondary, #9aa3b2); font-size: 0.78rem; }
	.actions { display: flex; gap: 0.5rem; }
	.control-row { display: flex; grid-template-columns: 1fr auto; align-items: center; justify-content: space-between; gap: 1rem; }
	.control-row span { display: grid; gap: 0.2rem; }
	.control-row input { width: 1.1rem; height: 1.1rem; }
	.muted { margin-top: 1rem; }
	.portal-copy { margin-top: 1rem; padding: 0.7rem; border-radius: 0.45rem; background: var(--bg-secondary, #1f232b); }
	.error { margin-top: 0.8rem; color: #fca5a5; }
	.share-indicator { position: fixed; z-index: 1500; top: 0.75rem; left: 50%; transform: translateX(-50%); display: flex; align-items: center; gap: 0.5rem; padding: 0.45rem 0.7rem; border: 1px solid rgba(74, 222, 128, 0.45); border-radius: 999px; background: rgba(12, 30, 22, 0.94); color: #dcfce7; font-size: 0.75rem; box-shadow: 0 0.5rem 1.5rem rgba(0,0,0,0.22); }
	.share-indicator button { padding: 0.3rem 0.55rem; background: rgba(255,255,255,0.1); }
	.share-indicator .stop { background: rgba(127, 29, 29, 0.9); }
	.shortcut { color: #bbf7d0; font-size: 0.68rem; white-space: nowrap; }
	.dot { width: 0.45rem; height: 0.45rem; border-radius: 50%; background: #4ade80; box-shadow: 0 0 0 0.18rem rgba(74,222,128,0.18); }
	.control-state { color: #bbf7d0; }
</style>
