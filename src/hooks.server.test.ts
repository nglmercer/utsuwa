import test from 'node:test';
import assert from 'node:assert/strict';

import { handle, isSameOriginRequest } from './hooks.server.ts';

function post(url: string, origin: string | null): Request {
	const headers: Record<string, string> = {};
	if (origin !== null) headers.origin = origin;
	return new Request(url, { method: 'POST', headers });
}

test('same-origin check allows missing origins and validates present ones', () => {
	const url = new URL('https://app.example/api/mcp/call');
	assert.equal(isSameOriginRequest(post(url.href, null), url), true);
	assert.equal(isSameOriginRequest(post(url.href, 'https://app.example'), url), true);
	assert.equal(isSameOriginRequest(post(url.href, 'https://evil.example'), url), false);
	assert.equal(isSameOriginRequest(post(url.href, 'https://app.example:8443'), url), false);
	assert.equal(isSameOriginRequest(post(url.href, 'http://app.example'), url), false);
	assert.equal(isSameOriginRequest(post(url.href, 'null'), url), false);
	assert.equal(isSameOriginRequest(post(url.href, 'not a url'), url), false);
});

test('handle rejects cross-origin API mutations with 403', async () => {
	const url = new URL('http://localhost:5173/api/mcp/call');
	const event = { request: post(url.href, 'https://evil.example'), url } as never;
	const response = await handle({
		event,
		resolve: async () => new Response('reached')
	} as never);
	assert.equal(response.status, 403);
	assert.match(await response.text(), /Cross-origin/);
});

test('handle lets same-origin API mutations through', async () => {
	const url = new URL('http://localhost:5173/api/mcp/call');
	const event = { request: post(url.href, 'http://localhost:5173'), url } as never;
	const response = await handle({
		event,
		resolve: async () => new Response('reached')
	} as never);
	assert.equal(response.status, 200);
	assert.equal(await response.text(), 'reached');
});
