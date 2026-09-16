import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
	connectNativeMcpServer,
	getNativeMcpServers,
	getNativeMcpStatus,
	parseNativeMcpStatus,
	setNativeMcpServers,
	setNativeMcpServerToken,
	type NativeInvoke
} from './mcp.ts';

function stubInvoke(handlers: Record<string, (params?: Record<string, unknown>) => unknown>): NativeInvoke {
	const calls: Array<{ method: string; params?: Record<string, unknown> }> = [];
	const invoke = (async (method: string, params?: Record<string, unknown>) => {
		calls.push({ method, params });
		const handler = handlers[method];
		if (!handler) throw { message: `unknown method ${method}` };
		return handler(params);
	}) as NativeInvoke & { calls: typeof calls };
	invoke.calls = calls;
	return invoke;
}

describe('getNativeMcpServers', () => {
	it('translates canonical Rust configs to the flat UI shape', async () => {
		const invoke = stubInvoke({
			'settings.get': () => ({
				value: [
					{
						id: 'ha',
						name: 'Home',
						transport: { type: 'http', url: 'https://ha.example.com/mcp' },
						enabled: true,
						trust: 'Untrusted'
					},
					{
						id: 'fs',
						transport: {
							type: 'stdio',
							command: 'npx',
							args: ['-y', 'x'],
							extra_env: { A: 'b' }
						},
						enabled: false,
						trust: 'Limited'
					}
				]
			})
		});
		const servers = await getNativeMcpServers(invoke);
		assert.equal(servers.length, 2);
		assert.deepEqual(servers[0], {
			transport: 'http',
			id: 'ha',
			name: 'Home',
			url: 'https://ha.example.com/mcp',
			enabled: true
		});
		assert.deepEqual(servers[1], {
			transport: 'stdio',
			id: 'fs',
			command: 'npx',
			args: ['-y', 'x'],
			env: { A: 'b' },
			enabled: false
		});
	});

	it('returns [] for missing settings and surfaces bridge errors', async () => {
		const empty = stubInvoke({ 'settings.get': () => ({ value: null }) });
		assert.deepEqual(await getNativeMcpServers(empty), []);

		const failing = stubInvoke({
			'settings.get': () => {
				throw { message: 'host down' };
			}
		});
		await assert.rejects(() => getNativeMcpServers(failing), /host down/);
	});
});

describe('setNativeMcpServers', () => {
	it('writes the canonical Rust shape without tokens', async () => {
		const invoke = stubInvoke({ 'settings.set': () => ({ ok: true }) });
		await setNativeMcpServers(invoke, [
			{
				transport: 'http',
				id: 'ha',
				name: 'Home',
				url: 'https://ha.example.com/mcp',
				bearerToken: 'must-not-persist',
				enabled: true
			}
		]);
		const call = (invoke as unknown as { calls: Array<{ params?: Record<string, unknown> }> }).calls[0];
		assert.equal(call.params?.key, 'mcp.servers');
		const value = call.params?.value as Array<Record<string, unknown>>;
		assert.deepEqual(value, [
			{
				id: 'ha',
				name: 'Home',
				enabled: true,
				trust: 'Untrusted',
				transport: { type: 'http', url: 'https://ha.example.com/mcp' }
			}
		]);
		assert.ok(!JSON.stringify(call.params).includes('must-not-persist'));
	});
});

describe('parseNativeMcpStatus + getNativeMcpStatus', () => {
	it('parses status rows and drops garbage', () => {
		assert.deepEqual(parseNativeMcpStatus({
			id: 'ha',
			name: 'Home',
			transport: 'http',
			enabled: true,
			connected: true,
			tools: 3,
			has_token: true
		}), {
			id: 'ha',
			name: 'Home',
			transport: 'http',
			enabled: true,
			connected: true,
			tools: 3,
			has_token: true
		});
		assert.equal(parseNativeMcpStatus(null), null);
		assert.equal(parseNativeMcpStatus({}), null);
		assert.equal(parseNativeMcpStatus({ id: 42 }), null);
		const failed = parseNativeMcpStatus({ id: 'x', connected: false, last_error: 'y'.repeat(9999) });
		assert.equal(failed?.last_error?.length, 500);
	});

	it('returns parsed rows and rejects malformed answers', async () => {
		const invoke = stubInvoke({ 'mcp.status': () => [{ id: 'a' }, null, { id: 'b', tools: 2 }] });
		const rows = await getNativeMcpStatus(invoke);
		assert.deepEqual(
			rows.map((r) => r.id),
			['a', 'b']
		);
		const bad = stubInvoke({ 'mcp.status': () => ({}) });
		await assert.rejects(() => getNativeMcpStatus(bad), /malformed/);
	});
});

describe('connectNativeMcpServer + setNativeMcpServerToken', () => {
	it('returns tool names and token receipts', async () => {
		const invoke = stubInvoke({
			'mcp.connect': () => ({ tools: ['mcp.ha.turn_on'] }),
			'mcp.set_server_token': () => ({ ok: true, has_token: true })
		});
		assert.deepEqual(await connectNativeMcpServer(invoke, 'ha'), ['mcp.ha.turn_on']);
		assert.equal(await setNativeMcpServerToken(invoke, 'ha', 'tok'), true);

		const bad = stubInvoke({ 'mcp.connect': () => ({ tools: 'nope' }) });
		await assert.rejects(() => connectNativeMcpServer(bad, 'ha'), /malformed/);
	});
});
