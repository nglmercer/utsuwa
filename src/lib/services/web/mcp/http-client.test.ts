import test from 'node:test';
import assert from 'node:assert/strict';

import { McpHttpClient, toggleTrailingSlash, type FetchImpl } from './http-client.ts';
import { McpError } from '../../mcp/types.ts';

function jsonResponse(payload: unknown, headers: Record<string, string> = {}): Response {
	return new Response(JSON.stringify(payload), {
		status: 200,
		headers: { 'Content-Type': 'application/json', ...headers }
	});
}

function rpcResult(id: number | string, result: unknown): Record<string, unknown> {
	return { jsonrpc: '2.0', id, result };
}

/** Stub fetch recording calls and answering from a script. */
function stubFetch(script: (call: { url: string; init: RequestInit; body: Record<string, unknown> }) => Response) {
	const calls: Array<{ url: string; init: RequestInit; body: Record<string, unknown> }> = [];
	const fetchImpl: FetchImpl = async (url, init) => {
		const body = JSON.parse(String(init.body)) as Record<string, unknown>;
		const call = { url, init, body };
		calls.push(call);
		return script(call);
	};
	return { fetchImpl, calls };
}

const CONFIG = { transport: 'http' as const, id: 'ha', url: 'https://ha.example.com/mcp', bearerToken: 'sekret', enabled: true };

test('initialize captures the session id and sends the bearer token', async () => {
	const { fetchImpl, calls } = stubFetch(({ body }) => {
		if (body.method === 'initialize') {
			return jsonResponse(rpcResult(body.id as number, { protocolVersion: '2025-06-18' }), {
				'Mcp-Session-Id': 'sess-1'
			});
		}
		return new Response(null, { status: 202 });
	});
	const client = new McpHttpClient(CONFIG, { fetchImpl });
	await client.initialize();
	assert.equal(client.activeSessionId, 'sess-1');
	const headers = calls[0].init.headers as Record<string, string>;
	assert.equal(headers['Authorization'], 'Bearer sekret');
	assert.equal(headers['Accept'], 'application/json, text/event-stream');
	// initialize + notifications/initialized
	assert.equal(calls.length, 2);
	assert.equal(calls[1].body.method, 'notifications/initialized');
});

test('listTools resends the session id and returns tools', async () => {
	const { fetchImpl, calls } = stubFetch(({ body }) => {
		if (body.method === 'initialize') {
			return jsonResponse(rpcResult(body.id as number, {}), { 'Mcp-Session-Id': 'sess-9' });
		}
		if (body.method === 'tools/list') {
			return jsonResponse(rpcResult(body.id as number, { tools: [{ name: 'get_state', inputSchema: { type: 'object' } }] }));
		}
		return new Response(null, { status: 202 });
	});
	const client = new McpHttpClient(CONFIG, { fetchImpl });
	const tools = await client.listTools();
	assert.deepEqual(tools, [{ name: 'get_state', inputSchema: { type: 'object' } }]);
	const listCall = calls.find((c) => c.body.method === 'tools/list');
	assert.equal((listCall!.init.headers as Record<string, string>)['Mcp-Session-Id'], 'sess-9');
});

test('SSE answers are parsed and matched to the request id', async () => {
	const { fetchImpl } = stubFetch(({ body }) => {
		if (body.method === 'initialize') return jsonResponse(rpcResult(body.id as number, {}));
		const payload = `event: message\ndata: ${JSON.stringify(rpcResult(999, { ignored: true }))}\n\ndata: ${JSON.stringify(rpcResult(body.id as number, { content: [{ type: 'text', text: 'hi' }] }))}\n\n`;
		return new Response(payload, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
	});
	const client = new McpHttpClient(CONFIG, { fetchImpl });
	const result = await client.callTool('get_state', {});
	assert.deepEqual(result, { content: [{ type: 'text', text: 'hi' }] });
});

test('unknown session triggers one re-initialize and retry', async () => {
	let initializes = 0;
	const { fetchImpl, calls } = stubFetch(({ body, init }) => {
		if (body.method === 'initialize') {
			initializes++;
			return jsonResponse(rpcResult(body.id as number, {}), { 'Mcp-Session-Id': `sess-${initializes}` });
		}
		if (body.method === 'notifications/initialized') return new Response(null, { status: 202 });
		// First tools/list attempt uses the stale session -> 404; retry succeeds.
		const session = (init.headers as Record<string, string>)['Mcp-Session-Id'];
		if (session === 'sess-1') return new Response('gone', { status: 404 });
		return jsonResponse(rpcResult(body.id as number, { tools: [] }));
	});
	const client = new McpHttpClient(CONFIG, { fetchImpl });
	await client.initialize();
	assert.equal(client.activeSessionId, 'sess-1');
	const tools = await client.listTools();
	assert.deepEqual(tools, []);
	assert.equal(initializes, 2);
	assert.ok(calls.length >= 4);
});

test('trailing-slash variant is probed once on 404', async () => {
	const { fetchImpl } = stubFetch(({ url, body }) => {
		if (url === 'https://ha.example.com/mcp') return new Response('nope', { status: 404 });
		assert.equal(url, 'https://ha.example.com/mcp/');
		if (body.method === 'initialize') return jsonResponse(rpcResult(body.id as number, {}));
		return new Response(null, { status: 202 });
	});
	const client = new McpHttpClient(CONFIG, { fetchImpl });
	await client.initialize();
	assert.equal(client.activeUrl, 'https://ha.example.com/mcp/');
});

test('toggleTrailingSlash preserves queries and refuses the root path', () => {
	assert.equal(toggleTrailingSlash('https://x/mcp'), 'https://x/mcp/');
	assert.equal(toggleTrailingSlash('https://x/mcp/'), 'https://x/mcp');
	assert.equal(toggleTrailingSlash('https://x/mcp?a=1'), 'https://x/mcp/?a=1');
	assert.equal(toggleTrailingSlash('https://x/'), null);
	assert.equal(toggleTrailingSlash('not a url'), null);
});

test('RPC errors surface as McpError with kind rpc', async () => {
	const { fetchImpl } = stubFetch(({ body }) => {
		if (body.method === 'initialize') return jsonResponse(rpcResult(body.id as number, {}));
		return jsonResponse({ jsonrpc: '2.0', id: body.id, error: { code: -32602, message: 'bad args' } });
	});
	const client = new McpHttpClient(CONFIG, { fetchImpl });
	const error = await client.callTool('nope', {}).then(
		() => null,
		(err) => err as McpError
	);
	assert.ok(error instanceof McpError);
	assert.equal(error.kind, 'rpc');
	assert.equal(error.rpcCode, -32602);
	assert.equal(error.serverId, 'ha');
});

test('HTML answers and disabled servers fail fast with clear errors', async () => {
	const html = stubFetch(() => new Response('<html></html>', { status: 200, headers: { 'Content-Type': 'text/html' } }));
	const client = new McpHttpClient(CONFIG, { fetchImpl: html.fetchImpl });
	const error = await client.listTools().then(
		() => null,
		(err) => err as McpError
	);
	assert.ok(error instanceof McpError);
	assert.equal(error.kind, 'protocol');
	assert.match(error.message, /web page/);

	const disabled = new McpHttpClient({ ...CONFIG, enabled: false }, { fetchImpl: html.fetchImpl });
	const disabledError = await disabled.listTools().then(
		() => null,
		(err) => err as McpError
	);
	assert.equal(disabledError?.kind, 'disabled');
});

test('timeouts surface as McpError with kind timeout', async () => {
	const hanging: FetchImpl = async (_url, init) => {
		await new Promise((_, reject) => {
			init.signal?.addEventListener('abort', () => {
				const error = new Error('aborted');
				error.name = 'TimeoutError';
				reject(error);
			});
		});
		throw new Error('unreachable');
	};
	const client = new McpHttpClient(CONFIG, { fetchImpl: hanging, timeoutMs: 20 });
	const error = await client.listTools().then(
		() => null,
		(err) => err as McpError
	);
	assert.ok(error instanceof McpError);
	assert.equal(error.kind, 'timeout');
});
