import test from 'node:test';
import assert from 'node:assert/strict';

import { parseLMStudioModelCapabilities } from './model-capabilities.ts';
import {
	applyChatModelFilter,
	CatalogHttpError,
	extractHttpStatus,
	NON_CHAT_MODEL_PATTERN,
	normalizeModelName,
	parseAnthropicModels,
	parseElevenLabsModels,
	parseGoogleModels,
	parseLMStudioCatalog,
	parseOllamaTags,
	parseOpenAIListModels,
	parseOpenAITtsModels,
	resolveModelsBaseUrl,
	toCatalogFailure,
	withCompatibleToolCalling
} from './model-parsers.ts';

function chatModels(ids: string[]) {
	return ids.map((id) => ({ id, name: id }));
}

test('chat denylist hides non-chat families but keeps unknown future models', () => {
	const hidden = [
		'text-embedding-3-small',
		'whisper-1',
		'gpt-4o-mini-tts',
		'dall-e-3',
		'omni-moderation-latest',
		'gpt-4o-transcribe',
		'gpt-4o-realtime-preview'
	];
	for (const id of hidden) {
		assert.match(id, NON_CHAT_MODEL_PATTERN, `${id} should be hidden`);
	}
	const visible = [
		'gpt-4o',
		'gpt-99',
		'claude-opus-4-5',
		'gemini-3-pro',
		'grok-4',
		'deepseek-chat',
		'some-future-chat-model'
	];
	for (const id of visible) {
		assert.doesNotMatch(id, NON_CHAT_MODEL_PATTERN, `${id} should stay visible`);
	}
});

test('applyChatModelFilter skips TTS pickers, which scope their own lists', () => {
	const tts = chatModels(['gpt-4o-mini-tts']);
	assert.deepEqual(applyChatModelFilter(tts, 'openai-tts'), tts);
	assert.deepEqual(applyChatModelFilter(tts, 'elevenlabs'), tts);
	assert.deepEqual(applyChatModelFilter(chatModels(['gpt-4o', 'text-embedding-3-small']), 'openai'), [
		{ id: 'gpt-4o', name: 'gpt-4o' }
	]);
});

test('resolveModelsBaseUrl mirrors the previous per-path base selection', () => {
	assert.equal(resolveModelsBaseUrl('lmstudio', 'http://localhost:1234'), 'http://localhost:1234/v1');
	assert.equal(resolveModelsBaseUrl('ollama', 'http://localhost:11434/v1'), 'http://localhost:11434');
	assert.equal(resolveModelsBaseUrl('openai', 'https://api.openai.com/v1/'), 'https://api.openai.com/v1');
	assert.match(resolveModelsBaseUrl('kilo'), /kilo/);
});

test('parsers preserve the legacy direct-fetch shapes', () => {
	assert.deepEqual(parseOpenAIListModels({ data: [{ id: 'gpt-4o' }] }, 'openai'), [
		{ id: 'gpt-4o', name: 'GPT 4o' }
	]);
	assert.deepEqual(parseAnthropicModels({ data: [{ id: 'claude-opus-4-5-20251101' }] }), [
		{ id: 'claude-opus-4-5-20251101', name: normalizeModelName('claude-opus-4-5-20251101', 'anthropic') }
	]);
	assert.deepEqual(parseOllamaTags({ models: [{ name: 'llama3' }] }), [
		{
			id: 'llama3',
			name: 'llama3',
			capabilities: { toolCalling: true, toolCallingSupport: 'compatible' }
		}
	]);
	const llmRecord = { type: 'llm', key: 'lmstudio/qwen', display_name: 'Qwen' };
	assert.deepEqual(
		parseLMStudioCatalog({ models: [llmRecord, { type: 'embeddings', key: 'hidden-embed' }] }),
		[
			{
				id: 'lmstudio/qwen',
				name: 'Qwen',
				capabilities: parseLMStudioModelCapabilities(llmRecord)
			}
		]
	);
	assert.deepEqual(parseGoogleModels({ models: [{ name: 'models/gemini-2.0-flash' }] }), [
		{ id: 'gemini-2.0-flash', name: normalizeModelName('models/gemini-2.0-flash', 'google') }
	]);
	assert.deepEqual(
		parseElevenLabsModels([
			{ model_id: 'eleven-v3', name: 'Eleven v3', can_do_text_to_speech: true },
			{ model_id: 'scribe-v2', name: 'Scribe', can_do_text_to_speech: false }
		]),
		[{ id: 'eleven-v3', name: 'Eleven v3' }]
	);
	assert.deepEqual(parseOpenAITtsModels({ data: [{ id: 'gpt-4o-mini-tts' }, { id: 'gpt-4o' }] }), [
		{ id: 'gpt-4o-mini-tts', name: normalizeModelName('gpt-4o-mini-tts', 'openai-tts') }
	]);
});

test('withCompatibleToolCalling marks generic endpoints without dropping metadata', () => {
	const models = withCompatibleToolCalling([{ id: 'custom', name: 'Custom', capabilities: { vision: true } }]);
	assert.deepEqual(models, [
		{
			id: 'custom',
			name: 'Custom',
			capabilities: { vision: true, toolCalling: true, toolCallingSupport: 'compatible' }
		}
	]);
});

test('extractHttpStatus reads our own Rust and shared-helper formats', () => {
	assert.equal(extractHttpStatus('provider error 401: bad key'), 401);
	assert.equal(extractHttpStatus('Kilo rejected the request (HTTP 403). Check your key.'), 403);
	assert.equal(extractHttpStatus('Failed to fetch'), undefined);
	assert.equal(extractHttpStatus('transport error: connection refused'), undefined);
});

test('toCatalogFailure keeps local hints and attaches HTTP status', () => {
	const http = toCatalogFailure('openai', 'https://api.openai.com/v1', new CatalogHttpError(429, 'slow down'));
	assert.deepEqual(http, { models: [], error: 'slow down', status: 429 });
	const native = toCatalogFailure('openai', 'https://api.openai.com/v1', new Error('provider error 500: boom'));
	assert.equal(native.status, 500);
	const local = toCatalogFailure('ollama', 'http://localhost:11434', new Error('boom'));
	assert.equal(local.models.length, 0);
	assert.match(local.error, /Ollama|ollama/);
	assert.equal(local.status, undefined);
});
