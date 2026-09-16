import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import {
	buildRoutineTaskInput,
	executeAvatarPlan,
	hostTaskToRoutineSummary,
	launchAvatarPlan,
	waitForHostTask,
	type AvatarTaskDeps
} from './avatar-task.ts';
import type { HostTask } from '../../tasks/host.ts';
import type { RoutineStepInput } from '../../stores/vrm.svelte.ts';

function hostTask(overrides: Partial<HostTask> = {}): HostTask {
	return {
		id: 'task-1',
		title: 'Chat avatar routine',
		instruction: 'run',
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

function receiptStep(result: unknown) {
	return {
		id: 'step-1',
		step_type: 'avatar_routine' as const,
		status: 'completed' as const,
		input: {},
		result,
		attempts: 1,
		max_attempts: 1
	};
}

const waveStep: RoutineStepInput = { kind: 'emote', action: 'wave' };

describe('routine task input', () => {
	it('builds a verified avatar_routine task', () => {
		const input = buildRoutineTaskInput({
			steps: [waveStep, { kind: 'walk', action: 'walk', direction: 'left' }]
		});
		assert.equal(input.steps.length, 1);
		assert.equal(input.steps[0].step_type, 'avatar_routine');
		assert.deepEqual(input.verification, {
			type: 'avatar_routine',
			expected_steps: ['emote:wave', 'walk:walk:left']
		});
		assert.equal(
			(input.steps[0].input as Record<string, unknown>).receipt_timeout_ms,
			120_000
		);
	});
});

describe('host task to routine summary', () => {
	it('maps completed tasks to completed summaries', () => {
		const summary = hostTaskToRoutineSummary(
			hostTask({
				status: 'completed',
				steps: [
					receiptStep({ status: 'success', completed_steps: ['emote:wave'], failures: [] })
				]
			})
		);
		assert.equal(summary.status, 'completed');
		assert.deepEqual(summary.completed, ['emote:wave']);
	});

	it('maps partial receipts to partial', () => {
		const summary = hostTaskToRoutineSummary(
			hostTask({
				status: 'failed',
				steps: [
					receiptStep({
						status: 'failed',
						completed_steps: ['emote:wave'],
						failures: [{ stepIndex: 1, key: 'walk:walk:left', reason: 'stuck' }]
					})
				]
			})
		);
		assert.equal(summary.status, 'partial');
		assert.equal(summary.failures.length, 1);
	});

	it('falls back to the task error without a receipt', () => {
		const summary = hostTaskToRoutineSummary(
			hostTask({
				status: 'failed',
				last_error: { message: 'no renderer', retryable: false, timestamp: 2 }
			})
		);
		assert.equal(summary.status, 'failed');
		assert.equal(summary.failures[0].reason, 'no renderer');
	});

	it('maps cancelled tasks to cancelled', () => {
		const summary = hostTaskToRoutineSummary(hostTask({ status: 'cancelled' }));
		assert.equal(summary.status, 'cancelled');
	});
});

describe('wait for host task', () => {
	it('polls until terminal', async () => {
		const states = [
			hostTask({ status: 'running' }),
			hostTask({ status: 'waiting' }),
			hostTask({ status: 'completed' })
		];
		let calls = 0;
		const terminal = await waitForHostTask(
			async () => states[Math.min(calls++, states.length - 1)],
			'task-1',
			{ sleep: async () => {} }
		);
		assert.equal(terminal.status, 'completed');
		assert.equal(calls, 3);
	});

	it('throws on timeout', async () => {
		await assert.rejects(
			waitForHostTask(async () => hostTask({ status: 'running' }), 'task-1', {
				timeoutMs: 0,
				sleep: async () => {}
			}),
			/did not finish within/
		);
	});

	it('throws when the task disappears', async () => {
		await assert.rejects(
			waitForHostTask(async () => null, 'task-1', { sleep: async () => {} }),
			/disappeared/
		);
	});
});

function deps(
	overrides: Partial<AvatarTaskDeps> = {}
): AvatarTaskDeps & { created: number; cancelled: string[]; localRuns: number } {
	const d: AvatarTaskDeps & { created: number; cancelled: string[]; localRuns: number } = {
		created: 0,
		cancelled: [],
		localRuns: 0,
		useHost: () => true,
		createTask: async () => {
			d.created += 1;
			return hostTask();
		},
		getTask: async () => hostTask({ status: 'completed' }),
		cancelTask: async (id) => {
			d.cancelled.push(id);
			return hostTask({ status: 'cancelled' });
		},
		runLocal: async () => {
			d.localRuns += 1;
			return { status: 'completed' as const, completed: ['emote:wave'], failures: [] };
		},
		sleep: async () => {}
	};
	return Object.assign(d, overrides);
}

describe('execute avatar plan', () => {
	it('runs locally without a host', async () => {
		const d = deps({ useHost: () => false });
		const summary = await executeAvatarPlan(d, { steps: [waveStep] });
		assert.equal(summary.status, 'completed');
		assert.equal(d.localRuns, 1);
		assert.equal(d.created, 0);
	});

	it('waits for the host task when bridged', async () => {
		const states = [hostTask({ status: 'running' }), hostTask({ status: 'completed' })];
		let calls = 0;
		const d = deps({ getTask: async () => states[Math.min(calls++, 1)] });
		const summary = await executeAvatarPlan(d, { steps: [waveStep] });
		assert.equal(summary.status, 'completed');
		assert.equal(d.created, 1);
		assert.equal(d.localRuns, 0);
	});

	it('falls back to local when submission fails', async () => {
		const d = deps({
			createTask: async () => {
				throw new Error('bridge down');
			}
		});
		const summary = await executeAvatarPlan(d, { steps: [waveStep] });
		assert.equal(summary.status, 'completed');
		assert.equal(d.localRuns, 1);
		assert.deepEqual(d.cancelled, []);
	});

	it('reports timed_out without duplicating the run', async () => {
		const d = deps({ getTask: async () => hostTask({ status: 'running' }) });
		const summary = await executeAvatarPlan(d, { steps: [waveStep] }, { timeoutMs: 0 });
		assert.equal(summary.status, 'timed_out');
		assert.equal(d.localRuns, 0);
		assert.deepEqual(d.cancelled, []);
	});

	it('cancels then falls back when the wait transport fails', async () => {
		const d = deps({
			getTask: async () => {
				throw new Error('ipc broken');
			}
		});
		const summary = await executeAvatarPlan(d, { steps: [waveStep] });
		assert.equal(summary.status, 'completed');
		assert.equal(d.localRuns, 1);
		assert.deepEqual(d.cancelled, ['task-1']);
	});
});

describe('launch avatar plan', () => {
	it('submits without waiting', async () => {
		const d = deps();
		launchAvatarPlan(d, { steps: [waveStep] });
		await new Promise((resolve) => setTimeout(resolve, 10));
		assert.equal(d.created, 1);
		assert.equal(d.localRuns, 0);
	});

	it('falls back to local when submission fails', async () => {
		const d = deps({
			createTask: async () => {
				throw new Error('bridge down');
			}
		});
		launchAvatarPlan(d, { steps: [waveStep] });
		await new Promise((resolve) => setTimeout(resolve, 10));
		assert.equal(d.localRuns, 1);
	});
});
