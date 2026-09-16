// One-way migration: Dexie (browser prototype authority) → native host
// (SQLite authority). Runs once at boot when the bridge exists. Migrated
// tasks are re-created on the host and the Dexie originals are cancelled so
// nothing executes twice. Pure mapping + injected store/client: node-safe.
import type { HostTaskClient, NewHostTask } from './host.ts';
import type { DurableTask, TaskStatus } from './types.ts';

export interface MigrationStore {
	list(statuses?: TaskStatus[]): Promise<DurableTask[]>;
	update(id: string, patch: Partial<DurableTask>): Promise<DurableTask | null>;
}

const TERMINAL: ReadonlySet<TaskStatus> = new Set(['completed', 'failed', 'cancelled']);

export function isMigratable(status: TaskStatus): boolean {
	return !TERMINAL.has(status);
}

/** Map one browser task to a host `task.create` body. Attempt counters and
 * lease state do not migrate: the host re-runs from step 0 with fresh
 * attempts (documented: migration replays, it does not resume mid-step). */
export function browserTaskToHostInput(task: DurableTask): NewHostTask {
	const steps = task.steps.map((step) => ({
		step_type: step.type,
		input: (step.input ?? {}) as Record<string, unknown>,
		max_attempts: step.maxAttempts
	}));
	const input: NewHostTask = {
		title: task.title,
		instruction: task.instruction,
		steps
	};
	if (task.scheduledAt !== undefined) input.scheduled_at = task.scheduledAt;
	if (typeof task.priority === 'number') input.priority = task.priority;
	if (typeof task.maxAttempts === 'number') input.max_attempts = task.maxAttempts;
	if (task.verification) input.verification = task.verification;
	if (task.parentTaskId) input.parent_task_id = task.parentTaskId;
	return input;
}

export interface MigrationReport {
	migrated: string[];
	skippedTerminal: string[];
	failed: Array<{ id: string; error: string }>;
}

export async function migrateDexieTasksToHost(deps: {
	store: MigrationStore;
	client: HostTaskClient;
	log?: (message: string) => void;
}): Promise<MigrationReport> {
	const report: MigrationReport = { migrated: [], skippedTerminal: [], failed: [] };
	let tasks: DurableTask[];
	try {
		tasks = await deps.store.list();
	} catch (err) {
		deps.log?.(`task migration: cannot list Dexie tasks: ${(err as Error)?.message ?? err}`);
		return report;
	}
	for (const task of tasks) {
		if (!isMigratable(task.status)) {
			report.skippedTerminal.push(task.id);
			continue;
		}
		try {
			const created = await deps.client.create(browserTaskToHostInput(task));
			report.migrated.push(task.id);
			// Cancel the original AFTER the host copy exists. If this fails,
			// the task may run twice — loud log, and the browser authority
			// is already dormant under host cutover so the window is small.
			try {
				await deps.store.update(task.id, {
					status: 'cancelled',
					finishedAt: Date.now(),
					lastError: {
						message: `migrated to host task ${created.id}`,
						retryable: false,
						timestamp: Date.now()
					}
				});
			} catch (err) {
				deps.log?.(
					`task migration: host copy ${created.id} created but Dexie original ${task.id} NOT cancelled: ${(err as Error)?.message ?? err}`
				);
			}
		} catch (err) {
			report.failed.push({ id: task.id, error: (err as Error)?.message ?? String(err) });
		}
	}
	return report;
}
