import test from 'node:test';
import assert from 'node:assert/strict';

import {
	buildGeminiTranscriptionRequest,
	formatGeminiSttError,
	GeminiSttTransport,
	normalizeAudioMimeType,
	type GeminiSttClient,
	type GeminiTranscriptionRequest
} from './gemini-stt.ts';

const audio = new Blob(['fake-audio'], { type: 'audio/webm;codecs=opus' });

test('builds the dedicated Gemini Transcribe interaction request', () => {
	const request = buildGeminiTranscriptionRequest({}, 'https://generativelanguage.googleapis.com/v1beta/files/test', 'audio/webm');

	assert.equal(request.model, 'gemini-3.5-transcribe');
	assert.deepEqual(request.generation_config?.transcription_config, {
		language_codes: [],
		mode: 'smart'
	});
	assert.deepEqual(request.input, [
		{
			type: 'audio',
			uri: 'https://generativelanguage.googleapis.com/v1beta/files/test',
			mime_type: 'audio/webm'
		}
	]);
});

test('normalizes codec parameters without changing the media type', () => {
	assert.equal(normalizeAudioMimeType('audio/webm;codecs=opus'), 'audio/webm');
	assert.equal(normalizeAudioMimeType('audio/webm'), 'audio/webm');
	assert.equal(normalizeAudioMimeType('audio/mp4'), 'audio/mp4');
	assert.equal(normalizeAudioMimeType('audio/ogg;codecs=opus'), 'audio/ogg');
});

test('maps common Gemini failures to concise user-facing messages', () => {
	const invalidKey = Object.assign(new Error('authentication failed'), { status: 401 });
	const quota = Object.assign(new Error('RESOURCE_EXHAUSTED'), { status: 429 });

	assert.equal(formatGeminiSttError(invalidKey), 'Invalid Gemini API key.');
	assert.equal(formatGeminiSttError(quota), 'Gemini transcription quota exceeded.');
	assert.equal(formatGeminiSttError(new TypeError('Failed to fetch')), 'Could not reach Gemini.');
});

interface MockGeminiClient {
	client: GeminiSttClient;
	uploads: Array<Parameters<GeminiSttClient['files']['upload']>[0]>;
	requests: GeminiTranscriptionRequest[];
	requestSignals: Array<AbortSignal | undefined>;
	deletions: string[];
}

function createMockClient(
	outputText: string | undefined,
	options: { uploadUri?: string; failInteraction?: boolean } = {}
): MockGeminiClient {
	const uploads: Array<Parameters<GeminiSttClient['files']['upload']>[0]> = [];
	const requests: GeminiTranscriptionRequest[] = [];
	const requestSignals: Array<AbortSignal | undefined> = [];
	const deletions: string[] = [];
	const client: GeminiSttClient = {
		files: {
			upload: async (params) => {
				uploads.push(params);
				return {
					name: 'files/recording-123',
					uri: options.uploadUri ?? 'https://generativelanguage.googleapis.com/files/recording-123',
					mimeType: 'audio/webm'
				};
			},
			delete: async ({ name }) => {
				deletions.push(name);
			}
		},
		interactions: {
			create: async (request, requestOptions) => {
				requests.push(request);
				requestSignals.push(requestOptions?.signal);
				if (options.failInteraction) throw new Error('transcription failed');
				return { output_text: outputText };
			}
		}
	};
	return { client, uploads, requests, requestSignals, deletions };
}

test('uploads, transcribes, and deletes the Gemini file on success', async () => {
	const mock = createMockClient(' Hello from Gemini! ');
	const transport = new GeminiSttTransport({ apiKey: 'test-key' }, () => mock.client);
	const controller = new AbortController();
	const stages: string[] = [];

	const result = await transport.transcribe(audio, {
		filename: 'recording.webm',
		signal: controller.signal,
		onStage: (stage) => stages.push(stage)
	});

	assert.equal(result, 'Hello from Gemini!');
	assert.equal(mock.uploads.length, 1);
	assert.equal(mock.uploads[0].file, audio);
	assert.equal(mock.uploads[0].config?.mimeType, 'audio/webm');
	assert.equal(mock.uploads[0].config?.displayName, 'recording.webm');
	assert.equal(mock.requests.length, 1);
	assert.equal(mock.requests[0].model, 'gemini-3.5-transcribe');
	assert.equal(mock.requestSignals[0], controller.signal);
	assert.deepEqual(mock.deletions, ['files/recording-123']);
	assert.deepEqual(stages, [
		'upload-start',
		'upload-success',
		'transcription-start',
		'transcription-success'
	]);
});

test('deletes the uploaded file when transcription fails', async () => {
	const mock = createMockClient(undefined, { failInteraction: true });
	const transport = new GeminiSttTransport({ apiKey: 'test-key' }, () => mock.client);

	await assert.rejects(
		transport.transcribe(audio, { filename: 'recording.webm', signal: new AbortController().signal }),
		/Gemini transcription failed: transcription failed/
	);
	assert.deepEqual(mock.deletions, ['files/recording-123']);
});

test('does not accept an empty transcription as chat input', async () => {
	const mock = createMockClient('   ');
	const transport = new GeminiSttTransport({ apiKey: 'test-key' }, () => mock.client);

	await assert.rejects(
		transport.transcribe(audio, { filename: 'recording.webm', signal: new AbortController().signal }),
		/Gemini returned an empty transcription/
	);
	assert.deepEqual(mock.deletions, ['files/recording-123']);
});

test('aborted transcription does not start the interaction request', async () => {
	const controller = new AbortController();
	const mock = createMockClient('should not be returned');
	mock.client.files.upload = async (params) => {
		mock.uploads.push(params);
		controller.abort();
		return {
			name: 'files/recording-123',
			uri: 'https://generativelanguage.googleapis.com/files/recording-123',
			mimeType: 'audio/webm'
		};
	};
	const transport = new GeminiSttTransport({ apiKey: 'test-key' }, () => mock.client);

	await assert.rejects(
		transport.transcribe(audio, { filename: 'recording.webm', signal: controller.signal }),
		(error: unknown) => error instanceof Error && error.name === 'AbortError'
	);
	assert.equal(mock.requests.length, 0);
	assert.deepEqual(mock.deletions, ['files/recording-123']);
});
