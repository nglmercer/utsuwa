import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
	McpToolExecutor,
	capResultText,
	joinToolName,
	splitToolName,
	sanitizeToolNameSegment,
	toolResultToText,
	MAX_TOOL_RESULT_CHARS
} from './mcp-executor.ts';
import type { McpServerConfig } from './types.ts';

const HTTP_SERVER: McpServerConfig = {
	transport: 'http',
	id: 'home',
	name: 'Home',
	url: 'https://ha.example.com/mcp',
	enabled: true
};

/** Minimal fetch stub serving the /api/mcp proxy routes. */
function proxyFetch(
	tools: unknown[] = [{ name: 'get_weather', description: 'Get weather', inputSchema: { type: 'object' } }],
	callResult: unknown = { content: [{ type: 'text', text: 'sunny' }] }
) {
	return async (url: unknown, init?: { body?: unknown }) => {
		const path = String(url);
		const body = init?.body ? JSON.parse(String(init.body)) : {};
		if (path.endsWith('/api/mcp/tools')) {
			assert.equal(body.server.id, 'home');
			return Response.json({ tools, sessionId: 'sess-1' });
		}
		if (path.endsWith('/api/mcp/call')) {
			assert.equal(body.tool, 'get_weather');
			return Response.json({ result: callResult, sessionId: 'sess-1' });
		}
		return Response.json({ error: 'not found' }, { status: 404 });
	};
}

describe('tool naming', () => {
	it('joins and splits server__tool names', () => {
		assert.equal(joinToolName('home', 'get_weather'), 'home__get_weather');
		assert.deepEqual(splitToolName('home__get_weather'), { serverId: 'home', tool: 'get_weather' });
	});

	it('sanitizes segments to the provider charset', () => {
		assert.equal(sanitizeToolNameSegment('weird name!'), 'weird_name_');
		assert.equal(sanitizeToolNameSegment(''), 'tool');
		assert.equal(joinToolName('a b', 'c d').length <= 64, true);
	});

	it('splitToolName rejects malformed names', () => {
		assert.equal(splitToolName('noseparator'), null);
		assert.equal(splitToolName('__tool'), null);
		assert.equal(splitToolName('server__'), null);
	});
});

describe('toolResultToText + capResultText', () => {
	it('flattens text blocks and marks errors', () => {
		assert.equal(toolResultToText({ content: [{ type: 'text', text: 'hi' }] }), 'hi');
		assert.equal(
			toolResultToText({ content: [{ type: 'text', text: 'bad' }], isError: true }),
			'Tool error: bad'
		);
		assert.match(toolResultToText({ content: [{ type: 'image', mimeType: 'image/png' }] }), /omitted/);
	});

	it('caps long results with a truncation marker', () => {
		const long = 'x'.repeat(MAX_TOOL_RESULT_CHARS + 100);
		const capped = capResultText(long);
		assert.ok(capped.length <= MAX_TOOL_RESULT_CHARS + 40);
		assert.match(capped, /truncated/);
		assert.equal(capResultText('short'), 'short');
	});
});

describe('McpToolExecutor via proxy', () => {
	it('lists definitions from proxy tools', async () => {
		const ex = new McpToolExecutor([HTTP_SERVER], { mode: 'proxy', fetchImpl: proxyFetch() });
		const defs = await ex.definitions();
		assert.equal(defs.length, 1);
		assert.equal(defs[0].name, 'home__get_weather');
		assert.equal(defs[0].description, 'Get weather');
		assert.equal(ex.errors.size, 0);
	});

	it('executes a tool and returns capped text', async () => {
		const ex = new McpToolExecutor([HTTP_SERVER], { mode: 'proxy', fetchImpl: proxyFetch() });
		await ex.definitions();
		const text = await ex.execute('home__get_weather', JSON.stringify({ city: 'Oslo' }));
		assert.equal(text, 'sunny');
	});

	it('throws on unknown tool names, returns text on bad args', async () => {
		const ex = new McpToolExecutor([HTTP_SERVER], { mode: 'proxy', fetchImpl: proxyFetch() });
		await ex.definitions();
		await assert.rejects(() => ex.execute('home__nope', '{}'), /Unknown tool/);
		const bad = await ex.execute('home__get_weather', 'not json');
		assert.match(bad, /not valid JSON/);
		const arr = await ex.execute('home__get_weather', '[]');
		assert.match(arr, /must be a JSON object/);
	});

	it('never auto-executes confirm-list tools', async () => {
		let calls = 0;
		const ex = new McpToolExecutor([HTTP_SERVER], {
			mode: 'proxy',
			fetchImpl: (async (url: unknown, init?: { body?: unknown }) => {
				if (String(url).endsWith('/api/mcp/call')) calls++;
				return proxyFetch()(url, init);
			}),
			confirmTools: ['home__get_weather']
		});
		await ex.definitions();
		const text = await ex.execute('home__get_weather', '{}');
		assert.match(text, /requires confirmation/);
		assert.equal(calls, 0);
	});

	it('matches confirm list by bare tool name too', async () => {
		const ex = new McpToolExecutor([HTTP_SERVER], {
			mode: 'proxy',
			fetchImpl: proxyFetch(),
			confirmTools: ['get_weather']
		});
		await ex.definitions();
		assert.match(await ex.execute('home__get_weather', '{}'), /requires confirmation/);
	});

	it('skips disabled servers and records per-server errors', async () => {
		const failing: McpServerConfig = { ...HTTP_SERVER, id: 'down' };
		const ex = new McpToolExecutor([failing], {
			mode: 'proxy',
			fetchImpl: async () => Response.json({ error: 'boom' }, { status: 500 })
		});
		const defs = await ex.definitions();
		assert.equal(defs.length, 0);
		assert.match(ex.errors.get('down') ?? '', /boom/);

		const disabled: McpServerConfig = { ...HTTP_SERVER, id: 'off', enabled: false };
		const ex2 = new McpToolExecutor([disabled], { mode: 'proxy', fetchImpl: proxyFetch() });
		assert.equal((await ex2.definitions()).length, 0);
		assert.equal(ex2.errors.size, 0);
	});

	it('refuses stdio servers in direct mode', async () => {
		const stdio: McpServerConfig = { transport: 'stdio', id: 'local', command: 'x', enabled: true };
		const ex = new McpToolExecutor([stdio], { mode: 'direct', fetchImpl: proxyFetch() });
		assert.equal((await ex.definitions()).length, 0);
		assert.match(ex.errors.get('local') ?? '', /proxy/);
	});

	it('reports execution failures as text and records the error', async () => {
		const ex = new McpToolExecutor([HTTP_SERVER], {
			mode: 'proxy',
			fetchImpl: async (url: unknown) =>
				String(url).endsWith('/api/mcp/tools')
					? Response.json({ tools: [{ name: 'get_weather' }] })
					: Response.json({ error: 'kaput' }, { status: 500 })
		});
		await ex.definitions();
		const text = await ex.execute('home__get_weather', '{}');
		assert.match(text, /failed: .*kaput/);
		assert.match(ex.errors.get('home') ?? '', /kaput/);
	});
});
