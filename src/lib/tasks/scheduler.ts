// Task scheduler: deterministic time/state transitions plus single-worker
// dispatch. Pure orchestration over the store; the tick never blocks on task
// execution (runs launch unawaited) and never asks the model anything.
import type { TaskStore } from './store.ts';
import { eventMatchesWait, makeEvent, type TaskEventBus } from './events.ts';
import { retryDelayMs } from './executor.ts';
import type { DurableTask, TaskEvent } from './types.ts';

export interface SchedulerDeps {
	store: TaskStore;
	bus: TaskEventBus;
	clock?: () => number;
	// Launched (not awaited) for a claimed task.
	launch: (taskId: string) => void;
	// Task ids with an in-flight run; the tick renews their leases.
	inFlight: Set<string>;
}

export const LEASE_MS = 60000;

function nowOf(deps: SchedulerDeps): number {
	return deps.clock ? deps.clock() : Date.now();
}

// Promote due work: pending always becomes ready; scheduled becomes ready at
// its time. Returns the number of tasks promoted.
export async function promoteDue(deps: SchedulerDeps): Promise<number> {
	const now = nowOf(deps);
	let promoted = 0;
	const candidates = await deps.store.list(['pending', 'scheduled']);
	for (const task of candidates) {
		if (task.status === 'pending') {
			const claimed = await deps.store.claim(task.id, ['pending'], { status: 'ready' });
			if (claimed) {
				promoted++;
				deps.bus.emit(makeEvent('task.due', now, { sourceId: task.id }));
			}
		} else if (task.scheduledAt !== undefined && task.scheduledAt <= now) {
			const claimed = await deps.store.claim(task.id, ['scheduled'], { status: 'ready' });
			if (claimed) {
				promoted++;
				deps.bus.emit(makeEvent('task.due', now, { sourceId: task.id }));
			}
		}
	}
	return promoted;
}

// Crash recovery: running tasks whose lease expired go back to ready (bounded
// by attempts) or failed. Also used at startup.
export async function recoverExpiredLeases(deps: SchedulerDeps): Promise<number> {
	const now = nowOf(deps);
	let recovered = 0;
	const running = await deps.store.list(['running']);
	for (const task of running) {
		if (deps.inFlight.has(task.id)) continue;
		if (task.leaseUntil !== undefined && task.leaseUntil >= now) continue;
		task.attempts += 1;
		if (task.attempts > task.maxAttempts) {
			const failed = await deps.store.claim(task.id, ['running'], {
				status: 'failed',
				finishedAt: now,
				attempts: task.attempts,
				lastError: {
					message: 'worker lease expired too many times',
					retryable: false,
					timestamp: now
				}
			});
			if (failed) {
				recovered++;
				deps.bus.emit(makeEvent('task.failed', now, { sourceId: task.id }));
			}
			continue;
		}
		const ready = await deps.store.claim(task.id, ['running'], {
			status: 'ready',
			attempts: task.attempts,
			nextAttemptAt: now + retryDelayMs(task.attempts),
			lastError: { message: 'worker lease expired; resuming', retryable: true, timestamp: now }
		});
		if (ready) recovered++;
	}
	return recovered;
}

// Waiting tasks: pure-duration waits complete their step on timeout; event
// waits with an expired timeout fail retryably (the event path completes them
// earlier via resumeWaitingForEvent).
export async function resumeTimedOutWaits(deps: SchedulerDeps): Promise<number> {
	const now = nowOf(deps);
	let resumed = 0;
	const waiting = await deps.store.list(['waiting']);
	for (const task of waiting) {
		const timeoutAt = task.waitFor?.timeoutAt;
		if (timeoutAt === undefined || timeoutAt > now) continue;
		if (!task.waitFor?.eventType) {
			const step = task.steps[task.currentStepIndex];
			const claimed = await deps.store.claim(task.id, ['waiting'], {
				status: 'ready',
				waitFor: undefined,
				steps: task.steps.map((s) =>
					s.id === step?.id ? { ...s, status: 'completed' as const, result: { waitedMs: timeoutAt } } : s
				),
				currentStepIndex: task.currentStepIndex + 1
			});
			if (claimed) resumed++;
			continue;
		}
		const step = task.steps[task.currentStepIndex];
		if (step) step.attempts += 1;
		if (step && step.attempts < step.maxAttempts) {
			const claimed = await deps.store.claim(task.id, ['waiting'], {
				status: 'ready',
				waitFor: undefined,
				nextAttemptAt: now + retryDelayMs(step.attempts),
				steps: task.steps.map((s) =>
					s.id === step.id ? { ...step, status: 'pending' as const, error: undefined } : s
				),
				lastError: { message: 'wait timed out before the event arrived', retryable: true, timestamp: now }
			});
			if (claimed) resumed++;
		} else {
			const claimed = await deps.store.claim(task.id, ['waiting'], {
				status: 'failed',
				finishedAt: now,
				waitFor: undefined,
				lastError: {
					message: 'wait timed out before the event arrived',
					retryable: false,
					timestamp: now
				}
			});
			if (claimed) {
				resumed++;
				deps.bus.emit(makeEvent('task.failed', now, { sourceId: task.id }));
			}
		}
	}
	return resumed;
}

// Event path: a matching event completes the current wait step immediately.
export async function resumeWaitingForEvent(deps: SchedulerDeps, event: TaskEvent): Promise<number> {
	const now = nowOf(deps);
	let resumed = 0;
	const waiting = await deps.store.list(['waiting']);
	for (const task of waiting) {
		if (!task.waitFor || !eventMatchesWait(event, task.waitFor)) continue;
		const step = task.steps[task.currentStepIndex];
		const claimed = await deps.store.claim(task.id, ['waiting'], {
			status: 'ready',
			waitFor: undefined,
			steps: task.steps.map((s) =>
				s.id === step?.id
					? { ...s, status: 'completed' as const, result: { eventId: event.id, payload: event.payload } }
					: s
			),
			currentStepIndex: task.currentStepIndex + 1
		});
		if (claimed) resumed++;
	}
	return resumed;
}

// Renew leases for in-flight runs so a healthy worker never looks crashed.
export async function heartbeatInFlight(deps: SchedulerDeps): Promise<void> {
	const now = nowOf(deps);
	for (const id of deps.inFlight) {
		const task = await deps.store.get(id);
		if (task && task.status === 'running') {
			await deps.store.update(id, { leaseUntil: now + LEASE_MS });
		}
	}
}

// Dispatch one task: the highest-priority ready task whose backoff elapsed,
// when no other task is running. Returns the dispatched id or null.
export async function dispatchOne(deps: SchedulerDeps): Promise<string | null> {
	const now = nowOf(deps);
	const running = await deps.store.countByStatus('running');
	if (running > 0) return null;
	const ready = await deps.store.list(['ready']);
	const eligible = ready
		.filter((task) => task.nextAttemptAt === undefined || task.nextAttemptAt <= now)
		.sort((a, b) => b.priority - a.priority || a.createdAt - b.createdAt);
	const next = eligible[0];
	if (!next) return null;
	const claimed = await deps.store.claim(next.id, ['ready'], {
		status: 'running',
		startedAt: next.startedAt ?? now,
		leaseUntil: now + LEASE_MS,
		nextAttemptAt: undefined
	});
	if (!claimed) return null;
	deps.launch(claimed.id);
	return claimed.id;
}

export async function runSchedulerTick(deps: SchedulerDeps): Promise<void> {
	await promoteDue(deps);
	await recoverExpiredLeases(deps);
	await resumeTimedOutWaits(deps);
	await heartbeatInFlight(deps);
	await dispatchOne(deps);
}
