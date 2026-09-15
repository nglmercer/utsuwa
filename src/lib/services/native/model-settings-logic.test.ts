import test from 'node:test';
import assert from 'node:assert/strict';
import {
	buildNativeModelProviderParams,
	normalizeNativeBaseUrl
} from './model-settings-logic.ts';

test('native LM Studio settings use the shared /v1 base URL', () => {
	assert.equal(
		normalizeNativeBaseUrl('lmstudio', 'http://localhost:1234'),
		'http://localhost:1234/v1'
	);
	assert.equal(
		normalizeNativeBaseUrl('lmstudio', 'http://localhost:1234/v1/chat/completions'),
		'http://localhost:1234/v1'
	);
});

test('native Ollama settings are idempotent and custom paths are preserved', () => {
	assert.equal(
		normalizeNativeBaseUrl('ollama', 'http://localhost:11434/'),
		'http://localhost:11434/v1'
	);
	assert.equal(
		normalizeNativeBaseUrl('openai-compatible', 'http://localhost:9000/custom'),
		'http://localhost:9000/custom'
	);
});

test('native Kilo settings preserve the gateway base without adding /v1', () => {
	assert.equal(
		normalizeNativeBaseUrl('kilo', 'https://api.kilo.ai/api/gateway/'),
		'https://api.kilo.ai/api/gateway'
	);
});

test('native settings payload sends the normalized provider base URL', () => {
	assert.deepEqual(
		buildNativeModelProviderParams({
			provider: 'lmstudio',
			baseUrl: 'http://localhost:1234',
			model: 'qwen-tool',
			apiKey: ''
		}),
		{
			provider: 'lmstudio',
			base_url: 'http://localhost:1234/v1',
			model: 'qwen-tool',
			api_key: ''
		}
	);
});

test('native settings payload carries the vision classification when present', () => {
	assert.deepEqual(
		buildNativeModelProviderParams({
			provider: 'ollama',
			baseUrl: 'http://localhost:11434',
			model: 'llava:13b',
			vision: true
		}),
		{
			provider: 'ollama',
			base_url: 'http://localhost:11434/v1',
			model: 'llava:13b',
			vision: true
		}
	);
	const without = buildNativeModelProviderParams({
		provider: 'ollama',
		baseUrl: 'http://localhost:11434',
		model: 'llama3.1:8b'
	});
	assert.ok(!('vision' in without));
});
