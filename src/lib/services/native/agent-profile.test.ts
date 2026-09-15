import test from 'node:test';
import assert from 'node:assert/strict';

import {
	AGENT_TOOL_PROFILES,
	AGENT_TOOL_PROFILE_DESCRIPTIONS,
	getAgentToolProfile,
	isAgentToolProfile,
	setAgentToolProfile
} from './agent-profile.ts';

function stubInvoke(handler: (method: string, params?: Record<string, unknown>) => unknown) {
	const calls: Array<{ method: string; params?: Record<string, unknown> }> = [];
	const invoke = async (method: string, params?: Record<string, unknown>) => {
		calls.push({ method, params });
		return handler(method, params);
	};
	return { invoke, calls };
}

test('every profile has a description for the settings dropdown', () => {
	for (const profile of AGENT_TOOL_PROFILES) {
		assert.ok(
			AGENT_TOOL_PROFILE_DESCRIPTIONS[profile]?.length > 0,
			`${profile} needs a description`
		);
	}
});

test('getAgentToolProfile reads through settings.get and normalizes', async () => {
	const { invoke, calls } = stubInvoke(() => ({ value: 'Developer' }));
	assert.equal(await getAgentToolProfile(invoke), 'developer');
	assert.deepEqual(calls[0], {
		method: 'settings.get',
		params: { key: 'agent.tool_profile' }
	});

	const dashed = stubInvoke(() => ({ value: 'computer-use' }));
	assert.equal(await getAgentToolProfile(dashed.invoke), 'computeruse');

	const unset = stubInvoke(() => ({ value: null }));
	assert.equal(await getAgentToolProfile(unset.invoke), null);

	const unknown = stubInvoke(() => ({ value: 'turbo' }));
	assert.equal(await getAgentToolProfile(unknown.invoke), null);
});

test('getAgentToolProfile rejects malformed payloads', async () => {
	const { invoke } = stubInvoke(() => ({ nope: true }));
	await assert.rejects(() => getAgentToolProfile(invoke), /unexpected payload/);
});

test('setAgentToolProfile persists through settings.set', async () => {
	const { invoke, calls } = stubInvoke(() => ({ ok: true }));
	await setAgentToolProfile(invoke, 'simple');
	assert.deepEqual(calls[0], {
		method: 'settings.set',
		params: { key: 'agent.tool_profile', value: 'simple' }
	});
});

test('setAgentToolProfile rejects failed writes', async () => {
	const { invoke } = stubInvoke(() => ({ ok: false }));
	await assert.rejects(() => setAgentToolProfile(invoke, 'full'), /unexpected payload/);
});

test('isAgentToolProfile guards the dropdown input', () => {
	assert.equal(isAgentToolProfile('full'), true);
	assert.equal(isAgentToolProfile('SIMPLE'), true);
	assert.equal(isAgentToolProfile('turbo'), false);
	assert.equal(isAgentToolProfile(null), false);
});
