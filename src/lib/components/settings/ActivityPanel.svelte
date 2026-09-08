<script lang="ts">
	import { onMount } from 'svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import {
		activityRecords,
		attachActivityListener,
		refreshActivity
	} from '$lib/services/native/activity.svelte';
	import {
		formatDuration,
		headlineFor,
		shortHash,
		type ActivityRecord
	} from '$lib/services/native/activity';

	let refreshing = $state(false);
	let expanded = $state<Set<number>>(new Set());

	onMount(() => {
		attachActivityListener();
	});

	async function refresh() {
		refreshing = true;
		try {
			await refreshActivity(100);
		} finally {
			refreshing = false;
		}
	}

	function toggle(index: number) {
		const next = new Set(expanded);
		if (next.has(index)) next.delete(index);
		else next.add(index);
		expanded = next;
	}

	function outcomeClass(outcome: string): string {
		if (outcome === 'Executed' || outcome === 'Approved') return 'ok';
		if (outcome === 'Denied' || outcome === 'Failed' || outcome === 'ApprovalDenied')
			return 'bad';
		return 'pending';
	}
</script>

<div class="panel">
	<div class="panel-head">
		<div>
			<h2>Activity</h2>
			<p class="sub">What the assistant did, newest first. Details are redacted at record time.</p>
		</div>
		<Button variant="secondary" size="sm" onclick={refresh} disabled={refreshing}>
			{refreshing ? 'Refreshing…' : 'Refresh'}
		</Button>
	</div>

	{#if activityRecords().length === 0}
		<p class="empty">No activity yet. Tool calls appear here after the first turn.</p>
	{:else}
		<ol class="trail">
			{#each activityRecords() as record, i (record.timestamp_ms + '-' + i)}
				<li class="entry">
					<button class="row" onclick={() => toggle(i)} aria-expanded={expanded.has(i)}>
						<span class="dot {outcomeClass(record.outcome)}"></span>
						<span class="headline">{headlineFor(record)}</span>
						{#if record.duration_ms !== null}
							<span class="duration">{formatDuration(record.duration_ms)}</span>
						{/if}
					</button>
					{#if expanded.has(i)}
						<div class="detail">
							{#if record.resource}
								<div class="detail-row"><span>Resource</span><code>{record.resource}</code></div>
							{/if}
							{#if record.detail}
								<div class="detail-row"><span>Detail</span><span>{record.detail}</span></div>
							{/if}
							{#if record.mutation}
								<div class="detail-row">
									<span>Changed</span><code>{record.mutation.path}</code>
								</div>
								<div class="detail-row">
									<span>Before</span><code>{shortHash(record.mutation.before_sha256) ?? 'created'}</code>
								</div>
								<div class="detail-row">
									<span>After</span><code>{shortHash(record.mutation.after_sha256) ?? 'removed'}</code>
								</div>
							{/if}
						</div>
					{/if}
				</li>
			{/each}
		</ol>
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
	.empty {
		font-size: 0.9rem;
		opacity: 0.7;
	}
	.trail {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.375rem;
	}
	.entry {
		border: 1px solid var(--border, #2a2a2e);
		border-radius: 8px;
		overflow: hidden;
	}
	.row {
		width: 100%;
		display: flex;
		align-items: center;
		gap: 0.625rem;
		padding: 0.625rem 0.75rem;
		background: none;
		border: none;
		color: inherit;
		font: inherit;
		cursor: pointer;
		text-align: left;
	}
	.dot {
		flex: none;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: currentColor;
		opacity: 0.5;
	}
	.dot.ok {
		background: #4caf7d;
		opacity: 1;
	}
	.dot.bad {
		background: #d45a5a;
		opacity: 1;
	}
	.headline {
		flex: 1;
		font-size: 0.875rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.duration {
		flex: none;
		font-size: 0.75rem;
		opacity: 0.6;
	}
	.detail {
		padding: 0.25rem 0.75rem 0.75rem 1.625rem;
		display: flex;
		flex-direction: column;
		gap: 0.375rem;
		font-size: 0.8rem;
	}
	.detail-row {
		display: flex;
		gap: 0.5rem;
		align-items: baseline;
	}
	.detail-row > span:first-child {
		flex: none;
		width: 4.5rem;
		opacity: 0.6;
	}
	.detail-row code {
		font-family: ui-monospace, monospace;
		font-size: 0.75rem;
		word-break: break-all;
	}
</style>
