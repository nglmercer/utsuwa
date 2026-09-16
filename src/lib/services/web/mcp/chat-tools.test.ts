import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
	clearMcpProxyCache,
	isMcpProxyAvailable,
	mergeConfirmTools,
	resolveMcpChatTools
} from './chat-tools.ts';
import type { McpServerConfig } from '../../mcp/types.ts';

const SERVER: McpServerConfig = {
	transport: 'http',
	id: 'home',
	url: 'https://ha.example.com/mcp',
	enabled: true
};

function proxyFetch(tools: unknown[] = [{ name: 't', description: 'T' }]) {
	return async (url: unknown) => {
		if (String(url).endsWith('/api/mcp/tools')) return Response.json({ tools });
		if (String(url).endsWith('/api/mcp/status')) return Response.json({ enabled: true });
		return Response.json({ error: 'not found' }, { status: 404 });
	};
}

describe('resolveMcpChatTools', () => {
	it('returns null when disabled, serverless, or proxyless', async () => {
		assert.equal(
			await resolveMcpChatTools({ enabled: false, servers: [SERVER], proxyAvailable: true }),
			null
		);
		assert.equal(
			await resolveMcpChatTools({ enabled: true, servers: [], proxyAvailable: true }),
			null
		);
		assert.equal(
			await resolveMcpChatTools({
				enabled: true,
				servers: [{ ...SERVER, enabled: false }],
				proxyAvailable: true
			}),
			null
		);
		assert.equal(
			await resolveMcpChatTools({ enabled: true, servers: [SERVER], proxyAvailable: false }),
			null
		);
	});

	it('primes an executor with definitions when active', async () => {
		const tools = await resolveMcpChatTools({
			enabled: true,
			servers: [SERVER],
			proxyAvailable: true,
			fetchImpl: proxyFetch()
		});
		assert.ok(tools);
		assert.equal(tools.definitions.length, 1);
		assert.equal(tools.definitions[0].name, 'home__t');
		assert.match(await tools.executor.execute('home__t', '{}'), /failed/);
	});

	it('returns null when no server yields tools', async () => {
		const tools = await resolveMcpChatTools({
			enabled: true,
			servers: [SERVER],
			proxyAvailable: true,
			fetchImpl: proxyFetch([])
		});
		assert.equal(tools, null);
	});

	it('passes the confirm list through to the executor', async () => {
		const tools = await resolveMcpChatTools({
			enabled: true,
			servers: [SERVER],
			proxyAvailable: true,
			confirmTools: ['home__t'],
			fetchImpl: proxyFetch()
		});
		assert.ok(tools);
		assert.match(await tools.executor.execute('home__t', '{}'), /requires confirmation/);
	});
});

describe('mergeConfirmTools', () => {
	it('merges confirm lists without duplicates', () => {
		assert.deepEqual(mergeConfirmTools(['a', 'b'], ['b', 'c'], undefined), ['a', 'b', 'c']);
		assert.deepEqual(mergeConfirmTools(undefined), []);
	});
});

describe('isMcpProxyAvailable', () => {
	it('reads and caches the status endpoint', async () => {
		clearMcpProxyCache();
		let calls = 0;
		const fetchImpl = async () => {
			calls++;
			return Response.json({ enabled: true });
		};
		assert.equal(await isMcpProxyAvailable(fetchImpl, 60_000, 1000), true);
		assert.equal(await isMcpProxyAvailable(fetchImpl, 60_000, 2000), true);
		assert.equal(calls, 1);
		assert.equal(await isMcpProxyAvailable(fetchImpl, 60_000, 1000 + 60_001), true);
		assert.equal(calls, 2);
		clearMcpProxyCache();
	});

	it('fails closed on errors and non-OK statuses', async () => {
		clearMcpProxyCache();
		assert.equal(
			await isMcpProxyAvailable(async () => {
				throw new Error('down');
			}),
			false
		);
		clearMcpProxyCache();
		assert.equal(await isMcpProxyAvailable(async () => new Response('x', { status: 500 })), false);
		clearMcpProxyCache();
		assert.equal(await isMcpProxyAvailable(async () => Response.json({ enabled: false })), false);
		clearMcpProxyCache();
	});
});
