import test from 'node:test';
import assert from 'node:assert/strict';

import {
	formatCapability,
	headlineFor,
	parsePermissionRequest,
	replyParams,
	riskLevel
} from './permissions.ts';

test('risk levels separate observation from mutation and control', () => {
	assert.equal(riskLevel('FilesystemRead'), 'observe');
	assert.equal(riskLevel('ScreenCapture'), 'control');
	assert.equal(riskLevel('FilesystemWrite'), 'mutate');
	assert.equal(riskLevel('DesktopControl'), 'control');
	assert.equal(riskLevel('ProcessSpawn'), 'control');
	assert.equal(riskLevel('ClipboardRead'), 'observe');
});

test('capability ids become readable labels', () => {
	assert.equal(formatCapability('FilesystemRead'), 'Filesystem read');
	assert.equal(formatCapability('DesktopControl'), 'Desktop control');
});

test('parses a Rust PendingRequest payload', () => {
	const req = parsePermissionRequest({
		id: 'perm-1',
		principal: { Agent: 'a1' },
		capability: 'FilesystemWrite',
		resource: { Path: '/work/src/main.rs' },
		reason: 'patch file'
	});
	assert.equal(req?.id, 'perm-1');
	assert.equal(req?.principal, 'Agent');
	assert.equal(req?.resource.kind, 'path');
	assert.equal(req?.resource.label, '/work/src/main.rs');
	assert.equal(headlineFor(req!), 'The assistant wants to modify:');
});

test('rejects malformed payloads instead of rendering garbage', () => {
	assert.equal(parsePermissionRequest(null), null);
	assert.equal(parsePermissionRequest({}), null);
	assert.equal(parsePermissionRequest({ id: 'x' }), null);
	assert.equal(
		parsePermissionRequest({ id: 'x', capability: 'FilesystemRead' })?.resource.kind,
		'unknown'
	);
});

test('reply params route deny and searchable lifetimes', () => {
	assert.deepEqual(replyParams('perm-1', 'deny'), {
		method: 'permission.deny',
		params: { id: 'perm-1' }
	});
	assert.deepEqual(replyParams('perm-1', 'task'), {
		method: 'permission.approve',
		params: { id: 'perm-1', lifetime: 'task' }
	});
});
