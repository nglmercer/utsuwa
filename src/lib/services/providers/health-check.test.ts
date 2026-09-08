import test from 'node:test';
import assert from 'node:assert/strict';
import {
	checkTTSProviderHealth,
	getTTSProviderHealth,
	subscribeTTSProviderHealth,
	checkLLMProviderHealth
} from './health-check.ts';

test('OmniVoice health check returns healthy when /health responds ok', async () => {
	const requests: string[] = [];
	globalThis.fetch = (input: string | URL | Request) => {
		requests.push(String(input));
		return Promise.resolve({ ok: true, status: 200 } as Response);
	};

	const status = await checkTTSProviderHealth('omnivoice', 'http://localhost:8880/v1/');

	assert.equal(status, 'healthy');
	assert.equal(requests.length, 1);
	assert.equal(requests[0], 'http://localhost:8880/health');
});

test('Local TTS health check returns healthy when /v1/audio/voices responds ok', async () => {
	const requests: string[] = [];
	globalThis.fetch = (input: string | URL | Request) => {
		requests.push(String(input));
		return Promise.resolve({ ok: true, status: 200 } as Response);
	};

	const status = await checkTTSProviderHealth('local-tts', 'http://localhost:8880/v1/');

	assert.equal(status, 'healthy');
	assert.equal(requests.length, 1);
	assert.equal(requests[0], 'http://localhost:8880/v1/audio/voices');
});

test('Health check returns unhealthy on connection error', async () => {
	globalThis.fetch = () => Promise.reject(new Error('Failed to fetch'));

	const status = await checkTTSProviderHealth('omnivoice', 'http://localhost:8880/v1/');

	assert.equal(status, 'unhealthy');
});

test('Health check returns unhealthy on non-ok response', async () => {
	globalThis.fetch = () => Promise.resolve({ ok: false, status: 503 } as Response);

	const status = await checkTTSProviderHealth('omnivoice', 'http://localhost:8880/v1/');

	assert.equal(status, 'unhealthy');
});

test('Non-local TTS providers return unknown without fetching', async () => {
	let fetchCalled = false;
	globalThis.fetch = () => {
		fetchCalled = true;
		return Promise.resolve({ ok: true, status: 200 } as Response);
	};

	const status = await checkTTSProviderHealth('elevenlabs');

	assert.equal(status, 'unknown');
	assert.equal(fetchCalled, false);
});

test('Health status is readable after check and notifies subscribers', async () => {
	globalThis.fetch = () => Promise.resolve({ ok: true, status: 200 } as Response);

	let notified = false;
	const unsubscribe = subscribeTTSProviderHealth(() => {
		notified = true;
	});

	await checkTTSProviderHealth('omnivoice', 'http://localhost:8880/v1/');

	assert.equal(getTTSProviderHealth('omnivoice', 'http://localhost:8880/v1/'), 'healthy');
	assert.equal(notified, true);

	unsubscribe();
});

test('LM Studio health check normalizes the host and reports model capabilities', async () => {
	const requests: string[] = [];
	globalThis.fetch = (input: string | URL | Request) => {
		requests.push(String(input));
		return Promise.resolve(
			new Response(
				JSON.stringify({
					models: [
						{
							key: 'qwen-tool',
							display_name: 'Qwen Tool',
							type: 'llm',
							capabilities: { vision: true, trained_for_tool_use: true }
						}
					]
				}),
				{ status: 200, headers: { 'Content-Type': 'application/json' } }
			)
		);
	};

	const health = await checkLLMProviderHealth(
		'lmstudio',
		undefined,
		'http://localhost:1234',
		'qwen-tool'
	);

	assert.deepEqual(requests, ['http://localhost:1234/api/v1/models']);
	assert.equal(health.reachable, true);
	assert.equal(health.endpointValid, true);
	assert.equal(health.modelAvailable, true);
	assert.equal(health.toolCalling, 'native');
	assert.equal(health.vision, true);
});

test('LM Studio health falls back to the legacy metadata endpoint', async () => {
	const requests: string[] = [];
	globalThis.fetch = (input: string | URL | Request) => {
		const url = String(input);
		requests.push(url);
		if (url.endsWith('/api/v1/models')) {
			return Promise.resolve(new Response('not found', { status: 404 }));
		}
		return Promise.resolve(
			new Response(JSON.stringify({ data: [{ id: 'legacy-model', capabilities: [] }] }), {
				status: 200,
				headers: { 'Content-Type': 'application/json' }
			})
		);
	};

	const health = await checkLLMProviderHealth(
		'lmstudio',
		undefined,
		'http://localhost:1235/v1',
		'legacy-model'
	);

	assert.deepEqual(requests, [
		'http://localhost:1235/api/v1/models',
		'http://localhost:1235/api/v0/models'
	]);
	assert.equal(health.modelAvailable, true);
	assert.equal(health.toolCalling, 'unknown');
});
