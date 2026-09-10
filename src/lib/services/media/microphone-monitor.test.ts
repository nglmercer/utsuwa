import test from 'node:test';
import assert from 'node:assert/strict';

import { MicrophoneMonitor, normalizeMicrophoneLevel } from './microphone-monitor.ts';

test('normalizes microphone energy into a bounded meter value', () => {
	assert.equal(normalizeMicrophoneLevel(-1), 0);
	assert.equal(normalizeMicrophoneLevel(Number.NaN), 0);
	assert.equal(normalizeMicrophoneLevel(0.05), 0.3);
	assert.equal(normalizeMicrophoneLevel(1), 1);
});

test('reports unsupported capture without opening a stream', async () => {
	const states: string[] = [];
	const errors: string[] = [];
	const monitor = new MicrophoneMonitor({
		onStateChange: (state) => states.push(state),
		onError: (error) => errors.push(error.userMessage)
	});

	if (monitor.isSupported()) return;

	assert.equal(await monitor.start(), false);
	assert.deepEqual(states, ['idle', 'requesting', 'error']);
	assert.equal(errors.length, 1);
	assert.equal(monitor.getError()?.category, 'unsupported');
	monitor.stop();
	assert.equal(monitor.getState(), 'idle');
});
