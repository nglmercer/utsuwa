import test from 'node:test';
import assert from 'node:assert/strict';

import { parseMcpServerConfigs, McpError } from './types.ts';

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
