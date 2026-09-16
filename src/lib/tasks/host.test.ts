import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import {
	createHostTaskClient,
	parseCapabilityReview,
	type HostTask,
	type InvokeFn,
	type NewHostTask
} from './host.ts';
import {
	AVATAR_ROUTINE_COMPLETED,
	AVATAR_ROUTINE_REQUESTED,
	handleRoutineRequest,
	parseRoutineRequest,
	routineResultToReceipt,
	routineStepKey,
	runTimeoutFor,
	startHostAvatarListener,
	type HostRoutineRequest
} from './host-avatar.ts';
import {
	browserTaskToHostInput,
	isMigratable,
	migrateDexieTasksToHost
} from './migration.ts';
import { HOST_EVENT } from '../services/native/bridge.ts';
import type { RoutineResult } from '../stores/vrm.svelte.ts';
import type { DurableTask } from './types.ts';

function hostTask(overrides: Partial<HostTask> = {}): HostTask {
	return {
		id: 'task-1',
		title: 't',
		instruction: 'i',
		status: 'pending',
		priority: 50,
		created_at: 1,
		updated_at: 1,
		steps: [],
		current_step_index: 0,
		attempts: 0,
		max_attempts: 3,
		...overrides
	};
}

describe('host task client', () => {
	it('creates tasks through task.create', async () => {
		const calls: Array<{ method: string; params?: Record<string, unknown> }> = [];
		const invoke: InvokeFn = async (method, params) => {
			calls.push({ method, params });
			return hostTask({ id: 'new-id', status: 'pending' });
		};
		const client = createHostTaskClient(invoke);
		const created = await client.create({
			title: 'wave',
			instruction: 'wave',
			steps: [{ step_type: 'avatar_routine', input: { steps: [] } }]
		});
		assert.equal(created.id, 'new-id');
		assert.equal(calls[0].method, 'task.create');
		assert.equal((calls[0].params as Record<string, unknown>).title, 'wave');
	});

	it('delivers events and reports delivery', async () => {
		const invoke: InvokeFn = async (method) => {
			assert.equal(method, 'task.event');
			return { delivered: true, task: hostTask({ status: 'ready' }) };
		};
		const client = createHostTaskClient(invoke);
		const result = await client.deliverEvent(AVATAR_ROUTINE_COMPLETED, 'task-1', {
			status: 'success'
		});
		assert.equal(result.delivered, true);
		assert.equal(result.task?.status, 'ready');
	});

	it('reviews resolve parked tasks', async () => {
		const seen: Record<string, unknown>[] = [];
		const invoke: InvokeFn = async (method, params) => {
			assert.equal(method, 'task.review');
			seen.push(params ?? {});
			return hostTask({ status: 'ready' });
		};
		const client = createHostTaskClient(invoke);
		await client.review('task-1', true, 'looks good');
		assert.equal(seen[0].approved, true);
		assert.equal(seen[0].note, 'looks good');
	});

	it('get returns null for unknown tasks', async () => {
		const invoke: InvokeFn = async () => {
			throw new Error("task not found: unknown task 'x'");
		};
		const client = createHostTaskClient(invoke);
		assert.equal(await client.get('x'), null);
	});

	it('parses capability reviews and rejects plain text', () => {
		const structured = parseCapabilityReview(
			JSON.stringify({
				kind: 'capability',
				tool: 'notification.show',
				capability: 'NotificationSend',
				resource: 'NotificationService',
				detail: 'needs approval'
			})
		);
		assert.equal(structured?.tool, 'notification.show');
		assert.equal(parseCapabilityReview('just a human note'), null);
		assert.equal(parseCapabilityReview(undefined), null);
	});
});

function routineResult(overrides: Partial<RoutineResult> = {}): RoutineResult {
	return {
		routineId: 'r-1',
		status: 'completed',
		expected: ['wave'],
		completed: ['wave'],
		failures: [],
		startedAt: 1,
		finishedAt: 2,
		at: 2,
		seq: 1,
		...overrides
	};
}

describe('host avatar listener', () => {
	it('locks the canonical step key format', () => {
		assert.equal(routineStepKey({ kind: 'walk', action: 'walk', direction: 'left' }), 'walk:walk:left');
		assert.equal(routineStepKey({ kind: 'emote', action: 'wave' }), 'emote:wave');
		assert.equal(routineStepKey({ kind: 'jump', action: 'jump' }), 'jump:jump');
		assert.equal(routineStepKey({ kind: 'walk', action: 'run', direction: 'left' }), 'walk:run:left');
		assert.equal(
			routineStepKey({ kind: 'turn', action: 'turn', direction: 'back' }),
			'turn:turn:back'
		);
		assert.equal(routineStepKey({ kind: 'return_home', action: 'return_home' }), 'return_home:return_home');
		assert.equal(
			routineStepKey({ kind: 'goto', action: 'goto', anchorId: 'chair' }),
			'goto:goto:chair'
		);
		assert.equal(
			routineStepKey({ kind: 'goto', action: 'goto', x: 0.9, z: 0.35 }),
			'goto:goto:0.90,0.35'
		);
	});

	it('maps only completed to success receipts', () => {
		assert.equal(routineResultToReceipt(routineResult()).status, 'success');
		for (const status of ['partial', 'failed', 'cancelled', 'timed_out'] as const) {
			const receipt = routineResultToReceipt(routineResult({ status, completed: [] }));
			assert.equal(receipt.status, 'failed');
		}
		const receipt = routineResultToReceipt(routineResult());
		assert.deepEqual(receipt.completed_steps, ['wave']);
		assert.equal(receipt.routine_id, 'r-1');
	});

	it('parses routine requests and ignores foreign events', () => {
		const request = parseRoutineRequest({
			event: AVATAR_ROUTINE_REQUESTED,
			data: {
				task_id: 't-1',
				step_id: 's-1',
				routine: { steps: [{ kind: 'emote', action: 'wave' }], receipt_timeout_ms: 30000 }
			}
		});
		assert.equal(request?.taskId, 't-1');
		assert.equal(request?.steps.length, 1);
		assert.equal(request?.receiptTimeoutMs, 30000);
		assert.equal(parseRoutineRequest({ event: 'agent.text_delta', data: {} }), null);
		assert.equal(parseRoutineRequest(null), null);
		assert.equal(
			parseRoutineRequest({
				event: AVATAR_ROUTINE_REQUESTED,
				data: { task_id: 't', routine: {} }
			}),
			null
		);
	});

	it('keeps the local run under the host receipt budget', () => {
		const base: HostRoutineRequest = { taskId: 't', stepId: 's', steps: [] };
		assert.equal(runTimeoutFor({ ...base, receiptTimeoutMs: 30000 }), 28000);
		assert.equal(runTimeoutFor(base), 55000);
		assert.equal(runTimeoutFor({ ...base, receiptTimeoutMs: 1000 }), 5000);
	});

	it('posts a success receipt after the run', async () => {
		const delivered: Array<{ type: string; correlation: string | undefined; payload: unknown }> = [];
		await handleRoutineRequest(
			{ taskId: 't-1', stepId: 's-1', steps: [] },
			{
				runRoutine: async () => routineResult(),
				deliver: async (type, correlation, payload) => {
					delivered.push({ type, correlation, payload });
				}
			}
		);
		assert.equal(delivered.length, 1);
		assert.equal(delivered[0].type, AVATAR_ROUTINE_COMPLETED);
		assert.equal(delivered[0].correlation, 't-1');
		assert.equal((delivered[0].payload as { status: string }).status, 'success');
	});

	it('posts a failed receipt when the run throws', async () => {
		const delivered: Array<{ payload: unknown }> = [];
		const logs: string[] = [];
		await handleRoutineRequest(
			{ taskId: 't-9', stepId: 's-9', steps: [] },
			{
				runRoutine: async () => {
					throw new Error('renderer exploded');
				},
				deliver: async (_t, _c, payload) => {
					delivered.push({ payload });
				},
				log: (message) => logs.push(message)
			}
		);
		assert.equal(delivered.length, 1);
		assert.equal((delivered[0].payload as { status: string }).status, 'failed');
		assert.match(logs[0], /t-9/);
	});

	it('subscribes and unsubscribes on an event target', async () => {
		const target = new EventTarget();
		let runs = 0;
		const off = startHostAvatarListener({
			events: target,
			runRoutine: async () => {
				runs += 1;
				return routineResult();
			},
			deliver: async () => {}
		});
		target.dispatchEvent(
			new CustomEvent(HOST_EVENT, {
				detail: {
					event: AVATAR_ROUTINE_REQUESTED,
					data: { task_id: 't', step_id: 's', routine: { steps: [] } }
				}
			})
		);
		await new Promise((resolve) => setTimeout(resolve, 10));
		assert.equal(runs, 1);
		off();
		target.dispatchEvent(
			new CustomEvent(HOST_EVENT, {
				detail: {
					event: AVATAR_ROUTINE_REQUESTED,
					data: { task_id: 't', step_id: 's', routine: { steps: [] } }
				}
			})
		);
		await new Promise((resolve) => setTimeout(resolve, 10));
		assert.equal(runs, 1);
	});
});

function dexieTask(overrides: Partial<DurableTask> = {}): DurableTask {
	return {
		id: 'dex-1',
		title: 'dex',
		instruction: 'do',
		status: 'ready',
		priority: 50,
		createdAt: 1,
		updatedAt: 1,
		attempts: 0,
		maxAttempts: 3,
		steps: [
			{
				id: 'st-1',
				type: 'wait',
				status: 'pending',
				input: { durationMs: 100 },
				attempts: 0,
				maxAttempts: 1
			}
		],
		currentStepIndex: 0,
		...overrides
	};
}

describe('dexie to host migration', () => {
	it('maps browser tasks to host create bodies', () => {
		const input = browserTaskToHostInput(
			dexieTask({ scheduledAt: 500, verification: { type: 'result_present' } })
		);
		assert.equal(input.title, 'dex');
		assert.equal(input.scheduled_at, 500);
		assert.deepEqual(input.verification, { type: 'result_present' });
		assert.equal(input.steps[0].step_type, 'wait');
		assert.equal(input.steps[0].max_attempts, 1);
	});

	it('only migrates non-terminal tasks', () => {
		assert.equal(isMigratable('ready'), true);
		assert.equal(isMigratable('waiting'), true);
		assert.equal(isMigratable('needs_review'), true);
		assert.equal(isMigratable('completed'), false);
		assert.equal(isMigratable('failed'), false);
		assert.equal(isMigratable('cancelled'), false);
	});

	it('migrates then cancels originals exactly once', async () => {
		const tasks = [dexieTask({ id: 'a' }), dexieTask({ id: 'b', status: 'completed' })];
		const updated: Array<{ id: string; patch: Partial<DurableTask> }> = [];
		const created: string[] = [];
		const report = await migrateDexieTasksToHost({
			store: {
				list: async () => tasks,
				update: async (id, patch) => {
					updated.push({ id, patch });
					return null;
				}
			},
			client: {
				create: async (task: NewHostTask) => {
					created.push(task.title);
					return hostTask({ id: `host-${task.title}` });
				},
				get: async () => null,
				list: async () => [],
				cancel: async () => hostTask(),
				review: async () => hostTask(),
				deliverEvent: async () => ({ delivered: false, task: null })
			} as unknown as import('./host.ts').HostTaskClient
		});
		assert.deepEqual(report.migrated, ['a']);
		assert.deepEqual(report.skippedTerminal, ['b']);
		assert.deepEqual(report.failed, []);
		assert.equal(created.length, 1);
		assert.equal(updated.length, 1);
		assert.equal(updated[0].id, 'a');
		assert.equal(updated[0].patch.status, 'cancelled');
	});

	it('keeps originals when host creation fails', async () => {
		let updates = 0;
		const report = await migrateDexieTasksToHost({
			store: {
				list: async () => [dexieTask({ id: 'z' })],
				update: async () => {
					updates += 1;
					return null;
				}
			},
			client: {
				create: async () => {
					throw new Error('host down');
				}
			} as unknown as import('./host.ts').HostTaskClient
		});
		assert.deepEqual(report.migrated, []);
		assert.equal(report.failed.length, 1);
		assert.equal(updates, 0);
	});
});
