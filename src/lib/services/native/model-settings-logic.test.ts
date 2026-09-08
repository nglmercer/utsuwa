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
