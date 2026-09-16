// Pure Task Center helpers: filtering, progress, and review summaries
// for the host task list. Framework-free so it runs under `node --test`;
/// the reactive panel lives in `components/settings/TaskCenter.svelte`.
import {
	parseCapabilityReview,
	type HostTask,
	type HostTaskStatus
} from './host.ts';

export type TaskCenterFilter = 'all' | 'active' | 'needs_review' | 'scheduled' | 'done';

export const TASK_CENTER_FILTERS: { id: TaskCenterFilter; label: string }[] = [
	{ id: 'all', label: 'All' },
	{ id: 'active', label: 'Active' },
	{ id: 'needs_review', label: 'Needs review' },
	{ id: 'scheduled', label: 'Scheduled' },
	{ id: 'done', label: 'Done' }
];

const ACTIVE_STATUSES: HostTaskStatus[] = ['pending', 'ready', 'running', 'waiting'];
const DONE_STATUSES: HostTaskStatus[] = ['completed', 'failed', 'cancelled'];

export function filterCenterTasks(tasks: HostTask[], filter: TaskCenterFilter): HostTask[] {
	switch (filter) {
		case 'all':
			return [...tasks];
		case 'active':
			return tasks.filter((task) => ACTIVE_STATUSES.includes(task.status));
		case 'needs_review':
			return tasks.filter((task) => task.status === 'needs_review');
		case 'scheduled':
			return tasks.filter((task) => task.status === 'scheduled');
		case 'done':
			return tasks.filter((task) => DONE_STATUSES.includes(task.status));
	}
}

export function countCenterTasks(tasks: HostTask[]): Record<TaskCenterFilter, number> {
	return {
		all: tasks.length,
		active: tasks.filter((task) => ACTIVE_STATUSES.includes(task.status)).length,
		needs_review: tasks.filter((task) => task.status === 'needs_review').length,
		scheduled: tasks.filter((task) => task.status === 'scheduled').length,
		done: tasks.filter((task) => DONE_STATUSES.includes(task.status)).length
	};
}

export function stepProgress(task: HostTask): { done: number; total: number } {
	const total = task.steps.length;
	const done = task.steps.filter(
		(step) => step.status === 'completed' || step.status === 'skipped'
	).length;
	return { done, total };
}

export function isTerminalStatus(status: HostTaskStatus): boolean {
	return DONE_STATUSES.includes(status);
}

export function statusLabel(status: HostTaskStatus): string {
	if (status === 'needs_review') return 'Needs review';
	return status.charAt(0).toUpperCase() + status.slice(1);
}

/** Human line for a parked review: capability requests name their tool. */
export function reviewSummary(reason: string | undefined): string | null {
	if (!reason) return null;
	const capability = parseCapabilityReview(reason);
	if (capability) return `${capability.tool} needs ${capability.capability}`;
	return reason;
}

export function shortId(id: string): string {
	return id.length > 8 ? id.slice(0, 8) : id;
}

export function formatTaskTime(ms: number | undefined): string {
	if (typeof ms !== 'number' || !Number.isFinite(ms)) return '—';
	return new Date(ms).toLocaleString();
}
