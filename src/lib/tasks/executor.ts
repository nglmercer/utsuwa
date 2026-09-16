// Task executor: runs one task's steps from its persisted cursor. Pure
// orchestration logic over injected boundaries (store, event bus, avatar
// runner, notifier), so node tests drive it with fakes. Long steps never
// block the scheduler: waits park the task and return; the tick or an event
// resumes it later.
import type { TaskStore } from './store.ts';
import { makeEvent, type TaskEventBus } from './events.ts';
import { verifyTask } from './verifier.ts';
import type {
	AvatarRoutineStepInput,
	DurableTask,
	NotificationStepInput,
	TaskStep,
	WaitStepInput
} from './types.ts';

export interface AvatarRoutineRunner {
	run(input: AvatarRoutineStepInput): Promise<{ completed: string[]; detail?: unknown }>;
}

export interface NotificationSink {
	notify(input: NotificationStepInput, task: DurableTask): Promise<void>;
}

export interface ExecutorDeps {
	store: TaskStore;
	bus: TaskEventBus;
	clock?: () => number;
	avatarRunner: AvatarRoutineRunner;
	notifier: NotificationSink;
}

export const RETRY_BACKOFF_BASE_MS = 2000;
export const RETRY_BACKOFF_MAX_MS = 5 * 60 * 1000;

export function retryDelayMs(failedAttempt: number): number {
	return Math.min(RETRY_BACKOFF_MAX_MS, RETRY_BACKOFF_BASE_MS * 2 ** Math.max(0, failedAttempt - 1));
}

type StepOutcome =
	| { kind: 'completed'; result?: unknown }
	| { kind: 'wait'; waitFor: NonNullable<DurableTask['waitFor']> }
	| { kind: 'review'; reason: string }
	| { kind: 'failed'; message: string; retryable: boolean };

function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

async function withTimeout<T>(promise: Promise<T>, ms: number, message: string): Promise<T> {
	let timer: ReturnType<typeof setTimeout> | null = null;
	try {
		return await Promise.race([
			promise,
			new Promise<T>((_, reject) => {
				timer = setTimeout(() => reject(new Error(message)), ms);
			})
		]);
	} finally {
		if (timer) clearTimeout(timer);
	}
}

export function routineTimeoutMs(input: AvatarRoutineStepInput): number {
	const sum = input.steps.reduce((total, step) => total + (step.durationMs ?? 2000), 0);
	return Math.max(30000, (input.timeoutMs ?? sum + 15000));
}

async function runStep(
	deps: ExecutorDeps,
	task: DurableTask,
	step: TaskStep,
	now: number
): Promise<StepOutcome> {
	switch (step.type) {
		case 'avatar_routine': {
			const input = step.input as AvatarRoutineStepInput;
			try {
				const result = await withTimeout(
					deps.avatarRunner.run(input),
					routineTimeoutMs(input),
					`avatar routine timed out after ${routineTimeoutMs(input)}ms`
				);
				return { kind: 'completed', result };
			} catch (e) {
				return {
					kind: 'failed',
					message: e instanceof Error ? e.message : String(e),
					retryable: true
				};
			}
		}
		case 'notification': {
			try {
				await deps.notifier.notify(step.input as NotificationStepInput, task);
				return { kind: 'completed', result: { firedAt: now } };
			} catch (e) {
				return {
					kind: 'failed',
					message: e instanceof Error ? e.message : String(e),
					retryable: true
				};
			}
		}
		case 'wait': {
			const input = step.input as WaitStepInput;
			const timeoutAt =
				input.timeoutMs !== undefined
					? now + input.timeoutMs
					: input.durationMs !== undefined
						? now + input.durationMs
						: undefined;
			return {
				kind: 'wait',
				waitFor: { eventType: input.eventType, correlationId: input.correlationId, timeoutAt }
			};
		}
		case 'agent':
		case 'tool':
		case 'approval':
			return { kind: 'review', reason: `no executor attached for step type '${step.type}'` };
	}
}

async function failTask(
	deps: ExecutorDeps,
	task: DurableTask,
	step: TaskStep,
	message: string,
	retryable: boolean,
	now: number
): Promise<void> {
	const store = deps.store;
	step.status = 'failed';
	step.error = message;
	const steps = task.steps.map((s) => (s.id === step.id ? { ...step } : s));
	if (retryable && step.attempts < step.maxAttempts) {
		const backoff = retryDelayMs(step.attempts);
		await store.update(task.id, {
			status: 'ready',
			nextAttemptAt: now + backoff,
			steps: steps.map((s) => (s.id === step.id ? { ...s, status: 'pending' as const } : s)),
			lastError: { message, retryable, timestamp: now }
		});
		return;
	}
	await store.update(task.id, {
		status: 'failed',
		finishedAt: now,
		steps,
		lastError: { message, retryable, timestamp: now }
	});
	deps.bus.emit(makeEvent('task.failed', now, { sourceId: task.id, payload: { message } }));
}

// Run one task from its persisted cursor until it parks (wait/review), fails,
// or completes. Safe to call only for tasks the dispatcher claimed as
// running; re-checks the status after every await so cancellation wins.
export async function runContinuation(deps: ExecutorDeps, taskId: string): Promise<void> {
	const now = deps.clock ? deps.clock() : Date.now();
	let task = await deps.store.get(taskId);
	if (!task || task.status !== 'running') return;

	while (task.currentStepIndex < task.steps.length) {
		const step = task.steps[task.currentStepIndex];
		step.attempts += 1;
		step.status = 'running';
		step.error = undefined;
		await deps.store.update(task.id, {
			steps: task.steps.map((s) => (s.id === step.id ? { ...step } : s))
		});

		const outcome = await runStep(deps, task, step, deps.clock ? deps.clock() : Date.now());

		// Cancellation (or a crash-recovery claim) wins over any outcome.
		const fresh = await deps.store.get(task.id);
		if (!fresh || fresh.status !== 'running') return;
		task = fresh;
		const current = task.steps[task.currentStepIndex];
		const at = deps.clock ? deps.clock() : Date.now();

		if (outcome.kind === 'completed') {
			current.status = 'completed';
			current.result = outcome.result;
			task.result = outcome.result ?? task.result;
			await deps.store.update(task.id, {
				steps: task.steps.map((s) => (s.id === current.id ? { ...current } : s)),
				currentStepIndex: task.currentStepIndex + 1,
				result: task.result
			});
			task = (await deps.store.get(task.id)) as DurableTask;
			continue;
		}
		if (outcome.kind === 'wait') {
			await deps.store.update(task.id, {
				status: 'waiting',
				waitFor: outcome.waitFor,
				steps: task.steps.map((s) =>
					s.id === current.id ? { ...s, status: 'pending' as const } : s
				)
			});
			return;
		}
		if (outcome.kind === 'review') {
			await deps.store.update(task.id, {
				status: 'needs_review',
				lastError: { message: outcome.reason, retryable: false, timestamp: at }
			});
			deps.bus.emit(
				makeEvent('task.review_required', at, { sourceId: task.id, payload: { reason: outcome.reason } })
			);
			return;
		}
		await failTask(deps, task, current, outcome.message, outcome.retryable, at);
		return;
	}

	// All steps done: verify before completing.
	const verified = verifyTask(task);
	const at = deps.clock ? deps.clock() : Date.now();
	if (verified.ok === true) {
		await deps.store.update(task.id, { status: 'completed', finishedAt: at });
		deps.bus.emit(makeEvent('task.completed', at, { sourceId: task.id }));
		return;
	}
	if (verified.ok === 'review') {
		await deps.store.update(task.id, {
			status: 'needs_review',
			lastError: { message: verified.reason, retryable: false, timestamp: at }
		});
		deps.bus.emit(
			makeEvent('task.review_required', at, { sourceId: task.id, payload: { reason: verified.reason } })
		);
		return;
	}
	await failTask(
		deps,
		task,
		task.steps[task.steps.length - 1],
		`verification failed: ${verified.reason}`,
		true,
		at
	);
}
