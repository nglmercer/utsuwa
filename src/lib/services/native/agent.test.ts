import test from 'node:test';
import assert from 'node:assert/strict';

import {
	initialAgentChatState,
	parseAgentTurnEvent,
	reduceAgentEvent,
	reduceAgentSend,
	sendParams,
	statusFor
} from './agent.ts';

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

test('send params target the native agent method', () => {
	assert.deepEqual(sendParams('hello'), {
		method: 'agent.send_message',
		params: { text: 'hello' }
	});
});
