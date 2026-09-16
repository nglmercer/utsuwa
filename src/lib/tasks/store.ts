// TaskStore: persistence boundary for durable tasks. Async interface with two
// adapters: memory (node tests) and Dexie (browser). All timestamps are ms
// numbers so rows serialize cleanly.
import {
	isTerminalStatus,
	newStepId,
	newTaskId,
	type DurableTask,
	type NewTask,
	type TaskStatus
} from './types.ts';

export interface TaskStore {
	create(task: NewTask): Promise<DurableTask>;
	get(id: string): Promise<DurableTask | null>;
	update(id: string, patch: Partial<DurableTask>): Promise<DurableTask | null>;
	remove(id: string): Promise<void>;
	list(statuses?: TaskStatus[]): Promise<DurableTask[]>;
	// Atomically transition one task out of an expected status. Returns the
	// updated task, or null when another worker claimed it first.
	claim(id: string, from: TaskStatus[], patch: Partial<DurableTask>): Promise<DurableTask | null>;
	countByStatus(status: TaskStatus): Promise<number>;
}

export function hydrateNewTask(input: NewTask, now: number): DurableTask {
	return {
		id: newTaskId(),
		title: input.title,
		instruction: input.instruction,
		status: input.scheduledAt !== undefined && input.scheduledAt > now ? 'scheduled' : 'pending',
		priority: input.priority ?? 50,
		createdAt: now,
		updatedAt: now,
		scheduledAt: input.scheduledAt,
		attempts: 0,
		maxAttempts: input.maxAttempts ?? 3,
		verification: input.verification,
		parentTaskId: input.parentTaskId,
		steps: input.steps.map((step) => ({
			id: step.id ?? newStepId(),
			type: step.type,
			status: 'pending' as const,
			input: step.input,
			attempts: 0,
			maxAttempts: step.maxAttempts ?? 3
		})),
		currentStepIndex: 0
	};
}

export class MemoryTaskStore implements TaskStore {
	private tasks = new Map<string, DurableTask>();
	private clock: () => number;

	constructor(clock: () => number = () => Date.now()) {
		this.clock = clock;
	}

	private clone(task: DurableTask): DurableTask {
		return JSON.parse(JSON.stringify(task)) as DurableTask;
	}

	async create(task: NewTask): Promise<DurableTask> {
		const row = hydrateNewTask(task, this.clock());
		this.tasks.set(row.id, this.clone(row));
		return this.clone(row);
	}

	async get(id: string): Promise<DurableTask | null> {
		const row = this.tasks.get(id);
		return row ? this.clone(row) : null;
	}

	async update(id: string, patch: Partial<DurableTask>): Promise<DurableTask | null> {
		const row = this.tasks.get(id);
		if (!row) return null;
		const next = { ...this.clone(row), ...JSON.parse(JSON.stringify(patch)), updatedAt: this.clock() };
		this.tasks.set(id, next);
		return this.clone(next);
	}

	async remove(id: string): Promise<void> {
		this.tasks.delete(id);
	}

	async list(statuses?: TaskStatus[]): Promise<DurableTask[]> {
		const rows = [...this.tasks.values()];
		const filtered =
			statuses && statuses.length > 0
				? rows.filter((row) => (statuses as string[]).includes(row.status))
				: rows;
		filtered.sort((a, b) => a.createdAt - b.createdAt);
		return filtered.map((row) => this.clone(row));
	}

	async claim(id: string, from: TaskStatus[], patch: Partial<DurableTask>): Promise<DurableTask | null> {
		const row = this.tasks.get(id);
		if (!row || !(from as string[]).includes(row.status)) return null;
		if (isTerminalStatus(row.status)) return null;
		return this.update(id, patch);
	}

	async countByStatus(status: TaskStatus): Promise<number> {
		let count = 0;
		for (const row of this.tasks.values()) {
			if (row.status === status) count++;
		}
		return count;
	}
}
