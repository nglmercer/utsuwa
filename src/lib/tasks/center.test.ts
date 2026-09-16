import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import type { HostTask, HostTaskStatus, HostStepStatus } from './host.ts';
import {
	countCenterTasks,
	filterCenterTasks,
	formatTaskTime,
	isTerminalStatus,
	reviewSummary,
	shortId,
	statusLabel,
	stepProgress
} from './center.ts';

function task(overrides: Partial<HostTask> = {}): HostTask {
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

function stepStatus(status: HostStepStatus) {
	return {
		id: `step-${status}`,
		step_type: 'wait' as const,
		status,
		input: {},
		attempts: 0,
		max_attempts: 1
	};
}

describe('filterCenterTasks', () => {
	const tasks = [
		task({ id: 'a', status: 'running' }),
		task({ id: 'b', status: 'needs_review' }),
		task({ id: 'c', status: 'scheduled' }),
		task({ id: 'd', status: 'completed' }),
		task({ id: 'e', status: 'failed' })
	];

	it('partitions by lifecycle bucket', () => {
		assert.deepEqual(
			filterCenterTasks(tasks, 'active').map((t) => t.id),
			['a']
		);
		assert.deepEqual(
			filterCenterTasks(tasks, 'needs_review').map((t) => t.id),
			['b']
		);
		assert.deepEqual(
			filterCenterTasks(tasks, 'scheduled').map((t) => t.id),
			['c']
		);
		assert.deepEqual(
			filterCenterTasks(tasks, 'done').map((t) => t.id),
			['d', 'e']
		);
		assert.equal(filterCenterTasks(tasks, 'all').length, 5);
	});

	it('counts every bucket', () => {
		assert.deepEqual(countCenterTasks(tasks), {
			all: 5,
			active: 1,
			needs_review: 1,
			scheduled: 1,
			done: 2
		});
	});
});

describe('stepProgress', () => {
	it('counts completed and skipped steps as done', () => {
		const progress = stepProgress(
			task({
				steps: [
					stepStatus('completed'),
					stepStatus('skipped'),
					stepStatus('running'),
					stepStatus('pending')
				]
			})
		);
		assert.deepEqual(progress, { done: 2, total: 4 });
	});

	it('handles tasks without steps', () => {
		assert.deepEqual(stepProgress(task()), { done: 0, total: 0 });
	});
});

describe('status helpers', () => {
	it('labels every status', () => {
		const statuses: HostTaskStatus[] = [
			'pending',
			'scheduled',
			'ready',
			'running',
			'waiting',
			'needs_review',
			'completed',
			'failed',
			'cancelled'
		];
		for (const status of statuses) {
			assert.ok(statusLabel(status).length > 0, status);
		}
		assert.equal(statusLabel('needs_review'), 'Needs review');
		assert.equal(statusLabel('running'), 'Running');
	});

	it('knows terminal states', () => {
		assert.equal(isTerminalStatus('completed'), true);
		assert.equal(isTerminalStatus('failed'), true);
		assert.equal(isTerminalStatus('cancelled'), true);
		assert.equal(isTerminalStatus('running'), false);
		assert.equal(isTerminalStatus('needs_review'), false);
	});
});

describe('reviewSummary', () => {
	it('names the tool for capability-gated reviews', () => {
		const reason = JSON.stringify({
			kind: 'capability',
			tool: 'test.gated',
			capability: 'notification_send',
			resource: 'notification_service',
			detail: 'needs approval'
		});
		assert.equal(reviewSummary(reason), 'test.gated needs notification_send');
	});

	it('passes plain reasons through', () => {
		assert.equal(reviewSummary('check this'), 'check this');
		assert.equal(reviewSummary(undefined), null);
		assert.equal(reviewSummary(''), null);
	});
});

describe('formatting', () => {
	it('shortens long ids', () => {
		assert.equal(shortId('abcdef123456'), 'abcdef12');
		assert.equal(shortId('short'), 'short');
	});

	it('guards missing timestamps', () => {
		assert.equal(formatTaskTime(undefined), '—');
		assert.equal(formatTaskTime(Number.NaN), '—');
		assert.ok(formatTaskTime(1_700_000_000_000).length > 0);
	});
});
