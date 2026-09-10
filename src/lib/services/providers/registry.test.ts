import test from 'node:test';
import assert from 'node:assert/strict';

import {
	LLM_PROVIDERS,
	TTS_PROVIDERS,
	getLLMProvider,
	getSTTProvider,
	getTTSProvider,
	providerSupportsVision
} from './registry.ts';

test('Gemini Transcribe is registered as a dedicated STT provider', () => {
	const gemini = getSTTProvider('gemini-stt');

	assert.ok(gemini, 'Gemini STT should be registered');
	assert.equal(gemini?.name, 'Google Gemini');
	assert.equal(gemini?.description, 'Gemini 3.5 speech-to-text');
	assert.equal(gemini?.requiresApiKey, true);
	assert.equal(gemini?.defaultBaseUrl, 'https://generativelanguage.googleapis.com/');
	assert.deepEqual(gemini?.models, [{ id: 'gemini-3.5-transcribe', name: 'Gemini 3.5 Transcribe' }]);
});

test('Kilo is a registered optional-key public gateway', () => {
	const kilo = getLLMProvider('kilo');

	assert.ok(kilo, 'Kilo should be registered');
	assert.equal(kilo?.name, 'Kilo Gateway');
	assert.equal(kilo?.requiresApiKey, false);
	assert.equal(kilo?.authentication, 'optional');
	assert.equal(kilo?.defaultBaseUrl, 'https://api.kilo.ai/api/gateway');
	assert.deepEqual(kilo?.models ?? [], []);
});

test('local LLM providers rely on discovered installed models', () => {
	const localProviders = LLM_PROVIDERS.filter((provider) => provider.isLocal);

	assert.ok(localProviders.length > 0);
	for (const provider of localProviders) {
		assert.deepEqual(provider.models ?? [], [], `${provider.name} should not expose static model choices`);
	}
});

test('local TTS provider is keyless, local, and ships fallback voices', () => {
	const localTTS = getTTSProvider('local-tts');

	assert.ok(localTTS, 'local-tts should be registered');
	assert.equal(localTTS?.isLocal, true);
	assert.equal(localTTS?.requiresApiKey, false);
	assert.ok((localTTS?.voices?.length ?? 0) > 0, 'local-tts should seed voices for offline use');
});

test('OmniVoice provider is registered as keyless local proxy with preset voices', () => {
	const omnivoice = getTTSProvider('omnivoice');

	assert.ok(omnivoice, 'omnivoice should be registered');
	assert.equal(omnivoice?.isLocal, true);
	assert.equal(omnivoice?.requiresApiKey, false);
	assert.equal(omnivoice?.defaultBaseUrl, 'http://localhost:8881/v1/');
	assert.ok((omnivoice?.voices?.length ?? 0) > 0, 'omnivoice should expose preset voices');
	assert.ok(omnivoice?.voices?.some((v) => v.id === 'alloy'), 'alloy preset should exist');
});

test('every TTS provider declares whether it needs an API key', () => {
	for (const provider of TTS_PROVIDERS) {
		assert.equal(typeof provider.requiresApiKey, 'boolean', `${provider.name} must declare requiresApiKey`);
	}
});

test('vision-capable cloud providers are flagged; text-only and local are not', () => {
	assert.equal(providerSupportsVision('openai'), true);
	assert.equal(providerSupportsVision('anthropic'), true);
	assert.equal(providerSupportsVision('google'), true);
	assert.equal(providerSupportsVision('xai'), true);
	assert.equal(providerSupportsVision('deepseek'), false);
	assert.equal(providerSupportsVision('ollama'), false);
	assert.equal(providerSupportsVision('lmstudio'), false);
});
