import test from 'node:test';
import assert from 'node:assert/strict';

import { isCommandAllowed, parseAllowedCommands, runStdioMethod } from './stdio.ts';
import { McpError } from '../../../lib/services/mcp/types.ts';

/** Minimal fake MCP server over newline-delimited JSON-RPC. */
const FAKE_SERVER = `
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (raw) => {
	const line = raw.trim();
	if (!line) return;
	const m = JSON.parse(line);
	if (m.id === undefined) return;
	let result = null;
	if (m.method === 'initialize') result = { protocolVersion: '2025-06-18' };
	else if (m.method === 'tools/list') result = { tools: [{ name: 'echo', inputSchema: { type: 'object' } }] };
	else if (m.method === 'tools/call') result = { content: [{ type: 'text', text: 'called:' + m.params.name }] };
	process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: m.id, result }) + '\\n');
});
`;

const stdioServer = (overrides = {}) => ({
	transport: 'stdio' as const,
	id: 'fake',
	command: 'node',
	args: ['-e', FAKE_SERVER],
	enabled: true,
	...overrides
});

test('stdio tools/list works through handshake + call', async () => {
	const result = await runStdioMethod<{ tools: Array<{ name: string }> }>(
		stdioServer(),
		'tools/list',
		{},
		{ allowedCommands: ['node'], timeoutMs: 15000 }
	);
	assert.deepEqual(result.tools, [{ name: 'echo', inputSchema: { type: 'object' } }]);
});

test('stdio tools/call returns the result payload', async () => {
	const result = await runStdioMethod<{ content: Array<{ text: string }> }>(
		stdioServer(),
		'tools/call',
		{ name: 'echo', arguments: {} },
		{ allowedCommands: ['node'], timeoutMs: 15000 }
	);
	assert.equal(result.content[0].text, 'called:echo');
});

test('disallowed commands are refused without spawning (fail-closed)', async () => {
	for (const allowed of [null, [], ['uvx']]) {
		const error = await runStdioMethod(stdioServer(), 'tools/list', {}, {
			allowedCommands: allowed,
			timeoutMs: 5000
		}).then(
			() => null,
			(err) => err as McpError
		);
		assert.ok(error instanceof McpError, `allowed=${JSON.stringify(allowed)}`);
		assert.equal(error.kind, 'forbidden');
		assert.match(error.message, /allowlist/);
	}
});

test('parseAllowedCommands is fail-closed on unset/empty', () => {
	assert.equal(parseAllowedCommands(undefined), null);
	assert.equal(parseAllowedCommands(''), null);
	assert.equal(parseAllowedCommands('  , '), null);
	assert.deepEqual(parseAllowedCommands('npx, uvx ,/usr/bin/python3'), ['npx', 'uvx', '/usr/bin/python3']);
});

test('allowlist matches exact commands and basenames', () => {
	assert.equal(isCommandAllowed('npx', ['npx']), true);
	assert.equal(isCommandAllowed('/usr/bin/npx', ['npx']), true);
	assert.equal(isCommandAllowed('/usr/bin/npx', ['/usr/bin/npx']), true);
	assert.equal(isCommandAllowed('evil', ['npx']), false);
	assert.equal(isCommandAllowed('npx-evil', ['npx']), false);
	assert.equal(isCommandAllowed('npx', null), false);
});
