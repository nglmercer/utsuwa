import test from 'node:test';
import assert from 'node:assert/strict';

import { parseMcpServerConfigs, McpError, toRustMcpConfig, fromRustMcpConfigs } from './types.ts';

test('valid http and stdio servers parse with trimmed ids', () => {
	const { servers, dropped } = parseMcpServerConfigs([
		{ transport: 'http', id: ' ha ', url: 'https://ha.example.com/mcp', bearerToken: 'tok', enabled: true },
		{ transport: 'stdio', id: 'files', command: 'npx', args: ['-y', 'x'], env: { A: 'b' }, enabled: false }
	]);
	assert.equal(dropped.length, 0);
	assert.equal(servers.length, 2);
	assert.equal(servers[0].id, 'ha');
	assert.equal((servers[0] as { bearerToken?: string }).bearerToken, 'tok');
	assert.equal(servers[1].enabled, false);
});

test('malformed servers drop with reasons instead of throwing', () => {
	const { servers, dropped } = parseMcpServerConfigs([
		null,
		{ transport: 'http', id: 'bad id!', url: 'https://x/mcp' },
		{ transport: 'http', id: 'nou', url: '' },
		{ transport: 'http', id: 'ftp', url: 'ftp://x/mcp' },
		{ transport: 'stdio', id: 'nocmd', command: '' },
		{ transport: 'grpc', id: 'nope' },
		'nope'
	]);
	assert.equal(servers.length, 0);
	assert.equal(dropped.length, 7);
});

test('non-array input yields an empty list', () => {
	assert.deepEqual(parseMcpServerConfigs(undefined), { servers: [], dropped: [] });
	assert.deepEqual(parseMcpServerConfigs({}), { servers: [], dropped: [] });
});

test('McpError carries server id and kind without secrets', () => {
	const error = new McpError('ha', 'rpc', 'boom', { status: 500, rpcCode: -32603 });
	assert.equal(error.serverId, 'ha');
	assert.equal(error.kind, 'rpc');
	assert.equal(error.status, 500);
	assert.equal(error.rpcCode, -32603);
	assert.ok(!error.message.includes('tok'));
});

test('toRustMcpConfig emits the canonical Rust shape without tokens', () => {
	const http = toRustMcpConfig({
		transport: 'http',
		id: 'ha',
		name: 'Home',
		url: 'https://ha.example.com/mcp',
		bearerToken: 'secret',
		enabled: true
	});
	assert.deepEqual(http, {
		id: 'ha',
		name: 'Home',
		enabled: true,
		trust: 'Untrusted',
		transport: { type: 'http', url: 'https://ha.example.com/mcp' }
	});
	const stdio = toRustMcpConfig({
		transport: 'stdio',
		id: 'fs',
		command: 'npx',
		args: ['-y', 'x'],
		env: { A: 'b' },
		enabled: false
	});
	assert.deepEqual(stdio, {
		id: 'fs',
		enabled: false,
		trust: 'Untrusted',
		transport: { type: 'stdio', command: 'npx', args: ['-y', 'x'], extra_env: { A: 'b' } }
	});
});

test('fromRustMcpConfigs parses canonical Rust configs and drops malformed rows', () => {
	const { servers, dropped } = fromRustMcpConfigs([
		{ id: 'ha', name: 'Home', transport: { type: 'http', url: 'https://x/mcp' }, enabled: true },
		{ id: 'fs', transport: { type: 'stdio', command: 'npx' }, enabled: false },
		{ id: 'bad id!', transport: { type: 'http', url: 'https://x' } },
		{ id: 'nope', transport: { type: 'websocket' } },
		null
	]);
	assert.equal(servers.length, 2);
	assert.equal(servers[0].transport, 'http');
	assert.equal(servers[1].transport, 'stdio');
	assert.equal(servers[1].enabled, false);
	assert.equal(dropped.length, 3);
});

test('Rust translators round-trip', () => {
	const { servers } = fromRustMcpConfigs([
		toRustMcpConfig({ transport: 'http', id: 'a', url: 'https://x/mcp', enabled: true }),
		toRustMcpConfig({ transport: 'stdio', id: 'b', command: 'uvx', enabled: true })
	]);
	assert.equal(servers.length, 2);
	assert.deepEqual(toRustMcpConfig(servers[0]).transport, { type: 'http', url: 'https://x/mcp' });
});
