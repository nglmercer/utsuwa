import test from 'node:test';
import assert from 'node:assert/strict';

import {
	assertHttpHostAllowed,
	isHttpHostAllowed,
	parseHttpAllowedHosts
} from './shared.ts';
import { McpError } from '../../../lib/services/mcp/types.ts';

test('http host allowlist parsing trims, lowercases, and dedupes', () => {
	assert.deepEqual(parseHttpAllowedHosts(undefined), []);
	assert.deepEqual(parseHttpAllowedHosts(''), []);
	assert.deepEqual(parseHttpAllowedHosts('   '), []);
	assert.deepEqual(parseHttpAllowedHosts(' ha.example.com, HA.EXAMPLE.COM  mcp.local '), [
		'ha.example.com',
		'mcp.local'
	]);
});

test('empty allowlist permits any host; non-empty restricts to members', () => {
	assert.equal(isHttpHostAllowed('https://anything.example/x', []), true);
	const allowed = parseHttpAllowedHosts('ha.example.com');
	assert.equal(isHttpHostAllowed('https://ha.example.com/mcp', allowed), true);
	assert.equal(isHttpHostAllowed('https://HA.EXAMPLE.COM:8443/mcp', allowed), true);
	assert.equal(isHttpHostAllowed('https://ha.example.com./mcp', allowed), true);
	assert.equal(isHttpHostAllowed('https://evil-ha.example.com/', allowed), false);
	assert.equal(isHttpHostAllowed('https://ha.example.com.evil.com/', allowed), false);
	assert.equal(isHttpHostAllowed('https://example.com/', allowed), false);
	assert.equal(isHttpHostAllowed('not a url', allowed), false);
});

test('assertHttpHostAllowed throws a forbidden McpError off-list', () => {
	const server = {
		transport: 'http',
		id: 'ha',
		url: 'https://ha.example.com/mcp',
		enabled: true
	} as const;
	assert.doesNotThrow(() => assertHttpHostAllowed(server, undefined));
	assert.doesNotThrow(() => assertHttpHostAllowed(server, ''));
	assert.doesNotThrow(() => assertHttpHostAllowed(server, 'ha.example.com'));
	assert.throws(
		() => assertHttpHostAllowed(server, 'other.example'),
		(error: unknown) => {
			assert.ok(error instanceof McpError);
			assert.equal(error.kind, 'forbidden');
			assert.equal(error.serverId, 'ha');
			return true;
		}
	);
});
