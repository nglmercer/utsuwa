import test from 'node:test';
import assert from 'node:assert/strict';

import { createRedirectGuardedFetch } from './guarded-fetch.ts';

function redirectTo(location: string, status = 302): Response {
	return new Response('moved', { status, headers: { location } });
}

test('redirect targets are re-validated before fetching', async () => {
	const validated: string[] = [];
	const fetched: string[] = [];
	globalThis.fetch = (async (input: string | URL | Request) => {
		const url = String(input);
		fetched.push(url);
		if (url === 'https://provider.example/v1/models') return redirectTo('https://evil.example/x');
		return new Response('should never arrive');
	}) as typeof fetch;

	const guarded = createRedirectGuardedFetch(async (rawUrl: string) => {
		validated.push(rawUrl);
		if (rawUrl.includes('evil.example')) throw new Error('blocked');
	});
	await assert.rejects(guarded('https://provider.example/v1/models', {}), /blocked/);
	assert.deepEqual(validated, ['https://provider.example/v1/models', 'https://evil.example/x']);
	assert.deepEqual(fetched, ['https://provider.example/v1/models']);
});

test('authorization is dropped on cross-origin redirects', async () => {
	const seenAuth: (string | null)[] = [];
	globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
		seenAuth.push(new Headers(init?.headers).get('authorization'));
		if (String(input).includes('start.example')) return redirectTo('https://other.example/next');
		return new Response('ok');
	}) as typeof fetch;

	const guarded = createRedirectGuardedFetch(async () => {});
	const response = await guarded('https://start.example/a', {
		headers: { Authorization: 'Bearer [REDACTED]' }
	});
	assert.equal(response.status, 200);
	assert.deepEqual(seenAuth, ['Bearer [REDACTED]', null]);
});

test('post downgrades to get on 303 and loops fail closed', async () => {
	const methods: (string | undefined)[] = [];
	globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
		methods.push(init?.method);
		if (String(input).includes('downgrade')) return redirectTo('https://x.example/done', 303);
		if (String(input).includes('loop')) return redirectTo('https://x.example/loop');
		return new Response('ok');
	}) as typeof fetch;

	const guarded = createRedirectGuardedFetch(async () => {});
	await guarded('https://x.example/downgrade', { method: 'POST', body: 'hi' });
	assert.deepEqual(methods, ['POST', 'GET']);
	await assert.rejects(guarded('https://x.example/loop', {}), /too many redirects/);
});
