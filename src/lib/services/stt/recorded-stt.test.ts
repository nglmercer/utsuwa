import test from 'node:test';
import assert from 'node:assert/strict';

import {
	formatNoSpeechMessage,
	formatRecordedSttResultError,
	getAudioExtension,
	RecordedSttService,
	resolveRecordedMimeType,
	type RecordedSttSessionResult,
	type RecordedSttTransport
} from './recorded-stt.ts';

test('derives recording filenames from the actual audio MIME type', () => {
	assert.equal(getAudioExtension('audio/webm;codecs=opus'), 'webm');
	assert.equal(getAudioExtension('audio/ogg;codecs=opus'), 'ogg');
	assert.equal(getAudioExtension('audio/mp4'), 'm4a');
	assert.equal(getAudioExtension('audio/mpeg'), 'mp3');
	assert.equal(getAudioExtension('audio/wav'), 'wav');
	assert.equal(getAudioExtension('audio/unknown'), 'webm');
});

test('resolves the actual recorder MIME before falling back to a requested type', () => {
	const webmChunk = new Blob(['chunk'], { type: 'audio/webm;codecs=opus' });
	const mp4Chunk = new Blob(['chunk'], { type: 'audio/mp4' });

	assert.equal(resolveRecordedMimeType('audio/webm;codecs=opus', [mp4Chunk], 'audio/mp4'), 'audio/webm;codecs=opus');
	assert.equal(resolveRecordedMimeType('', [mp4Chunk], 'audio/webm'), 'audio/mp4');
	assert.equal(resolveRecordedMimeType(undefined, [], 'audio/ogg;codecs=opus'), 'audio/ogg;codecs=opus');
	assert.equal(resolveRecordedMimeType(undefined, [], undefined), 'audio/webm');
	assert.equal(resolveRecordedMimeType(undefined, [webmChunk]), 'audio/webm;codecs=opus');
});

test('formats strong input with a VAD-specific no-speech explanation', () => {
	const message = formatNoSpeechMessage({
		vadEnabled: true,
		analyserAvailable: true,
		speechDetected: false,
		currentRms: 0,
		peakRms: 0.1,
		noiseFloor: 0.007,
		speechThreshold: 0.015,
		speechCandidateActive: false,
		silenceDurationMs: 0,
		chunkCount: 0,
		recordedBytes: 0,
		durationMs: 5_000,
		providerStarted: false,
		uploadStarted: false,
		transcriptionStarted: false
	});

	assert.match(message, /Microphone audio was detected/);
	assert.match(message, /Raw peak RMS: 0\.100/);
	assert.match(message, /Speech threshold: 0\.015/);
});

test('classifies recorder silence separately when the analyser saw a signal', () => {
	const message = formatRecordedSttResultError({
		status: 'recorder-empty',
		diagnostics: {
			vadEnabled: true,
			analyserAvailable: true,
			speechDetected: false,
			currentRms: 0,
			peakRms: 0.06,
			speechCandidateActive: false,
			silenceDurationMs: 0,
			chunkCount: 0,
			recordedBytes: 0,
			durationMs: 5_000,
			providerStarted: false,
			uploadStarted: false,
			transcriptionStarted: false
		}
	});

	assert.equal(message, 'Microphone audio was detected, but the recorder produced no audio data.');
});

class FakeMediaRecorder {
	static latest: FakeMediaRecorder | null = null;
	static isTypeSupported(type: string): boolean {
		return type === 'audio/webm;codecs=opus';
	}

	state: 'inactive' | 'recording' = 'inactive';
	mimeType: string;
	ondataavailable: ((event: BlobEvent) => void) | null = null;
	onstop: (() => void) | null = null;
	onerror: ((event: Event) => void) | null = null;

	constructor(_stream: MediaStream, options?: { mimeType?: string }) {
		this.mimeType = options?.mimeType ?? 'audio/webm';
		FakeMediaRecorder.latest = this;
	}

	start(): void {
		this.state = 'recording';
	}

	emitData(data: Blob): void {
		this.ondataavailable?.({ data } as BlobEvent);
	}

	stop(): void {
		if (this.state === 'inactive') return;
		this.state = 'inactive';
		this.onstop?.();
	}
}

function installCaptureMocks(): () => void {
	const previousNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
	const previousRecorder = Object.getOwnPropertyDescriptor(globalThis, 'MediaRecorder');
	const track = {
		onended: null as (() => void) | null,
		stop(): void {
			this.onended = null;
		}
	};
	const stream = {
		getTracks: () => [track]
	} as unknown as MediaStream;

	Object.defineProperty(globalThis, 'navigator', {
		configurable: true,
		value: { mediaDevices: { getUserMedia: async () => stream } }
	});
	Object.defineProperty(globalThis, 'MediaRecorder', {
		configurable: true,
		value: FakeMediaRecorder
	});

	return () => {
		if (previousNavigator) Object.defineProperty(globalThis, 'navigator', previousNavigator);
		else Reflect.deleteProperty(globalThis, 'navigator');
		if (previousRecorder) Object.defineProperty(globalThis, 'MediaRecorder', previousRecorder);
		else Reflect.deleteProperty(globalThis, 'MediaRecorder');
		FakeMediaRecorder.latest = null;
	};
}

function waitForMicrotasks(): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, 0));
}

function createCallbacks(results: RecordedSttSessionResult[], errors: string[]) {
	return {
		onResult: () => undefined,
		onEnd: (result?: RecordedSttSessionResult) => {
			if (result) results.push(result);
		},
		onError: (message: string) => errors.push(message)
	};
}

async function startManualSession(transport: RecordedSttTransport) {
	const restore = installCaptureMocks();
	const service = new RecordedSttService();
	service.configure(transport);
	const results: RecordedSttSessionResult[] = [];
	const errors: string[] = [];
	const callbacks = createCallbacks(results, errors);
	assert.equal(await service.startListening(callbacks, { autoStop: false }), true);
	return { restore, service, results, errors };
}

test('manual stop sends a non-empty recording without enabling VAD', async () => {
	let providerCalls = 0;
	const session = await startManualSession({
		transcribe: async (audio, context) => {
			providerCalls++;
			assert.equal(audio.type, 'audio/webm;codecs=opus');
			assert.equal(context.filename, 'recording.webm');
			return 'hello from the provider';
		}
	});

	try {
		FakeMediaRecorder.latest?.emitData(new Blob(['audio'], { type: 'audio/webm;codecs=opus' }));
		session.service.stopListening();
		await waitForMicrotasks();

		assert.equal(providerCalls, 1);
		assert.equal(session.results.length, 1);
		assert.equal(session.results[0].status, 'complete');
		assert.equal(session.results[0].diagnostics.stopReason, 'manual');
		assert.equal(session.results[0].diagnostics.vadEnabled, false);
		assert.equal(session.results[0].diagnostics.chunkCount, 1);
		assert.equal(session.results[0].diagnostics.recordedBytes, 5);
		assert.equal(session.errors.length, 0);
	} finally {
		session.restore();
	}
});

test('manual empty recorder data is a recorder failure, not no-speech', async () => {
	const session = await startManualSession({ transcribe: async () => 'must not run' });

	try {
		session.service.stopListening();
		await waitForMicrotasks();

		assert.equal(session.results.length, 0);
		assert.equal(session.errors[0], 'The recorder produced no audio data.');
		assert.equal(session.service.getLastSessionResult()?.status, 'recorder-empty');
	} finally {
		session.restore();
	}
});

test('provider empty output and provider errors remain distinct', async () => {
	const emptySession = await startManualSession({
		emptyResultMessage: 'Gemini returned an empty transcription.',
		transcribe: async () => ''
	});
	try {
		FakeMediaRecorder.latest?.emitData(new Blob(['audio'], { type: 'audio/webm' }));
		emptySession.service.stopListening();
		await waitForMicrotasks();
		assert.equal(emptySession.service.getLastSessionResult()?.status, 'provider-empty');
		assert.equal(emptySession.errors[0], 'Gemini returned an empty transcription.');
	} finally {
		emptySession.restore();
	}

	const errorSession = await startManualSession({
		transcribe: async () => {
			throw new Error('Invalid Gemini API key.');
		}
	});
	try {
		FakeMediaRecorder.latest?.emitData(new Blob(['audio'], { type: 'audio/webm' }));
		errorSession.service.stopListening();
		await waitForMicrotasks();
		assert.equal(errorSession.service.getLastSessionResult()?.status, 'provider-error');
		assert.equal(errorSession.errors[0], 'Invalid Gemini API key.');
	} finally {
		errorSession.restore();
	}
});

test('cancellation prevents an old provider response from reaching callbacks', async () => {
	let resolveProvider!: (text: string) => void;
	const session = await startManualSession({
		transcribe: async () => new Promise<string>((resolve) => {
			resolveProvider = resolve;
		})
	});

	try {
		FakeMediaRecorder.latest?.emitData(new Blob(['audio'], { type: 'audio/webm' }));
		session.service.stopListening();
		await waitForMicrotasks();
		session.service.abort();
		resolveProvider('late result');
		await waitForMicrotasks();

		assert.equal(session.results.length, 0);
		assert.equal(session.errors.length, 0);
	} finally {
		session.restore();
	}
});
