// Dexie-backed TaskStore for the browser. Rows mirror the indexed scheduler
// columns plus the full task as JSON. Claims use the same atomic
// status-guarded modify pattern as the reminder pipeline, so two windows
// cannot dispatch the same task.
import { db, type DBTask } from '$lib/db/index';
import { hydrateNewTask, type TaskStore } from './store.ts';
import type { DurableTask, NewTask, TaskStatus } from './types.ts';

function toRow(task: DurableTask): DBTask {
	return {
		id: task.id,
		status: task.status,
		priority: task.priority,
		createdAt: task.createdAt,
		updatedAt: task.updatedAt,
		scheduledAt: task.scheduledAt,
		leaseUntil: task.leaseUntil,
		title: task.title,
		instruction: task.instruction,
		data: JSON.stringify(task)
	};
}

function fromRow(row: DBTask): DurableTask {
	return JSON.parse(row.data) as DurableTask;
}

export class DexieTaskStore implements TaskStore {
	private clock: () => number;

	constructor(clock: () => number = () => Date.now()) {
		this.clock = clock;
	}

	async create(task: NewTask): Promise<DurableTask> {
		const row = hydrateNewTask(task, this.clock());
		await db.tasks.add(toRow(row));
		return row;
	}

	async get(id: string): Promise<DurableTask | null> {
		const row = await db.tasks.get(id);
		return row ? fromRow(row) : null;
	}

	async update(id: string, patch: Partial<DurableTask>): Promise<DurableTask | null> {
		const current = await this.get(id);
		if (!current) return null;
		const next: DurableTask = {
			...current,
			...JSON.parse(JSON.stringify(patch)),
			updatedAt: this.clock()
		};
		await db.tasks.put(toRow(next));
		return next;
	}

	async remove(id: string): Promise<void> {
		await db.tasks.delete(id);
	}

	async list(statuses?: TaskStatus[]): Promise<DurableTask[]> {
		const rows =
			statuses && statuses.length > 0
				? await db.tasks.where('status').anyOf(statuses as string[]).toArray()
				: await db.tasks.toArray();
		rows.sort((a, b) => a.createdAt - b.createdAt);
		return rows.map(fromRow);
	}

	async claim(
		id: string,
		from: TaskStatus[],
		patch: Partial<DurableTask>
	): Promise<DurableTask | null> {
		const changed = await db.tasks
			.where('id')
			.equals(id)
			.and((row) => (from as string[]).includes(row.status))
			.modify((row) => {
				const current = JSON.parse(row.data) as DurableTask;
				const next: DurableTask = {
					...current,
					...JSON.parse(JSON.stringify(patch)),
					updatedAt: this.clock()
				};
				Object.assign(row, toRow(next));
			});
		if (changed === 0) return null;
		return this.get(id);
	}

	async countByStatus(status: TaskStatus): Promise<number> {
		return db.tasks.where('status').equals(status).count();
	}
}
