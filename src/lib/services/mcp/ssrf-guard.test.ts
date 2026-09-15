import test from 'node:test';
import assert from 'node:assert/strict';

import { assertSafeMcpUrl, isBlockedMcpHost, type HostResolver } from './ssrf-guard.ts';

test('loopback, link-local, and metadata literals are blocked', () => {
	for (const host of [
		'127.0.0.1',
		'127.1',
		'0x7f000001',
		'2130706433',
		'localhost',
		'evil.localhost',
		'::1',
		'::',
		'0.0.0.0',
		'169.254.169.254',
		'169.254.10.20',
		'fe80::1',
		'[fe80::1]',
		'ff02::1',
		'224.0.0.1',
		'::ffff:127.0.0.1',
		''
	]) {
		assert.equal(isBlockedMcpHost(host), true, host);
	}
});

test('private LAN and public hosts stay reachable', () => {
	for (const host of [
		'192.168.1.10',
		'10.0.0.5',
		'172.16.4.2',
		'homeassistant.local',
		'mcp.example.com',
		'8.8.8.8',
		'[fd00::1]'
	]) {
		assert.equal(isBlockedMcpHost(host), false, host);
	}
});

test('hostnames resembling blocked ranges are not misclassified', () => {
	assert.equal(isBlockedMcpHost('ffbanking.com'), false);
	assert.equal(isBlockedMcpHost('localhost.evil.com'), false);
});

const stubResolver =
	(table: Record<string, string[]>): HostResolver =>
	async (hostname) => {
		if (!(hostname in table)) throw new Error(`ENOTFOUND ${hostname}`);
		return table[hostname];
	};

test('assertSafeMcpUrl enforces scheme and literal blocks', async () => {
	const resolve = stubResolver({ 'mcp.example.com': ['93.184.216.34'] });
	await assert.rejects(() => assertSafeMcpUrl('ftp://x/mcp', resolve), /http or https/);
	await assert.rejects(() => assertSafeMcpUrl('not a url', resolve), /Invalid MCP/);
	await assert.rejects(() => assertSafeMcpUrl('http://127.0.0.1/mcp', resolve), /not allowed/);
	await assert.rejects(() => assertSafeMcpUrl('http://169.254.169.254/', resolve), /not allowed/);
	const url = await assertSafeMcpUrl('https://mcp.example.com/mcp', resolve);
	assert.equal(url.hostname, 'mcp.example.com');
});

test('DNS answers are all checked (rebinding protection)', async () => {
	const resolve = stubResolver({
		'good.example.com': ['93.184.216.34'],
		'evil.example.com': ['93.184.216.34', '169.254.169.254'],
		'loop.example.com': ['::1']
	});
	await assertSafeMcpUrl('https://good.example.com/mcp', resolve);
	await assert.rejects(
		() => assertSafeMcpUrl('https://evil.example.com/mcp', resolve),
		/blocked address/
	);
	await assert.rejects(() => assertSafeMcpUrl('https://loop.example.com/mcp', resolve), /blocked address/);
	await assert.rejects(() => assertSafeMcpUrl('https://nx.example.com/mcp', resolve), /did not resolve/);
});
