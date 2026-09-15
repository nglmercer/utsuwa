<script lang="ts">
	import { nativeBoot, type BootSnapshot } from '$lib/services/native/readiness';
	import { browser } from '$app/environment';

	interface Props {
		snapshot: BootSnapshot;
	}

	let { snapshot }: Props = $props();

	let retrying = $state(false);

	async function handleRetry() {
		if (!browser || retrying) return;
		retrying = true;
		try {
			await nativeBoot.retry();
		} catch {
			// The snapshot already carries the new fatal detail.
		} finally {
			retrying = false;
		}
	}

	function handleContinue() {
		nativeBoot.dismissToDegraded();
	}
</script>

<!-- Fatal native-boot overlay: only rendered for desktop builds whose
     handshake failed. The app behind stays mounted (degraded), so power
     users can continue limited or retry without losing state. -->
<div
	class="boot-overlay"
	role="alertdialog"
	aria-modal="true"
	aria-labelledby="boot-title"
	aria-describedby="boot-detail"
>
	<div class="boot-card">
		<p id="boot-title" class="boot-title">Native host unreachable</p>
		<p class="boot-sub">
			Utsuwa is running as a desktop app but the native handshake failed. Native tools,
			voice capture, and on-device services are unavailable until this is resolved.
		</p>
		<p id="boot-detail" class="boot-detail">{snapshot.detail || 'Unknown handshake error.'}</p>
		<div class="boot-actions">
			<button class="boot-btn primary" onclick={handleRetry} disabled={retrying}>
				{retrying ? 'Retrying…' : 'Retry'}
			</button>
			<button class="boot-btn" onclick={handleContinue}>Continue limited</button>
		</div>
		<p class="boot-hint">
		 If this persists, run the host with <code>--debug</code> and look for
			<code>host.frontend.ready</code> in the log.
		</p>
	</div>
</div>

<style>
	.boot-overlay {
		position: fixed;
		inset: 0;
		background: rgba(28, 43, 51, 0.35);
		backdrop-filter: blur(8px);
		-webkit-backdrop-filter: blur(8px);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 1200;
		padding: 1.5rem;
	}

	.boot-card {
		background: var(--bg-primary);
		border-radius: var(--radius-xl);
		max-width: 420px;
		width: 100%;
		padding: 1.5rem;
		box-shadow: var(--shadow-xl);
	}

	.boot-title {
		font-size: 1.1rem;
		font-weight: 700;
		color: var(--text-primary);
		margin: 0 0 0.5rem;
	}

	.boot-sub {
		font-size: 0.85rem;
		line-height: 1.5;
		color: var(--text-secondary);
		margin: 0 0 0.75rem;
	}

	.boot-detail {
		font-size: 0.8rem;
		line-height: 1.5;
		color: var(--text-primary);
		background: var(--bg-secondary);
		border-radius: var(--radius-md);
		padding: 0.6rem 0.75rem;
		margin: 0 0 1rem;
		overflow-wrap: anywhere;
	}

	.boot-actions {
		display: flex;
		gap: 0.6rem;
		margin-bottom: 0.9rem;
	}

	.boot-btn {
		flex: 1;
		font-size: 0.85rem;
		font-weight: 600;
		padding: 0.55rem 0.75rem;
		border-radius: var(--radius-md);
		border: 1px solid var(--border-primary);
		background: var(--bg-secondary);
		color: var(--text-primary);
		cursor: pointer;
	}

	.boot-btn.primary {
		background: var(--accent-primary);
		border-color: transparent;
		color: #fff;
	}

	.boot-btn:disabled {
		opacity: 0.6;
		cursor: wait;
	}

	.boot-hint {
		font-size: 0.75rem;
		line-height: 1.5;
		color: var(--text-tertiary);
		margin: 0;
	}

	.boot-hint code {
		font-family: ui-monospace, monospace;
		font-size: 0.72rem;
	}
</style>
