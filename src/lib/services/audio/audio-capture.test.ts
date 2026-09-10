import test from 'node:test';
import assert from 'node:assert/strict';

import {
	NativeAudioCaptureBackend,
	createAudioCaptureBackend
} from './audio-capture.ts';

test('native backend consumes host events and fetches WAV media by handle', async () => {
	const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
	const previousFetch = globalThis.fetch;
	const eventTarget = new EventTarget();
	const invocations: Array<{ method: string; params: Record<string, unknown> }> = [];
	const bridge = {
		invoke: async (method: string, params: Record<string, unknown> = {}) => {
			invocations.push({ method, params });
			if (method === 'audio_capture.start') {
				return {
					capture_id: 'capture-1',
					backend: 'native-cpal',
					device: 'Test microphone',
					sample_rate: 48_000,
					channels: 2
				};
			}
			if (method === 'audio_capture.stop') {
				return {
					capture_id: 'capture-1',
					media_url: 'companion://media/capture-1',
					mime_type: 'audio/wav',
					duration_ms: 1_250,
					wav_bytes: 128,
					stats: {
						current_rms: 0.03,
						peak_rms: 0.31,
						noise_floor: 0.005,
						speech_threshold: 0.015,
						speech_candidate_active: false,
						speech_detected: true,
						silence_duration_ms: 1_000,
						chunk_count: 12,
						dropped_chunks: 0
					}
				};
			}
			throw new Error(`unexpected method: ${method}`);
		}
	};

	Object.defineProperty(globalThis, 'window', {
		configurable: true,
		value: Object.assign(eventTarget, { utsuwa: bridge })
	});
	globalThis.fetch = async (input: RequestInfo | URL) => {
		assert.equal(String(input), 'companion://media/capture-1');
		return new Response(new Blob(['RIFF test'], { type: 'audio/wav' }), {
			status: 200,
			headers: { 'Content-Type': 'audio/wav' }
		});
	};

	try {
		const backend = createAudioCaptureBackend();
		assert.ok(backend instanceof NativeAudioCaptureBackend);
		const levels: Array<[number, number]> = [];
		const stopped: string[] = [];
		await backend.start({
			autoStop: true,
			onAudioLevel: (rms, peakRms) => levels.push([rms, peakRms]),
			onStopped: (reason) => stopped.push(reason)
		});

		assert.equal(backend.getInfo()?.device, 'Test microphone');
		assert.equal(invocations[0]?.method, 'audio_capture.start');
		assert.deepEqual(invocations[0]?.params.config, {
			auto_stop: true,
			silence_duration_ms: 1_000,
			max_duration_ms: 45_000,
			sample_rate: null
		});

		eventTarget.dispatchEvent(new CustomEvent('utsuwa-host-event', {
			detail: {
				event: 'audio.capture',
				data: { capture_id: 'capture-1', event: { type: 'audio_level', rms: 0.2, peak_rms: 0.31 } }
			}
		}));
		eventTarget.dispatchEvent(new CustomEvent('utsuwa-host-event', {
			detail: {
				event: 'audio.capture',
				data: { capture_id: 'capture-1', event: { type: 'speech_started' } }
			}
		}));
		eventTarget.dispatchEvent(new CustomEvent('utsuwa-host-event', {
			detail: {
				event: 'audio.capture',
				data: {
					capture_id: 'capture-1',
					event: { type: 'stopped', reason: 'silence_detected' }
				}
			}
		}));

		assert.deepEqual(levels, [[0.2, 0.31]]);
		assert.deepEqual(stopped, ['silence-detected']);

		const blob = await backend.stop();
		assert.equal(blob.type, 'audio/wav');
		assert.equal(blob.size, 9);
		assert.equal(invocations[1]?.method, 'audio_capture.stop');
		assert.equal(backend.getDiagnostics().wavBytes, 128);
		assert.equal(backend.getDiagnostics().speechDetected, true);
		assert.equal(backend.getDiagnostics().durationMs, 1_250);
	} finally {
		globalThis.fetch = previousFetch;
		if (previousWindow) Object.defineProperty(globalThis, 'window', previousWindow);
		else Reflect.deleteProperty(globalThis, 'window');
	}
});
