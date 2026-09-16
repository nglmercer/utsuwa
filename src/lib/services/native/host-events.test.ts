import test from 'node:test';
import assert from 'node:assert/strict';

import { HOST_EVENT, isHostEvent } from './bridge.ts';
import { HOST_EVENTS, subscribeHostEvents } from './host-events.ts';

test('isHostEvent validates the full detail shape', () => {
	assert.equal(
		isHostEvent(new CustomEvent(HOST_EVENT, { detail: { event: 'a.b', data: {} } })),
		true
	);
	assert.equal(isHostEvent(new Event(HOST_EVENT)), false);
	assert.equal(isHostEvent(new CustomEvent(HOST_EVENT, { detail: null })), false);
	assert.equal(isHostEvent(new CustomEvent(HOST_EVENT, { detail: {} })), false);
	assert.equal(
		isHostEvent(new CustomEvent(HOST_EVENT, { detail: { event: 42, data: {} } })),
		false
	);
	assert.equal(
		isHostEvent(new CustomEvent(HOST_EVENT, { detail: { event: 'a.b' } })),
		false
	);
	assert.equal(
		isHostEvent(new CustomEvent(HOST_EVENT, { detail: { event: 'a.b', data: null } })),
		false
	);
	assert.equal(
		isHostEvent(new CustomEvent(HOST_EVENT, { detail: { event: 'a.b', data: 'nope' } })),
		false
	);
});

test('host event constants cover the consumed channel', () => {
	assert.equal(HOST_EVENTS.AGENT_TEXT_DELTA, 'agent.text_delta');
	assert.equal(HOST_EVENTS.AGENT_DONE, 'agent.turn_done');
	assert.equal(HOST_EVENTS.AGENT_SUSPENDED, 'agent.turn_suspended');
	assert.equal(HOST_EVENTS.TASK_STEP_COMPLETED, 'task.step_completed');
	assert.equal(HOST_EVENTS.TASK_TERMINAL, 'task.terminal');
	assert.equal(HOST_EVENTS.AVATAR_ROUTINE_REQUESTED, 'avatar.routine.requested');
	assert.equal(HOST_EVENTS.AVATAR_ROUTINE_COMPLETED, 'avatar.routine.completed');
});

function hostEvent(event: string, data: Record<string, unknown> = {}) {
	return new CustomEvent(HOST_EVENT, { detail: { event, data } });
}

test('subscribe parses, dispatches, and ignores the rest', () => {
	const target = new EventTarget();
	const seen: string[] = [];
	const off = subscribeHostEvents(
		(event, data) =>
			event === HOST_EVENTS.AGENT_DONE && typeof data.text === 'string' ? data.text : null,
		(text) => {
			seen.push(text);
		},
		target
	);
	target.dispatchEvent(hostEvent(HOST_EVENTS.AGENT_DONE, { text: 'hi' }));
	target.dispatchEvent(hostEvent(HOST_EVENTS.AGENT_DONE, { text: 42 }));
	target.dispatchEvent(hostEvent('something.else', { text: 'hi' }));
	target.dispatchEvent(new CustomEvent(HOST_EVENT, { detail: { event: 'broken' } }));
	target.dispatchEvent(new Event(HOST_EVENT));
	assert.deepEqual(seen, ['hi']);
	off();
	target.dispatchEvent(hostEvent(HOST_EVENTS.AGENT_DONE, { text: 'late' }));
	assert.deepEqual(seen, ['hi']);
});

test('subscribe without a target is a no-op unsubscribe', () => {
	const seen: string[] = [];
	const off = subscribeHostEvents(
		() => 'x',
		(value) => {
			seen.push(value);
		},
		null
	);
	off();
	assert.deepEqual(seen, []);
});
