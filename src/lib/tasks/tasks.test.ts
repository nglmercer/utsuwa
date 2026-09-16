import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import { MemoryTaskStore } from './store.ts';
import { TaskEventBus, eventMatchesWait, makeEvent } from './events.ts';
import { verifyTask } from './verifier.ts';
import {
	dispatchOne,
	heartbeatInFlight,
	promoteDue,
	recoverExpiredLeases,
	resumeTimedOutWaits,
	resumeWaitingForEvent,
	type SchedulerDeps
} from './scheduler.ts';
import { runContinuation, retryDelayMs, type AvatarRoutineRunner, type NotificationSink } from './executor.ts';
import { TaskOrchestrator } from './orchestrator.ts';
import { TaskPriority, type DurableTask, type NewTask } from './types.ts';

function manualClock(start = 1_000_000) {
	let now = start;
	return {
		now: () => now,
		advance: (ms: number) => {
			now += ms;
		}
	};
}

function deps(overrides: Partial<SchedulerDeps> = {}): SchedulerDeps & { launched: string[] } {
	const launched: string[] = [];
	const clock = overrides.clock ?? (() => Date.now());
	return {
		store: new MemoryTaskStore(clock),
		bus: new TaskEventBus(),
		launch: (id: string) => {
			launched.push(id);
		},
		inFlight: new Set<string>(),
		...overrides,
		clock,
		launched
	} as SchedulerDeps & { launched: string[] };
}

function simpleTask(overrides: Partial<NewTask> = {}): NewTask {
	return {
		title: 'test',
		instruction: 'do the thing',
		steps: [{ type: 'notification', input: { title: 'hi', body: 'there' } }],
		...overrides
	};
}

describe('task store', () => {
	it('creates tasks as pending, or scheduled when in the future', async () => {
		const store = new MemoryTaskStore(() => 1000);
		const immediate = await store.create(simpleTask());
		assert.equal(immediate.status, 'pending');
		const future = await store.create(simpleTask({ scheduledAt: 5000 }));
		assert.equal(future.status, 'scheduled');
		const past = await store.create(simpleTask({ scheduledAt: 500 }));
		assert.equal(past.status, 'pending');
		assert.equal(immediate.steps[0].status, 'pending');
	});

	it('claims atomically: second claim from another status loses', async () => {
		const store = new MemoryTaskStore();
		const task = await store.create(simpleTask());
		await store.update(task.id, { status: 'ready' });
		const won = await store.claim(task.id, ['ready'], { status: 'running' });
		assert.equal(won?.status, 'running');
		const lost = await store.claim(task.id, ['ready'], { status: 'running' });
		assert.equal(lost, null);
	});

	it('lists and counts by status', async () => {
		const store = new MemoryTaskStore();
		const a = await store.create(simpleTask());
		await store.create(simpleTask());
		await store.update(a.id, { status: 'ready' });
		assert.equal(await store.countByStatus('ready'), 1);
		assert.equal((await store.list(['pending'])).length, 1);
		assert.equal((await store.list()).length, 2);
	});
});

describe('scheduler transitions', () => {
	it('promotes pending and due scheduled tasks to ready', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const due: string[] = [];
		d.bus.on('task.due', (event) => due.push(event.sourceId as string));
		const a = await d.store.create(simpleTask());
		const b = await d.store.create(simpleTask({ scheduledAt: clock.now() - 1 }));
		const c = await d.store.create(simpleTask({ scheduledAt: clock.now() + 60000 }));
		assert.equal(await promoteDue(d), 2);
		assert.equal((await d.store.get(a.id))?.status, 'ready');
		assert.equal((await d.store.get(b.id))?.status, 'ready');
		assert.equal((await d.store.get(c.id))?.status, 'scheduled');
		assert.deepEqual(due.sort(), [a.id, b.id].sort());
	});

	it('recovers expired leases back to ready, then fails after max attempts', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const task = await d.store.create(simpleTask({ maxAttempts: 1 }));
		await d.store.update(task.id, { status: 'running', leaseUntil: clock.now() - 1, attempts: 0 });
		// First expiry: attempts 1 <= max 1 -> back to ready.
		assert.equal(await recoverExpiredLeases(d), 1);
		assert.equal((await d.store.get(task.id))?.status, 'ready');
		// Expire again: attempts 2 > max 1 -> failed.
		await d.store.update(task.id, { status: 'running', leaseUntil: clock.now() - 1 });
		assert.equal(await recoverExpiredLeases(d), 1);
		const failed = await d.store.get(task.id);
		assert.equal(failed?.status, 'failed');
		assert.ok(failed?.finishedAt);
	});

	it('skips in-flight and healthy leases during recovery', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const task = await d.store.create(simpleTask());
		await d.store.update(task.id, { status: 'running', leaseUntil: clock.now() + 60000 });
		d.inFlight.add(task.id);
		assert.equal(await recoverExpiredLeases(d), 0);
		assert.equal((await d.store.get(task.id))?.status, 'running');
	});

	it('dispatches highest priority first, one at a time', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const low = await d.store.create(simpleTask({ priority: TaskPriority.Background }));
		const high = await d.store.create(simpleTask({ priority: TaskPriority.Urgent }));
		await d.store.update(low.id, { status: 'ready' });
		await d.store.update(high.id, { status: 'ready' });
		assert.equal(await dispatchOne(d), high.id);
		assert.deepEqual(d.launched, [high.id]);
		// A running task blocks the second dispatch.
		assert.equal(await dispatchOne(d), null);
	});

	it('respects retry backoff on dispatch', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const task = await d.store.create(simpleTask());
		await d.store.update(task.id, { status: 'ready', nextAttemptAt: clock.now() + 60000 });
		assert.equal(await dispatchOne(d), null);
		clock.advance(61000);
		assert.equal(await dispatchOne(d), task.id);
	});

	it('heartbeats in-flight leases', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const task = await d.store.create(simpleTask());
		await d.store.update(task.id, { status: 'running', leaseUntil: clock.now() + 1000 });
		d.inFlight.add(task.id);
		clock.advance(30000);
		await heartbeatInFlight(d);
		assert.ok((await d.store.get(task.id))?.leaseUntil as number > clock.now());
	});
});

describe('waits', () => {
	it('pure-duration waits complete on timeout', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const task = await d.store.create({
			title: 'waiter',
			instruction: 'wait',
			steps: [{ type: 'wait', input: { durationMs: 5000 } }]
		});
		await d.store.update(task.id, {
			status: 'waiting',
			waitFor: { timeoutAt: clock.now() + 5000 }
		});
		assert.equal(await resumeTimedOutWaits(d), 0);
		clock.advance(5000);
		assert.equal(await resumeTimedOutWaits(d), 1);
		const resumed = await d.store.get(task.id);
		assert.equal(resumed?.status, 'ready');
		assert.equal(resumed?.currentStepIndex, 1);
	});

	it('event waits resume on matching events only', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const task = await d.store.create({
			title: 'waiter',
			instruction: 'wait',
			steps: [{ type: 'wait', input: { eventType: 'avatar.completed', correlationId: 'r1' } }]
		});
		await d.store.update(task.id, {
			status: 'waiting',
			waitFor: { eventType: 'avatar.completed', correlationId: 'r1' }
		});
		const wrong = makeEvent('avatar.completed', clock.now(), { correlationId: 'r2' });
		assert.equal(await resumeWaitingForEvent(d, wrong), 0);
		const right = makeEvent('avatar.completed', clock.now(), { correlationId: 'r1' });
		assert.equal(await resumeWaitingForEvent(d, right), 1);
		assert.equal((await d.store.get(task.id))?.status, 'ready');
	});

	it('event waits fail retryably when the timeout passes', async () => {
		const clock = manualClock();
		const d = deps({ clock: clock.now });
		const task = await d.store.create({
			title: 'waiter',
			instruction: 'wait',
			steps: [{ type: 'wait', input: { eventType: 'avatar.completed', timeoutMs: 1000 }, maxAttempts: 1 }]
		});
		await d.store.update(task.id, {
			status: 'waiting',
			waitFor: { eventType: 'avatar.completed', timeoutAt: clock.now() + 1000 }
		});
		clock.advance(2000);
		assert.equal(await resumeTimedOutWaits(d), 1);
		// attempts(1) !< maxAttempts(1) -> terminal failure.
		assert.equal((await d.store.get(task.id))?.status, 'failed');
	});
});

describe('executor', () => {
	function fakes() {
		const notifications: Array<{ title: string; body: string }> = [];
		const routines: unknown[] = [];
		const avatarRunner: AvatarRoutineRunner = {
			run: async (input) => {
				routines.push(input);
				return { completed: input.steps.map((_, i) => `step-${i}`) };
			}
		};
		const notifier: NotificationSink = {
			notify: async (input) => {
				notifications.push({ title: input.title, body: input.body });
			}
		};
		return { notifications, routines, avatarRunner, notifier };
	}

	it('runs notification steps to completion with an event', async () => {
		const clock = manualClock();
		const store = new MemoryTaskStore(clock.now);
		const bus = new TaskEventBus();
		const f = fakes();
		const completed: string[] = [];
		bus.on('task.completed', (event) => completed.push(event.sourceId as string));
		const task = await store.create(simpleTask());
		await store.update(task.id, { status: 'running' });
		await runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: f.avatarRunner, notifier: f.notifier },
			task.id
		);
		assert.deepEqual(f.notifications, [{ title: 'hi', body: 'there' }]);
		assert.equal((await store.get(task.id))?.status, 'completed');
		assert.deepEqual(completed, [task.id]);
	});

	it('runs avatar routines through the injected runner', async () => {
		const clock = manualClock();
		const store = new MemoryTaskStore(clock.now);
		const bus = new TaskEventBus();
		const f = fakes();
		const task = await store.create({
			title: 'dance',
			instruction: 'dance',
			steps: [{ type: 'avatar_routine', input: { steps: [{ kind: 'walk', action: 'walk' }] } }]
		});
		await store.update(task.id, { status: 'running' });
		await runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: f.avatarRunner, notifier: f.notifier },
			task.id
		);
		assert.equal(f.routines.length, 1);
		assert.equal((await store.get(task.id))?.status, 'completed');
	});

	it('parks on wait steps and fails runner errors retryably', async () => {
		const clock = manualClock();
		const store = new MemoryTaskStore(clock.now);
		const bus = new TaskEventBus();
		const f = fakes();
		const task = await store.create({
			title: 'mixed',
			instruction: 'mixed',
			steps: [
				{ type: 'wait', input: { durationMs: 5000 } },
				{ type: 'notification', input: { title: 'x', body: 'y' } }
			]
		});
		await store.update(task.id, { status: 'running' });
		await runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: f.avatarRunner, notifier: f.notifier },
			task.id
		);
		const parked = await store.get(task.id);
		assert.equal(parked?.status, 'waiting');
		assert.equal(parked?.currentStepIndex, 0);

		const failing: AvatarRoutineRunner = {
			run: async () => {
				throw new Error('stage gone');
			}
		};
		const task2 = await store.create({
			title: 'flaky',
			instruction: 'flaky',
			steps: [{ type: 'avatar_routine', input: { steps: [] }, maxAttempts: 1 }]
		});
		await store.update(task2.id, { status: 'running' });
		await runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: failing, notifier: f.notifier },
			task2.id
		);
		const failed = await store.get(task2.id);
		assert.equal(failed?.status, 'failed');
		assert.ok(failed?.lastError?.message.includes('stage gone'));
	});

	it('sends agent/tool/approval steps to review', async () => {
		const clock = manualClock();
		const store = new MemoryTaskStore(clock.now);
		const bus = new TaskEventBus();
		const f = fakes();
		const reviews: string[] = [];
		bus.on('task.review_required', (event) => reviews.push(event.sourceId as string));
		const task = await store.create({
			title: 'think',
			instruction: 'think',
			steps: [{ type: 'agent', input: {} }]
		});
		await store.update(task.id, { status: 'running' });
		await runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: f.avatarRunner, notifier: f.notifier },
			task.id
		);
		assert.equal((await store.get(task.id))?.status, 'needs_review');
		assert.deepEqual(reviews, [task.id]);
	});

	it('cancellation during a run wins over the outcome', async () => {
		const clock = manualClock();
		const store = new MemoryTaskStore(clock.now);
		const bus = new TaskEventBus();
		const slow: AvatarRoutineRunner = {
			run: async () => {
				await new Promise((r) => setTimeout(r, 20));
				return { completed: [] };
			}
		};
		const f = fakes();
		const task = await store.create({
			title: 'slow',
			instruction: 'slow',
			steps: [{ type: 'avatar_routine', input: { steps: [] } }]
		});
		await store.update(task.id, { status: 'running' });
		const run = runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: slow, notifier: f.notifier },
			task.id
		);
		await store.update(task.id, { status: 'cancelled', finishedAt: clock.now() });
		await run;
		assert.equal((await store.get(task.id))?.status, 'cancelled');
	});

	it('verification gates completion', async () => {
		const clock = manualClock();
		const store = new MemoryTaskStore(clock.now);
		const bus = new TaskEventBus();
		const f = fakes();
		const task = await store.create({
			title: 'checked',
			instruction: 'checked',
			steps: [{ type: 'notification', input: { title: 'x', body: 'y' } }],
			verification: { type: 'result_present' }
		});
		await store.update(task.id, { status: 'running' });
		await runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: f.avatarRunner, notifier: f.notifier },
			task.id
		);
		assert.equal((await store.get(task.id))?.status, 'completed');

		const manual = await store.create({
			title: 'human',
			instruction: 'human',
			steps: [{ type: 'notification', input: { title: 'x', body: 'y' } }],
			verification: { type: 'human_review' }
		});
		await store.update(manual.id, { status: 'running' });
		await runContinuation(
			{ store, bus, clock: clock.now, avatarRunner: f.avatarRunner, notifier: f.notifier },
			manual.id
		);
		assert.equal((await store.get(manual.id))?.status, 'needs_review');
	});
});

describe('verifier', () => {
	it('passes none, requires results when asked', () => {
		const base = { verification: undefined, result: undefined } as unknown as DurableTask;
		assert.deepEqual(verifyTask(base), { ok: true });
		assert.deepEqual(verifyTask(base, { type: 'result_present' }).ok, false);
		assert.deepEqual(
			verifyTask({ ...base, result: { firedAt: 1 } }, { type: 'result_present' }),
			{ ok: true }
		);
		assert.deepEqual(verifyTask(base, { type: 'human_review' }).ok, 'review');
	});
});

describe('events', () => {
	it('matches waits by type and optional correlation', () => {
		const event = makeEvent('avatar.completed', 100, { correlationId: 'r1' });
		assert.ok(eventMatchesWait(event, { eventType: 'avatar.completed', correlationId: 'r1' }));
		assert.ok(eventMatchesWait(event, { eventType: 'avatar.completed' }));
		assert.ok(!eventMatchesWait(event, { eventType: 'avatar.completed', correlationId: 'r2' }));
		assert.ok(!eventMatchesWait(event, { eventType: 'timer.fired' }));
		assert.ok(!eventMatchesWait(event, {}));
	});

	it('emits to type and wildcard listeners; failures stay local', () => {
		const bus = new TaskEventBus();
		const seen: string[] = [];
		bus.on('task.completed', () => seen.push('typed'));
		bus.on('*', () => seen.push('wild'));
		bus.on('*', () => {
			throw new Error('boom');
		});
		bus.emit(makeEvent('task.completed', 1, { sourceId: 't' }));
		assert.deepEqual(seen.sort(), ['typed', 'wild']);
	});
});

describe('orchestrator', () => {
	function harness() {
		const clock = manualClock();
		const store = new MemoryTaskStore(clock.now);
		const bus = new TaskEventBus();
		const notifications: unknown[] = [];
		const orchestrator = new TaskOrchestrator({
			store,
			bus,
			clock: clock.now,
			avatarRunner: { run: async () => ({ completed: ['s0'] }) },
			notifier: {
				notify: async (input) => {
					notifications.push(input);
				}
			},
			pollMs: 3600000
		});
		return { clock, store, bus, notifications, orchestrator };
	}

	async function settle(ms = 30) {
		await new Promise((r) => setTimeout(r, ms));
	}

	it('runs a submitted task end to end', async () => {
		const h = harness();
		const created = await h.orchestrator.submit(simpleTask());
		await settle();
		assert.equal((await h.store.get(created.id))?.status, 'completed');
		assert.equal(h.notifications.length, 1);
	});

	it('holds scheduled tasks until due, then runs them', async () => {
		const h = harness();
		const created = await h.orchestrator.submit(simpleTask({ scheduledAt: h.clock.now() + 60000 }));
		await settle();
		assert.equal((await h.store.get(created.id))?.status, 'scheduled');
		h.clock.advance(61000);
		await h.orchestrator.tick();
		await settle();
		assert.equal((await h.store.get(created.id))?.status, 'completed');
	});

	it('resumes waiting tasks when their event arrives', async () => {
		const h = harness();
		h.orchestrator.start();
		try {
			const created = await h.orchestrator.submit({
				title: 'waiter',
				instruction: 'wait',
				steps: [
					{ type: 'wait', input: { eventType: 'avatar.completed', correlationId: 'r9' } },
					{ type: 'notification', input: { title: 'done', body: 'resumed' } }
				]
			});
			await settle();
			assert.equal((await h.store.get(created.id))?.status, 'waiting');
			h.bus.emit(makeEvent('avatar.completed', h.clock.now(), { correlationId: 'r9' }));
			await settle();
			assert.equal((await h.store.get(created.id))?.status, 'completed');
			assert.equal(h.notifications.length, 1);
		} finally {
			h.orchestrator.stop();
		}
	});

	it('survives a restart mid-run via lease recovery', async () => {
		const h = harness();
		// Simulate a crash: task stuck running with an expired lease, no worker.
		// Created directly in the store so no auto-tick interferes.
		const created = await h.store.create(simpleTask({ maxAttempts: 3 }));
		await h.store.update(created.id, {
			status: 'running',
			leaseUntil: h.clock.now() - 1,
			attempts: 0
		});
		await h.orchestrator.tick();
		assert.equal((await h.store.get(created.id))?.status, 'ready');
		h.clock.advance(3000);
		await h.orchestrator.tick();
		await settle();
		assert.equal((await h.store.get(created.id))?.status, 'completed');
	});

	it('cancels and reviews tasks', async () => {
		const h = harness();
		const wait = await h.orchestrator.submit({
			title: 'waiter',
			instruction: 'wait',
			steps: [{ type: 'wait', input: { durationMs: 60000 } }]
		});
		await settle();
		assert.equal((await h.store.get(wait.id))?.status, 'waiting');
		await h.orchestrator.cancel(wait.id);
		assert.equal((await h.store.get(wait.id))?.status, 'cancelled');

		const agent = await h.orchestrator.submit({
			title: 'think',
			instruction: 'think',
			steps: [{ type: 'agent', input: {} }]
		});
		await settle();
		assert.equal((await h.store.get(agent.id))?.status, 'needs_review');
		await h.orchestrator.review(agent.id, true, 'looks right');
		await settle();
		assert.equal((await h.store.get(agent.id))?.status, 'completed');
	});
});

describe('retry backoff', () => {
	it('doubles with a cap', () => {
		assert.equal(retryDelayMs(1), 2000);
		assert.equal(retryDelayMs(2), 4000);
		assert.equal(retryDelayMs(3), 8000);
		assert.equal(retryDelayMs(99), 300000);
	});
});
