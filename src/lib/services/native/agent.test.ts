import test from 'node:test';
import assert from 'node:assert/strict';

import {
	AGENT_HARD_TIMEOUT_MS,
	AGENT_NO_PROGRESS_TIMEOUT_MS,
	eventMatchesTurn,
	initialAgentChatState,
	isProgressEvent,
	parseAgentTurnEvent,
	reduceAgentEvent,
	reduceAgentSend,
	sendParams,
	statusFor
} from './agent.ts';
import { isNativeMutation, summarizeNativeToolSteps } from './tool-receipts.ts';

test('turn_done parses text, steps, and truncation', () => {
	const event = parseAgentTurnEvent('agent.turn_done', {
		text: 'read it',
		executed: [{ id: 'e1', name: 'filesystem.read', output: { content: 'hi' } }],
		truncated: false
	});
	assert.equal(event?.kind, 'done');
	if (event?.kind !== 'done') throw new Error('unreachable');
	assert.equal(event.done.text, 'read it');
	assert.equal(event.done.executed.length, 1);
	assert.equal(event.done.executed[0].name, 'filesystem.read');
});

test('malformed steps are dropped, not fatal', () => {
	const event = parseAgentTurnEvent('agent.turn_done', {
		text: 't',
		executed: [{ id: 'e1' }, 'junk', null, { id: 'e2', name: 'filesystem.read' }]
	});
	assert.equal(event?.kind, 'done');
	if (event?.kind !== 'done') throw new Error('unreachable');
	assert.equal(event.done.executed.length, 1);
	assert.equal(event.done.executed[0].id, 'e2');
});

test('turn_done keeps failed and successful native tool attempts', () => {
	const event = parseAgentTurnEvent('agent.turn_done', {
		text: 'recovered',
		tool_steps: [
			{
				id: 'call-1',
				name: 'filesystem.write',
				status: 'failed',
				ok: false,
				error: 'parent directory does not exist: /tmp/home/Desktop'
			},
			{
				id: 'call-2',
				name: 'filesystem.write',
				status: 'success',
				ok: true,
				output: { path: '/tmp/home/Escritorio/hello.txt', created: true }
			}
		]
	});
	assert.equal(event?.kind, 'done');
	if (event?.kind !== 'done') throw new Error('unreachable');
	assert.equal(event.done.toolSteps.length, 2);
	assert.equal(event.done.toolSteps[0].ok, false);
	assert.match(event.done.toolSteps[0].error ?? '', /parent directory/);
	assert.equal(event.done.toolSteps[1].output && (event.done.toolSteps[1].output as { path: string }).path, '/tmp/home/Escritorio/hello.txt');
});

test('native receipt summary treats a successful mutation retry as recovered', () => {
	const event = parseAgentTurnEvent('agent.turn_done', {
		text: 'created',
		tool_steps: [
			{
				id: 'wrong',
				name: 'filesystem.write',
				status: 'failed',
				ok: false,
				error: 'parent directory does not exist'
			},
			{
				id: 'right',
				name: 'filesystem.create_user_file',
				status: 'success',
				ok: true,
				output: { path: '/tmp/home/Escritorio/note.txt', created: true }
			}
		]
	});
	assert.equal(event?.kind, 'done');
	if (event?.kind !== 'done') throw new Error('unreachable');
	assert.equal(summarizeNativeToolSteps(event.done.toolSteps), 'recovered');
});

test('native receipt summary keeps an unrecovered failure as failed', () => {
	const event = parseAgentTurnEvent('agent.turn_done', {
		text: 'could not create it',
		tool_steps: [
			{
				id: 'wrong',
				name: 'filesystem.write',
				status: 'failed',
				ok: false,
				error: 'parent directory does not exist'
			}
		]
	});
	assert.equal(event?.kind, 'done');
	if (event?.kind !== 'done') throw new Error('unreachable');
	assert.equal(summarizeNativeToolSteps(event.done.toolSteps), 'failed');
});

test('recoverable edit validation is shown as retry-needed, not failed', () => {
	const event = parseAgentTurnEvent('agent.turn_done', {
		text: 'I need the current line',
		tool_steps: [
			{
				id: 'edit-1',
				name: 'filesystem.edit',
				status: 'retry',
				ok: false,
				error: 'Edit needs more information'
			}
		]
	});
	assert.equal(event?.kind, 'done');
	if (event?.kind !== 'done') throw new Error('unreachable');
	assert.equal(event.done.toolSteps[0].status, 'retry');
	assert.equal(summarizeNativeToolSteps(event.done.toolSteps), 'retry');
});

test('suspended/failed/cancelled parse; foreign events ignored', () => {
	const suspended = parseAgentTurnEvent('agent.turn_suspended', {
		text: 'need approval',
		request_id: 'perm-3'
	});
	assert.equal(suspended?.kind, 'suspended');

	const failed = parseAgentTurnEvent('agent.turn_failed', { error: 'model is not configured' });
	assert.equal(failed?.kind, 'failed');
	if (failed?.kind !== 'failed') throw new Error('unreachable');
	assert.match(failed.error, /not configured/);

	assert.equal(parseAgentTurnEvent('agent.turn_cancelled', {})?.kind, 'cancelled');
	assert.equal(parseAgentTurnEvent('permission.requested', {}) , null);
	assert.equal(parseAgentTurnEvent('agent.turn_done', { text: 42 }), null);
	assert.equal(parseAgentTurnEvent('agent.turn_suspended', { text: 'x' }), null);
});

test('state machine runs the send → suspend → done arc', () => {
	let state = initialAgentChatState();
	assert.equal(statusFor(state), 'Idle');

	state = reduceAgentSend(state);
	assert.equal(state.phase, 'running');
	assert.equal(statusFor(state), 'Thinking…');

	const suspended = parseAgentTurnEvent('agent.turn_suspended', {
		text: 'partial',
		request_id: 'perm-1'
	})!;
	state = reduceAgentEvent(state, suspended);
	assert.equal(state.phase, 'suspended');
	assert.equal(state.requestId, 'perm-1');
	assert.equal(statusFor(state), 'Waiting for your approval');

	const done = parseAgentTurnEvent('agent.turn_done', { text: 'final', executed: [] })!;
	state = reduceAgentEvent(state, done);
	assert.equal(state.phase, 'idle');
	assert.equal(state.latest, 'final');
	assert.equal(state.requestId, null);
});

test('turn events carry their turn id; others are ignored by match', () => {
	const done = parseAgentTurnEvent('agent.turn_done', { text: 'hi', turn_id: 't1' });
	assert.equal(done?.kind, 'done');
	if (done?.kind !== 'done') throw new Error('unreachable');
	assert.equal(done.turnId, 't1');
	assert.equal(eventMatchesTurn(done, 't1'), true);
	assert.equal(eventMatchesTurn(done, 't2'), false);

	const cancelled = parseAgentTurnEvent('agent.turn_cancelled', { turn_id: 't9' });
	assert.equal(cancelled?.kind, 'cancelled');
	if (cancelled?.kind !== 'cancelled') throw new Error('unreachable');
	assert.equal(eventMatchesTurn(cancelled, 't1'), false);

	// Missing ids degrade to unfiltered instead of hanging.
	const legacy = parseAgentTurnEvent('agent.turn_done', { text: 'hi' });
	assert.equal(legacy?.kind, 'done');
	if (legacy?.kind !== 'done') throw new Error('unreachable');
	assert.equal(legacy.turnId, null);
	assert.equal(eventMatchesTurn(legacy, 't1'), true);
	assert.equal(eventMatchesTurn(done, null), true);
});

test('progress events reset the no-progress watchdog; terminal ones do not', () => {
	const delta = parseAgentTurnEvent('agent.text_delta', { delta: 'hi' })!;
	const started = parseAgentTurnEvent('agent.tool_started', { id: 'c1', name: 'x.y' })!;
	const finished = parseAgentTurnEvent('agent.tool_finished', { id: 'c1', name: 'x.y', ok: true })!;
	const done = parseAgentTurnEvent('agent.turn_done', { text: 'hi' })!;
	const suspended = parseAgentTurnEvent('agent.turn_suspended', { text: 'p', request_id: 'r' })!;
	assert.equal(isProgressEvent(delta), true);
	assert.equal(isProgressEvent(started), true);
	assert.equal(isProgressEvent(finished), true);
	assert.equal(isProgressEvent(done), false);
	assert.equal(isProgressEvent(suspended), false);
	assert.equal(AGENT_HARD_TIMEOUT_MS, 180_000);
	assert.equal(AGENT_NO_PROGRESS_TIMEOUT_MS, 45_000);
});

test('send params target the native agent method', () => {
	assert.deepEqual(sendParams('hello'), {
		method: 'agent.send_message',
		params: { text: 'hello' }
	});
});

test('edit and append tools count as native mutations', () => {
	for (const name of [
		'filesystem.edit',
		'filesystem.edit_user_file',
		'filesystem.edit_file',
		'filesystem.replace_user_file',
		'filesystem.append_user_file',
		'filesystem.append_file',
		'filesystem.create_user_file',
		'filesystem.patch'
	]) {
		assert.equal(isNativeMutation(name), true, name);
	}
	assert.equal(isNativeMutation('filesystem.read'), false);
	assert.equal(isNativeMutation('system.time'), false);
});

test('native receipt summary treats an edit retry as recovered', () => {
	const event = parseAgentTurnEvent('agent.turn_done', {
		text: 'updated',
		tool_steps: [
			{
				id: 'wrong',
				name: 'filesystem.create_user_file',
				status: 'failed',
				ok: false,
				error: 'replacement block occurs 0 times'
			},
			{
				id: 'right',
				name: 'filesystem.edit_user_file',
				status: 'success',
				ok: true,
				output: { path: '/tmp/home/Escritorio/note.txt', updated: true }
			}
		]
	});
	assert.equal(event?.kind, 'done');
	if (event?.kind !== 'done') throw new Error('unreachable');
	assert.equal(summarizeNativeToolSteps(event.done.toolSteps), 'recovered');
});
