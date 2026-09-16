// Task orchestrator: owns the scheduler tick lifecycle, launches task runs,
// routes events to waiting tasks, and exposes submit/cancel/review. Pure
// orchestration over injected boundaries; the browser singleton wires the
// Dexie store and the avatar bridge, tests use memory + fakes.
import type { TaskStore } from './store.ts';
import { makeEvent, TaskEventBus } from './events.ts';
import type { TaskEvent } from './types.ts';
import {
	dispatchOne,
	heartbeatInFlight,
	promoteDue,
	recoverExpiredLeases,
	resumeTimedOutWaits,
	resumeWaitingForEvent,
	type SchedulerDeps
} from './scheduler.ts';
import {
	runContinuation,
	type AvatarRoutineRunner,
	type ExecutorDeps,
	type NotificationSink
} from './executor.ts';
import type { DurableTask, NewTask, TaskEvent as TaskEventType, TaskStatus } from './types.ts';

export interface OrchestratorDeps {
	store: TaskStore;
	bus?: TaskEventBus;
	clock?: () => number;
	avatarRunner: AvatarRoutineRunner;
	notifier: NotificationSink;
	pollMs?: number;
}

export class TaskOrchestrator {
	private store: TaskStore;
	private bus: TaskEventBus;
	private clock: () => number;
	private avatarRunner: AvatarRoutineRunner;
	private notifier: NotificationSink;
	private pollMs: number;
	private timer: ReturnType<typeof setInterval> | null = null;
	private inFlight = new Set<string>();
	private offEvent: (() => void) | null = null;
	private ticking = false;
	private tickQueued = false;

	constructor(deps: OrchestratorDeps) {
		this.store = deps.store;
		this.bus = deps.bus ?? new TaskEventBus();
		this.clock = deps.clock ?? (() => Date.now());
		this.avatarRunner = deps.avatarRunner;
		this.notifier = deps.notifier;
		this.pollMs = deps.pollMs ?? 5000;
	}

	get eventBus(): TaskEventBus {
		return this.bus;
	}

	private schedulerDeps(): SchedulerDeps {
		return {
			store: this.store,
			bus: this.bus,
			clock: this.clock,
			launch: (taskId) => void this.launch(taskId),
			inFlight: this.inFlight
		};
	}

	private executorDeps(): ExecutorDeps {
		return {
			store: this.store,
			bus: this.bus,
			clock: this.clock,
			avatarRunner: this.avatarRunner,
			notifier: this.notifier
		};
	}

	private async launch(taskId: string): Promise<void> {
		if (this.inFlight.has(taskId)) return;
		this.inFlight.add(taskId);
		try {
			await runContinuation(this.executorDeps(), taskId);
		} catch (e) {
			// runContinuation is total, but a boundary throwing outside its
			// guards must still release the worker, never wedge it.
			console.error('[Tasks] run crashed:', e);
			const task = await this.store.get(taskId);
			if (task && task.status === 'running') {
				await this.store.update(taskId, {
					status: 'failed',
					finishedAt: this.clock(),
					lastError: {
						message: e instanceof Error ? e.message : String(e),
						retryable: false,
						timestamp: this.clock()
					}
				});
				this.bus.emit(makeEvent('task.failed', this.clock(), { sourceId: taskId }));
			}
		} finally {
			this.inFlight.delete(taskId);
		}
	}

	private async onEvent(event: TaskEvent): Promise<void> {
		await resumeWaitingForEvent(this.schedulerDeps(), event);
		// A resumed task should not wait for the next poll.
		void this.tick();
	}

	async tick(): Promise<void> {
		// Overlapping ticks are coalesced, never dropped: a tick requested
		// mid-tick runs one more pass when the current pass finishes (capped
		// so an event storm still yields to the next poll).
		if (this.ticking) {
			this.tickQueued = true;
			return;
		}
		this.ticking = true;
		try {
			let passes = 0;
			do {
				this.tickQueued = false;
				const deps = this.schedulerDeps();
				await promoteDue(deps);
				await recoverExpiredLeases(deps);
				await resumeTimedOutWaits(deps);
				await heartbeatInFlight(deps);
				await dispatchOne(deps);
				passes++;
			} while (this.tickQueued && passes < 8);
		} finally {
			this.ticking = false;
			this.tickQueued = false;
		}
	}

	start(): void {
		if (this.timer) return;
		this.offEvent = this.bus.on('*', (event) => void this.onEvent(event));
		// Recovery first: due scheduled tasks, expired leases, and timed-out
		// waits resolve before the first dispatch.
		void this.tick();
		this.timer = setInterval(() => void this.tick().catch((e) => console.error('[Tasks] tick error:', e)), this.pollMs);
	}

	stop(): void {
		if (this.timer) {
			clearInterval(this.timer);
			this.timer = null;
		}
		this.offEvent?.();
		this.offEvent = null;
	}

	get running(): boolean {
		return this.timer !== null;
	}

	async submit(task: NewTask): Promise<DurableTask> {
		const created = await this.store.create(task);
		// Wake the scheduler immediately instead of waiting for the poll.
		void this.tick();
		return created;
	}

	async cancel(id: string): Promise<DurableTask | null> {
		const task = await this.store.get(id);
		if (!task) return null;
		if (task.status === 'completed' || task.status === 'failed' || task.status === 'cancelled') {
			return task;
		}
		return this.store.update(id, { status: 'cancelled', finishedAt: this.clock() });
	}

	// Resolve a needs_review task. Approval completes the current step with
	// the note and re-queues; rejection cancels the task.
	async review(id: string, approved: boolean, note?: string): Promise<DurableTask | null> {
		const task = await this.store.get(id);
		if (!task || task.status !== 'needs_review') return task;
		if (!approved) {
			return this.store.update(id, {
				status: 'cancelled',
				finishedAt: this.clock(),
				lastError: { message: note ?? 'rejected in review', retryable: false, timestamp: this.clock() }
			});
		}
		const step = task.steps[task.currentStepIndex];
		const updated = await this.store.update(id, {
			status: 'ready',
			steps: task.steps.map((s) =>
				s.id === step?.id ? { ...s, status: 'completed' as const, result: { approved: true, note } } : s
			),
			currentStepIndex: task.currentStepIndex + 1
		});
		void this.tick();
		return updated;
	}

	async get(id: string): Promise<DurableTask | null> {
		return this.store.get(id);
	}

	async list(statuses?: TaskStatus[]): Promise<DurableTask[]> {
		return this.store.list(statuses);
	}
}

export type { TaskEventType };
