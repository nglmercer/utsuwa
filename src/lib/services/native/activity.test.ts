import test from 'node:test';
import assert from 'node:assert/strict';

import {
	formatDuration,
	headlineFor,
	parseActivityList,
	parseActivityRecord,
	shortHash
} from './activity.ts';

test('parses a full Rust audit record', () => {
	const record = parseActivityRecord({
		timestamp_ms: 1725711720000,
		principal: { Agent: 'a1b2c3d4-e5f6' },
		capability: 'FilesystemRead',
		resource: { Path: '/work/src/main.rs' },
		outcome: 'Executed',
		detail: 'filesystem.read ok',
		duration_ms: 42
	});
	assert.equal(record?.principal, 'Agent a1b2c3d4');
	assert.equal(record?.capability, 'FilesystemRead');
	assert.equal(record?.resource, '/work/src/main.rs');
	assert.equal(record?.outcome, 'Executed');
	assert.equal(record?.duration_ms, 42);
});

test('MCP resources summarize as server / tool', () => {
	const record = parseActivityRecord({
		timestamp_ms: 1,
		principal: { McpServer: 'gh' },
		capability: 'McpInvoke',
		resource: { McpTool: { server: 'gh', tool: 'search' } },
		outcome: 'ApprovalRequested',
		detail: 'no grant',
		duration_ms: null
	});
	assert.equal(record?.principal, 'MCP gh');
	assert.equal(record?.resource, 'MCP gh / search');
	assert.equal(record?.duration_ms, null);
});

test('mutation witness parses, garbage witnesses yield null', () => {
	const record = parseActivityRecord({
		timestamp_ms: 1,
		principal: 'User',
		capability: 'FilesystemWrite',
		resource: { Path: '/work/notes.txt' },
		outcome: 'Executed',
		detail: 'filesystem.patch ok',
		duration_ms: 7,
		mutation: {
			path: '/work/notes.txt',
			before_sha256: 'aaaabbbbccccdddd',
			after_sha256: null
		}
	});
	assert.deepEqual(record?.mutation, {
		path: '/work/notes.txt',
		before_sha256: 'aaaabbbbccccdddd',
		after_sha256: null
	});

	const plain = parseActivityRecord({
		timestamp_ms: 1,
		principal: 'User',
		outcome: 'Executed',
		detail: ''
	});
	assert.equal(plain?.mutation, null);

	const broken = parseActivityRecord({
		timestamp_ms: 1,
		principal: 'User',
		outcome: 'Executed',
		detail: '',
		mutation: { before_sha256: 'abc' }
	});
	assert.equal(broken?.mutation, null);

	assert.equal(shortHash(null), null);
	assert.equal(shortHash('abc'), 'abc');
	assert.equal(shortHash('aaaabbbbccccdddd'), 'aaaa…dddd');
});

test('garbage records are dropped', () => {
	assert.equal(parseActivityRecord(null), null);
	assert.equal(parseActivityRecord({ outcome: 'Executed' }), null);
	assert.equal(parseActivityRecord({ timestamp_ms: 'now', outcome: 'Executed' }), null);
	assert.deepEqual(parseActivityList({}), []);
	assert.deepEqual(parseActivityList([null, { outcome: 'Executed' }]), []);
});

test('headlines and durations read naturally', () => {
	const record = parseActivityRecord({
		timestamp_ms: 1725711720000,
		principal: 'User',
		capability: 'FilesystemWrite',
		resource: null,
		outcome: 'Approved',
		detail: '',
		duration_ms: 3100
	})!;
	const headline = headlineFor(record);
	assert.match(headline, /User/);
	assert.match(headline, /Filesystem write/);
	assert.match(headline, /Approved/);
	assert.equal(formatDuration(record.duration_ms), '3.1 s');
	assert.equal(formatDuration(42), '42 ms');
	assert.equal(formatDuration(null), null);
});
