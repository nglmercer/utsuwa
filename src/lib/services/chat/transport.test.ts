import test from 'node:test';
import assert from 'node:assert/strict';

import { selectCompanionTransport } from './transport.ts';

test('native bridge routes companion chat to AgentRuntime', () => {
	assert.equal(
		selectCompanionTransport({
			nativeHostAvailable: true,
			nativeBuildExpected: false,
			localProvider: true
		}),
		'native-agent'
	);
});

test('a packaged build with no bridge fails instead of falling back', () => {
	assert.throws(
		() =>
			selectCompanionTransport({
				nativeHostAvailable: false,
				nativeBuildExpected: true,
				localProvider: true
			}),
		/Native host runtime was expected.*IPC bridge is unavailable/
	);
});

test('browser local providers use direct provider transport', () => {
	assert.equal(
		selectCompanionTransport({
			nativeHostAvailable: false,
			nativeBuildExpected: false,
			localProvider: true
		}),
		'direct'
	);
});

test('browser cloud providers use the server route transport', () => {
	assert.equal(
		selectCompanionTransport({
			nativeHostAvailable: false,
			nativeBuildExpected: false,
			localProvider: false
		}),
		'server'
	);
});
