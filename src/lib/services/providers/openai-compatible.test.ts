import test from 'node:test';
import assert from 'node:assert/strict';

import {
	buildOpenAICompatibleUrl,
	describeModelListError,
	fetchOpenAICompatibleModels,
	hasApiKey,
	isFreeModel,
	openAICompatibleChatUrl,
	openAICompatibleModelsUrl,
	optionalBearerHeaders,
	parseOpenAICompatibleModels
} from './openai-compatible.ts';

test('optional bearer headers omit Authorization for missing, empty, and whitespace keys', () => {
	assert.equal(hasApiKey(undefined), false);
	assert.equal(hasApiKey(''), false);
	assert.equal(hasApiKey('   '), false);
	assert.deepEqual(optionalBearerHeaders(undefined), {});
	assert.deepEqual(optionalBearerHeaders(''), {});
	assert.deepEqual(optionalBearerHeaders('   '), {});
	assert.deepEqual(optionalBearerHeaders('  test-key  '), {
		Authorization: 'Bearer test-key'
	});
});

test('Kilo-compatible URLs preserve the configured gateway path', () => {
	const baseUrl = 'https://api.kilo.ai/api/gateway';
	assert.equal(buildOpenAICompatibleUrl(baseUrl, '/chat/completions'), `${baseUrl}/chat/completions`);
	assert.equal(openAICompatibleChatUrl(baseUrl), `${baseUrl}/chat/completions`);
	assert.equal(openAICompatibleModelsUrl(baseUrl), `${baseUrl}/models`);
	assert.equal(buildOpenAICompatibleUrl(`${baseUrl}/`, 'models'), `${baseUrl}/models`);
});

test('Kilo free-model classification prefers explicit metadata and pricing', () => {
	assert.equal(isFreeModel('kilo-auto/free', { isFree: true }), true);
	assert.equal(isFreeModel('kilo-auto/efficient', { isFree: false }), false);
	assert.equal(
		isFreeModel('provider/model', {
			pricing: { prompt: '0.000000000000', completion: '0' }
		}),
		true
	);
	assert.equal(isFreeModel('provider/model:free'), true);
	assert.equal(isFreeModel('provider/model', { pricing: { prompt: '-1', completion: '-1' } }), false);
});

test('Kilo model parsing marks free models, preserves names, and sorts free first', () => {
	const models = parseOpenAICompatibleModels(
		{
			data: [
				{
					id: 'kilo-auto/efficient',
					name: 'Auto Efficient',
					isFree: false
				},
				{
					id: 'stepfun/step-3.7-flash:free',
					name: 'StepFun Flash',
					isFree: true,
					supported_parameters: ['tools'],
					architecture: { input_modalities: ['text', 'image'] }
				}
			]
		},
		{ classifyFree: true, includeCapabilities: true }
	);

	assert.deepEqual(models.map((model) => model.id), [
		'stepfun/step-3.7-flash:free',
		'kilo-auto/efficient'
	]);
	assert.equal(models[0].name, 'StepFun Flash');
	assert.equal(models[0].free, true);
	assert.deepEqual(models[0].capabilities, {
		vision: true,
		toolCalling: true,
		toolCallingSupport: 'compatible'
	});
	assert.deepEqual(
		parseOpenAICompatibleModels(
			{ data: [{ id: 'paid/model', isFree: false }, { id: 'free/model', isFree: true }] },
			{ classifyFree: true, onlyFree: true }
		).map((model) => model.id),
		['free/model']
	);
});

test('shared model fetcher uses the exact Kilo models URL and optional auth', async () => {
	const originalFetch = globalThis.fetch;
	let requestUrl = '';
	let requestHeaders: Headers | undefined;
	globalThis.fetch = async (input, init) => {
		requestUrl = String(input);
		requestHeaders = new Headers(init?.headers);
		return new Response(JSON.stringify({ data: [{ id: 'kilo-auto/free', isFree: true }] }), {
			status: 200,
			headers: { 'Content-Type': 'application/json' }
		});
	};

	try {
		await fetchOpenAICompatibleModels(undefined, 'https://api.kilo.ai/api/gateway', 'kilo', {
			classifyFree: true,
			onlyFree: true
		});
		assert.equal(requestUrl, 'https://api.kilo.ai/api/gateway/models');
		assert.equal(requestHeaders?.has('Authorization'), false);

		await fetchOpenAICompatibleModels('  test-key  ', 'https://api.kilo.ai/api/gateway/', 'kilo', {
			classifyFree: true
		});
		assert.equal(requestUrl, 'https://api.kilo.ai/api/gateway/models');
		assert.equal(requestHeaders?.get('Authorization'), 'Bearer test-key');
	} finally {
		globalThis.fetch = originalFetch;
	}
});

test('model discovery errors distinguish Kilo rate limits from auth failures', async () => {
	assert.match(describeModelListError('kilo', 429, 'Too Many Requests'), /free-model rate limit/);
	assert.match(describeModelListError('kilo', 401, 'Unauthorized'), /Kilo API key/);
	assert.match(describeModelListError('kilo', 503, 'Service Unavailable'), /upstream provider/);

	const originalFetch = globalThis.fetch;
	globalThis.fetch = async () =>
		new Response(JSON.stringify({ error: { message: 'try later' } }), {
			status: 429,
			headers: { 'Content-Type': 'application/json' }
		});
	try {
		await assert.rejects(
			fetchOpenAICompatibleModels(undefined, 'https://api.kilo.ai/api/gateway', 'kilo'),
			/Kilo's free-model rate limit has been reached/
		);
	} finally {
		globalThis.fetch = originalFetch;
	}
});
