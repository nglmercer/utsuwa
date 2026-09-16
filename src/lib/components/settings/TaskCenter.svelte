<script lang="ts">
	import { onMount } from 'svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import {
		countCenterTasks,
		filterCenterTasks,
		formatTaskTime,
		isTerminalStatus,
		reviewSummary,
		shortId,
		statusLabel,
		stepProgress,
		TASK_CENTER_FILTERS,
		type TaskCenterFilter
	} from '$lib/tasks/center';
	import { hostTasks, isHostTasksAvailable, type HostTask } from '$lib/tasks/host';

	let available = $state(true);
	let tasks = $state<HostTask[]>([]);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let filter = $state<TaskCenterFilter>('all');
	let expanded = $state<Set<string>>(new Set());
	let acting = $state<Set<string>>(new Set());

	const counts = $derived(countCenterTasks(tasks));
	const visible = $derived(filterCenterTasks(tasks, filter));

	onMount(() => {
		available = isHostTasksAvailable();
		if (available) void refresh();
	});

	async function refresh() {
		loading = true;
		error = null;
		try {
			tasks = await hostTasks().list();
		} catch (err) {
			error = err instanceof Error ? err.message : 'Could not load tasks.';
		} finally {
			loading = false;
		}
	}

	function toggle(id: string) {
		const next = new Set(expanded);
		if (next.has(id)) next.delete(id);
		else next.add(id);
		expanded = next;
	}

	function dotClass(status: HostTask['status']): string {
		if (status === 'completed') return 'ok';
		if (status === 'failed' || status === 'cancelled') return 'bad';
		if (status === 'needs_review') return 'review';
		return 'pending';
	}

	async function act(id: string, action: (client: ReturnType<typeof hostTasks>) => Promise<unknown>) {
		const next = new Set(acting);
		next.add(id);
		acting = next;
		error = null;
		try {
			await action(hostTasks());
			await refresh();
		} catch (err) {
			error = err instanceof Error ? err.message : 'Action failed.';
		} finally {
			const done = new Set(acting);
			done.delete(id);
			acting = done;
		}
	}

	function cancel(id: string) {
		void act(id, (client) => client.cancel(id, 'cancelled from Task Center'));
	}

	function review(id: string, approved: boolean) {
		void act(id, (client) =>
			client.review(id, approved, approved ? 'approved from Task Center' : 'rejected from Task Center')
		);
	}
</script>

<div class="panel">
	<div class="panel-head">
		<div>
			<h2>Task Center</h2>
			<p class="sub">Durable background tasks: schedules, reviews, and results.</p>
		</div>
		<Button variant="secondary" size="sm" onclick={refresh} disabled={loading || !available}>
			{loading ? 'Refreshing…' : 'Refresh'}
		</Button>
	</div>

	{#if !available}
		<p class="empty">Tasks live in the desktop app. Open Utsuwa natively to manage them.</p>
	{:else}
		<div class="filters" role="tablist" aria-label="Filter tasks">
			{#each TASK_CENTER_FILTERS as item (item.id)}
				<button
					class="filter"
					class:active={filter === item.id}
					role="tab"
					aria-selected={filter === item.id}
					onclick={() => (filter = item.id)}
				>
					{item.label}
					<span class="count">{counts[item.id]}</span>
				</button>
			{/each}
		</div>

		{#if error}
			<p class="error" role="alert">{error}</p>
		{/if}

		{#if visible.length === 0}
			<p class="empty">
				{filter === 'all'
					? 'No tasks yet. Ask the assistant to schedule something.'
					: `No ${TASK_CENTER_FILTERS.find((item) => item.id === filter)?.label.toLowerCase()} tasks.`}
			</p>
		{:else}
			<ol class="trail">
				{#each visible as task (task.id)}
					{@const progress = stepProgress(task)}
					{@const summary = task.status === 'needs_review' ? reviewSummary(task.last_error?.message) : null}
					<li class="entry">
						<button class="row" onclick={() => toggle(task.id)} aria-expanded={expanded.has(task.id)}>
							<span class="dot {dotClass(task.status)}"></span>
							<span class="headline">{task.title}</span>
							<span class="meta">
								{progress.done}/{progress.total} steps · {statusLabel(task.status)}
							</span>
						</button>
						{#if expanded.has(task.id)}
							<div class="detail">
								<div class="detail-row"><span>Status</span><span>{statusLabel(task.status)}</span></div>
								<div class="detail-row"><span>Instruction</span><span>{task.instruction}</span></div>
								<div class="detail-row">
									<span>Attempts</span><span>{task.attempts} / {task.max_attempts}</span>
								</div>
								<div class="detail-row">
									<span>Created</span><span>{formatTaskTime(task.created_at)}</span>
								</div>
								{#if task.scheduled_at !== undefined}
									<div class="detail-row">
										<span>Runs after</span><span>{formatTaskTime(task.scheduled_at)}</span>
									</div>
								{/if}
								{#if summary}
									<div class="detail-row review"><span>Review</span><span>{summary}</span></div>
								{/if}
								{#if task.last_error && task.status !== 'needs_review'}
									<div class="detail-row error-row">
										<span>Last error</span><span>{task.last_error.message}</span>
									</div>
								{/if}
								{#if task.steps.length > 0}
									<ol class="steps">
										{#each task.steps as step, i (step.id)}
											<li>
												<span class="step-index">{i + 1}.</span>
												<code>{step.step_type}</code>
												<span class="step-status">{step.status}</span>
												{#if step.error}<span class="step-error">{step.error}</span>{/if}
											</li>
										{/each}
									</ol>
								{/if}
								<div class="actions">
									<span class="id">id {shortId(task.id)}</span>
									{#if task.status === 'needs_review'}
										<Button
											variant="primary"
											size="sm"
											onclick={() => review(task.id, true)}
											disabled={acting.has(task.id)}
										>
											Approve
										</Button>
										<Button
											variant="secondary"
											size="sm"
											onclick={() => review(task.id, false)}
											disabled={acting.has(task.id)}
										>
											Reject
										</Button>
									{:else if !isTerminalStatus(task.status)}
										<Button
											variant="danger"
											size="sm"
											onclick={() => cancel(task.id)}
											disabled={acting.has(task.id)}
										>
											Cancel
										</Button>
									{/if}
								</div>
							</div>
						{/if}
					</li>
				{/each}
			</ol>
		{/if}
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
	.error {
		font-size: 0.85rem;
		color: #d45a5a;
		margin: 0;
	}
	.filters {
		display: flex;
		flex-wrap: wrap;
		gap: 0.375rem;
	}
	.filter {
		display: inline-flex;
		align-items: center;
		gap: 0.375rem;
		padding: 0.375rem 0.625rem;
		font: inherit;
		font-size: 0.8rem;
		color: inherit;
		background: none;
		border: 1px solid var(--border, #2a2a2e);
		border-radius: 999px;
		cursor: pointer;
	}
	.filter.active {
		border-color: currentColor;
		font-weight: 600;
	}
	.count {
		font-size: 0.75rem;
		opacity: 0.65;
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
	.dot.review {
		background: #d9a441;
		opacity: 1;
	}
	.headline {
		flex: 1;
		font-size: 0.875rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.meta {
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
		width: 5rem;
		opacity: 0.6;
	}
	.review > span:last-child {
		color: #d9a441;
	}
	.error-row > span:last-child {
		color: #d45a5a;
	}
	.steps {
		list-style: none;
		margin: 0.25rem 0 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}
	.steps li {
		display: flex;
		align-items: baseline;
		gap: 0.5rem;
		font-size: 0.75rem;
	}
	.steps code {
		font-family: ui-monospace, monospace;
	}
	.step-index {
		opacity: 0.5;
	}
	.step-status {
		opacity: 0.7;
	}
	.step-error {
		color: #d45a5a;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin-top: 0.25rem;
	}
	.id {
		font-size: 0.75rem;
		opacity: 0.5;
		margin-right: auto;
		font-family: ui-monospace, monospace;
	}
</style>
