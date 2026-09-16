import test from 'node:test';
import assert from 'node:assert/strict';

import {
	createRoutineController,
	type RoutineControllerOptions,
	type RoutineStepInput,
	type RoutineStepExecutor
} from './routine-controller.ts';
import type { RoutineResult, RoutineStepResult } from '../stores/vrm.svelte.ts';

interface Harness {
	options: RoutineControllerOptions;
	steps: Array<Omit<RoutineStepResult, 'at'>>;
	results: Array<Omit<RoutineResult, 'at' | 'seq'>>;
	started: RoutineStepInput[];
	stopped: RoutineStepInput[];
	clock: { now: number };
	changes: number;
}

function makeHarness(executor?: Partial<RoutineStepExecutor>): Harness {
	const harness: Harness = {
		options: null as unknown as RoutineControllerOptions,
		steps: [],
		results: [],
		started: [],
		stopped: [],
		clock: { now: 1000 },
		changes: 0
	};
	harness.options = {
		executor: {
			startStep: (step) => {
				harness.started.push(step);
				return (
					executor?.startStep?.(step, harness.started.length - 1) ?? {
						started: true,
						deadlineMs: 5000
					}
				);
			},
			stopStep: (step) => {
				harness.stopped.push(step);
				executor?.stopStep?.(step);
			}
		},
		reporter: {
			recordStep: (result) => {
				harness.steps.push(result);
			},
			recordResult: (result) => {
				harness.results.push(result);
			},
			now: () => harness.clock.now
		},
		getEndPosition: () => ({ x: 1, y: 2, z: 3 }),
		onChange: () => {
			harness.changes += 1;
		},
		log: () => {}
	};
	return harness;
}

const step = (overrides: Partial<RoutineStepInput> = {}): RoutineStepInput => ({
	kind: 'procedural',
	action: 'nod',
	...overrides
});

test('complete routine: every step completes, receipt is completed', () => {
	const harness = makeHarness();
	const controller = createRoutineController(harness.options);
	controller.start('r-1', [step(), step({ action: 'bow' })]);
	controller.completeCurrentStep();
	controller.completeCurrentStep();
	assert.equal(harness.results.length, 1);
	const result = harness.results[0];
	assert.equal(result.status, 'completed');
	assert.deepEqual(result.completed, ['procedural:nod', 'procedural:bow']);
	assert.deepEqual(result.failures, []);
	assert.deepEqual(result.expected, ['procedural:nod', 'procedural:bow']);
	assert.deepEqual(result.endPosition, { x: 1, y: 2, z: 3 });
	assert.equal(controller.active, null);
	const statuses = harness.steps.map((entry) => entry.status);
	assert.deepEqual(statuses, ['running', 'completed', 'running', 'completed']);
});

test('partial routine: failure halts with survivors reported', () => {
	const harness = makeHarness();
	const controller = createRoutineController(harness.options);
	controller.start('r-2', [step(), step({ action: 'bow' }), step({ action: 'shrug' })]);
	controller.completeCurrentStep();
	controller.failCurrentStep('leg stuck');
	assert.equal(harness.results.length, 1);
	const result = harness.results[0];
	assert.equal(result.status, 'partial');
	assert.deepEqual(result.completed, ['procedural:nod']);
	assert.equal(result.failures.length, 1);
	assert.equal(result.failures[0].reason, 'leg stuck');
	assert.equal(result.failures[0].stepIndex, 1);
	// The third step never started.
	assert.equal(harness.started.length, 2);
});

test('failed steps never silently become completed', () => {
	const harness = makeHarness();
	const controller = createRoutineController(harness.options);
	controller.start('r-3', [step({ action: 'jump', kind: 'jump' })]);
	controller.failCurrentStep('no floor');
	const result = harness.results[0];
	assert.equal(result.status, 'failed');
	assert.deepEqual(result.completed, []);
	assert.equal(result.failures[0].key, 'jump:jump');
	// The failed motion was halted.
	assert.equal(harness.stopped.length, 1);
});

test('cancel records the running step as cancelled, never success', () => {
	const harness = makeHarness();
	const controller = createRoutineController(harness.options);
	controller.start('r-4', [step(), step()]);
	controller.completeCurrentStep();
	controller.cancel('photo-mode');
	assert.equal(harness.results.length, 1);
	assert.equal(harness.results[0].status, 'cancelled');
	assert.deepEqual(harness.results[0].completed, ['procedural:nod']);
	const last = harness.steps[harness.steps.length - 1];
	assert.equal(last.status, 'cancelled');
	assert.equal(last.reason, 'photo-mode');
	assert.equal(controller.active, null);
});

test('timeout fails the step and finishes timed_out', () => {
	const harness = makeHarness({
		startStep: () => ({ started: true, deadlineMs: 100 })
	});
	const controller = createRoutineController(harness.options);
	controller.start('r-5', [step()]);
	controller.tick(0.05);
	assert.equal(harness.results.length, 0);
	controller.tick(0.06);
	assert.equal(harness.results.length, 1);
	assert.equal(harness.results[0].status, 'timed_out');
	assert.match(harness.results[0].failures[0].reason, /deadline/);
});

test('late events after finish are no-ops, never resurrecting', () => {
	const harness = makeHarness();
	const controller = createRoutineController(harness.options);
	controller.start('r-6', [step()]);
	controller.completeCurrentStep();
	assert.equal(harness.results[0].status, 'completed');
	controller.completeCurrentStep();
	controller.failCurrentStep('late');
	controller.cancel('late');
	controller.tick(99);
	assert.equal(harness.results.length, 1);
	assert.equal(harness.steps.filter((entry) => entry.status === 'completed').length, 1);
});

test('continueOnFailure runs every step and reports what survived', () => {
	const harness = makeHarness();
	const controller = createRoutineController(harness.options);
	controller.start('r-7', [step(), step({ action: 'bow' }), step({ action: 'shrug' })], {
		continueOnFailure: true
	});
	controller.failCurrentStep('first broke');
	controller.completeCurrentStep();
	controller.failCurrentStep('third broke');
	assert.equal(harness.results.length, 1);
	const result = harness.results[0];
	assert.equal(result.status, 'partial');
	assert.deepEqual(result.completed, ['procedural:bow']);
	assert.equal(result.failures.length, 2);
	assert.equal(harness.started.length, 3);
});

test('unknown steps fail instead of hanging', () => {
	const harness = makeHarness({
		startStep: () => ({ started: false, deadlineMs: 5000 })
	});
	const controller = createRoutineController(harness.options);
	controller.start('r-8', [step({ kind: 'mystery', action: 'mystery' })]);
	assert.equal(harness.results.length, 1);
	assert.equal(harness.results[0].status, 'failed');
	assert.equal(harness.results[0].failures[0].reason, 'unknown step');
});

test('starting a routine cancels the previous one', () => {
	const harness = makeHarness();
	const controller = createRoutineController(harness.options);
	controller.start('r-9', [step()]);
	controller.start('r-10', [step({ action: 'bow' })]);
	assert.equal(harness.results.length, 1);
	assert.equal(harness.results[0].routineId, 'r-9');
	assert.equal(harness.results[0].status, 'cancelled');
	controller.completeCurrentStep();
	assert.equal(harness.results.length, 2);
	assert.equal(harness.results[1].status, 'completed');
});

test('clip-derived deadlines extend emote steps', () => {
	const harness = makeHarness({
		startStep: () => ({ started: true, deadlineMs: 100 })
	});
	const controller = createRoutineController(harness.options);
	controller.start('r-11', [step({ kind: 'emote', action: 'wave' })]);
	controller.tick(0.09);
	// The clip actually started playing: budget becomes clip-derived.
	controller.setStepDeadline(5000);
	controller.resetStepElapsed();
	controller.tick(1);
	assert.equal(harness.results.length, 0);
	controller.completeCurrentStep();
	assert.equal(harness.results[0].status, 'completed');
});
