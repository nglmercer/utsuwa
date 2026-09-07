<script lang="ts">
	import { onMount } from 'svelte';
	import {
		attachPermissionListener,
		permissionRequests,
		respondToPermission
	} from '$lib/services/native/permissions.svelte';
	import {
		formatCapability,
		headlineFor,
		riskLevel,
		type LifetimeChoice
	} from '$lib/services/native/permissions';

	let answering = $state<string | null>(null);
	let failed = $state<string | null>(null);

	onMount(() => {
		attachPermissionListener();
	});

	async function answer(id: string, choice: LifetimeChoice) {
		if (answering) return;
		answering = choice;
		failed = null;
		const ok = await respondToPermission(id, choice);
		if (!ok) failed = 'The host did not accept the answer. The request is still queued.';
		answering = null;
	}
</script>

{#if permissionRequests()[0]}
	{@const current = permissionRequests()[0]}
	{@const risk = riskLevel(current.capability)}
	<div class="perm-overlay" role="presentation">
		<div class="perm-dialog" role="alertdialog" aria-modal="true" aria-label="Permission request">
			<div class="perm-risk" data-risk={risk}>
				{risk === 'observe' ? 'Read access' : risk === 'mutate' ? 'Modification' : 'Control'}
			</div>
			<p class="perm-headline">{headlineFor(current)}</p>
			<code class="perm-resource">{current.resource.label}</code>
			<dl class="perm-meta">
				<div>
					<dt>Tool</dt>
					<dd>{formatCapability(current.capability)}</dd>
				</div>
				<div>
					<dt>Requested by</dt>
					<dd>{current.principal}</dd>
				</div>
				{#if current.reason}
					<div>
						<dt>Reason</dt>
						<dd>{current.reason}</dd>
					</div>
				{/if}
			</dl>
			{#if failed}
				<p class="perm-error">{failed}</p>
			{/if}
			<div class="perm-actions">
				<button
					class="perm-btn perm-deny"
					onclick={() => answer(current.id, 'deny')}
					disabled={!!answering}
				>
					Deny
				</button>
				<button
					class="perm-btn perm-allow"
					onclick={() => answer(current.id, 'once')}
					disabled={!!answering}
				>
					Allow once
				</button>
				<button
					class="perm-btn perm-allow"
					onclick={() => answer(current.id, 'task')}
					disabled={!!answering}
				>
					Allow for task
				</button>
				<button
					class="perm-btn perm-allow"
					onclick={() => answer(current.id, 'session')}
					disabled={!!answering}
				>
					Allow for session
				</button>
			</div>
		</div>
	</div>
{/if}

<style>
	/* Blocking prompt: renders above onboarding and every other modal. */
	.perm-overlay {
		position: fixed;
		inset: 0;
		z-index: 2000;
		display: flex;
		align-items: center;
		justify-content: center;
		background: rgba(0, 0, 0, 0.6);
		padding: 1rem;
	}
	.perm-dialog {
		width: min(26rem, 100%);
		background: var(--bg-primary, #16181d);
		color: var(--text-primary, #f2f4f7);
		border: 1px solid var(--border-subtle, #2a2e37);
		border-radius: var(--radius-lg, 0.75rem);
		padding: 1.25rem;
		box-shadow: 0 1.5rem 4rem rgba(0, 0, 0, 0.5);
	}
	.perm-risk {
		display: inline-block;
		font-size: 0.75rem;
		font-weight: 600;
		padding: 0.2rem 0.6rem;
		border-radius: 999px;
		margin-bottom: 0.75rem;
	}
	.perm-risk[data-risk='observe'] {
		background: rgba(56, 189, 248, 0.15);
		color: #7dd3fc;
	}
	.perm-risk[data-risk='mutate'] {
		background: rgba(251, 191, 36, 0.15);
		color: #fcd34d;
	}
	.perm-risk[data-risk='control'] {
		background: rgba(248, 113, 113, 0.15);
		color: #fca5a5;
	}
	.perm-headline {
		margin: 0 0 0.5rem;
		font-size: 0.95rem;
	}
	.perm-resource {
		display: block;
		font-size: 0.85rem;
		word-break: break-all;
		background: var(--bg-secondary, #1f232b);
		border-radius: var(--radius-md, 0.5rem);
		padding: 0.6rem 0.75rem;
		margin-bottom: 0.75rem;
	}
	.perm-meta {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		margin: 0 0 1rem;
		font-size: 0.8rem;
	}
	.perm-meta div {
		display: flex;
		gap: 0.5rem;
	}
	.perm-meta dt {
		color: var(--text-secondary, #9aa3b2);
		min-width: 6.5rem;
	}
	.perm-meta dd {
		margin: 0;
	}
	.perm-error {
		font-size: 0.8rem;
		color: #fca5a5;
		margin: 0 0 0.75rem;
	}
	.perm-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.perm-btn {
		flex: 1 1 auto;
		border: none;
		border-radius: var(--radius-md, 0.5rem);
		padding: 0.65rem 0.75rem;
		font-size: 0.85rem;
		font-weight: 600;
		cursor: pointer;
	}
	.perm-btn:disabled {
		opacity: 0.6;
		cursor: wait;
	}
	.perm-deny {
		background: var(--bg-secondary, #1f232b);
		color: var(--text-primary, #f2f4f7);
	}
	.perm-allow {
		background: #0ea5e9;
		color: #fff;
	}
</style>
