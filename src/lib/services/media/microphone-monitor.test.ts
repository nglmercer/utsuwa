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
	await monitor.stop();
	assert.equal(monitor.getState(), 'idle');
});

test('native hosts monitor microphone levels through CPAL without getUserMedia', async () => {
	const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
	const previousNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
	const eventTarget = new EventTarget();
	const invocations: Array<{ method: string; params: Record<string, unknown> }> = [];
	let getUserMediaCalls = 0;
	const bridge = {
		invoke: async (method: string, params: Record<string, unknown> = {}) => {
			invocations.push({ method, params });
			if (method === 'audio_capture.start') {
				return {
					capture_id: 'monitor-1',
					backend: 'native-cpal',
					device: 'Test microphone',
					sample_rate: 48_000,
					channels: 2
				};
			}
			if (method === 'audio_capture.cancel') return { ok: true };
			throw new Error(`unexpected method: ${method}`);
		}
	};

	Object.defineProperty(globalThis, 'window', {
		configurable: true,
		value: Object.assign(eventTarget, { utsuwa: bridge })
	});
	Object.defineProperty(globalThis, 'navigator', {
		configurable: true,
		value: {
			mediaDevices: {
				getUserMedia: async () => {
					getUserMediaCalls++;
					throw new Error('getUserMedia must not run in native mode');
				}
			}
		}
	});

	try {
		const levels: Array<[number, number]> = [];
		const states: string[] = [];
		const monitor = new MicrophoneMonitor({
			onLevel: (level) => levels.push([level, monitor.getCurrentRms()]),
			onStateChange: (state) => states.push(state)
		});

		assert.equal(monitor.isSupported(), true);
		assert.equal(await monitor.start(), true);
		assert.equal(invocations[0]?.method, 'audio_capture.start');
		assert.equal(
			(invocations[0]?.params.config as Record<string, unknown> | undefined)?.retain_audio,
			false
		);
		assert.equal(getUserMediaCalls, 0);
		assert.equal(monitor.getHasLevelMeter(), true);

		eventTarget.dispatchEvent(new CustomEvent('utsuwa-host-event', {
			detail: {
				event: 'audio.capture',
				data: { capture_id: 'monitor-1', event: { type: 'audio_level', rms: 0.1, peak_rms: 0.25 } }
			}
		}));

		assert.equal(monitor.getCurrentRms(), 0.1);
		assert.equal(monitor.getPeakRms(), 0.25);
		assert.equal(monitor.getLevel(), 0.6);
		await monitor.stop();
		assert.equal(invocations[1]?.method, 'audio_capture.cancel');
		assert.deepEqual(states, ['idle', 'requesting', 'monitoring', 'idle']);
	} finally {
		if (previousWindow) Object.defineProperty(globalThis, 'window', previousWindow);
		else Reflect.deleteProperty(globalThis, 'window');
		if (previousNavigator) Object.defineProperty(globalThis, 'navigator', previousNavigator);
		else Reflect.deleteProperty(globalThis, 'navigator');
	}
});
