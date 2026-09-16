import test from 'node:test';
import assert from 'node:assert/strict';

import { parseTaskProgressEvent, toastMessageFor, truncateToast } from './task-events.ts';

test('step events parse readings with ids', () => {
	const event = parseTaskProgressEvent('task.step_completed', {
		task_id: 't1',
		step_id: 's1',
		title: 'Clock reading interval task',
		text: '1: 07:27:26'
	});
	assert.deepEqual(event, {
		kind: 'step',
		taskId: 't1',
		stepId: 's1',
		title: 'Clock reading interval task',
		text: '1: 07:27:26'
	});
	assert.equal(toastMessageFor(event!), '1: 07:27:26');
});

test('terminal events summarize with joined text', () => {
	const event = parseTaskProgressEvent('task.terminal', {
		task_id: 't1',
		title: 'Clock reading interval task',
		status: 'completed',
		text: '1: 07:27:26\n2: 07:28:47'
	});
	assert.equal(event?.kind, 'terminal');
	assert.equal(
		toastMessageFor(event!),
		'Done: Clock reading interval task\n1: 07:27:26\n2: 07:28:47'
	);
});

test('failed terminals keep their status; blank text stays blank', () => {
	const failed = parseTaskProgressEvent('task.terminal', {
		task_id: 't1',
		title: 'Clock task',
		status: 'failed',
		text: ''
	});
	assert.equal(toastMessageFor(failed!), '');
	const failedText = parseTaskProgressEvent('task.terminal', {
		task_id: 't1',
		title: 'Clock task',
		status: 'failed',
		text: 'x'
	});
	assert.equal(toastMessageFor(failedText!), 'Clock task failed\nx');
});

test('foreign and malformed events are ignored', () => {
	assert.equal(parseTaskProgressEvent('agent.turn_done', {}), null);
	assert.equal(parseTaskProgressEvent('task.step_completed', { text: 'x' }), null);
	assert.equal(parseTaskProgressEvent('task.terminal', { title: 't', status: 'completed' }), null);
});

test('long toasts truncate with ellipsis', () => {
	assert.equal(truncateToast('short'), 'short');
	assert.equal(truncateToast('x'.repeat(300)).length, 280);
	assert.ok(truncateToast('x'.repeat(300)).endsWith('…'));
});
