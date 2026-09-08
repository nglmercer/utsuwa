import test from 'node:test';
import assert from 'node:assert/strict';

import {
	formatCapability,
	grantHomeReadAccess,
	headlineFor,
	homeReadGranted,
	listGrants,
	parsePermissionRequest,
	replyParams,
	revokeHomeReadAccess,
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

test('grants snapshot detects the home read toggle', async () => {
	const calls: Array<[string, Record<string, unknown> | undefined]> = [];
	const invoke = async (method: string, params?: Record<string, unknown>) => {
		calls.push([method, params]);
		if (method === 'permission.grants') {
			return {
				grants: [
					{
						principal_kind: 'Agent',
						capability: 'FilesystemRead',
						scope: { resources: [{ Path: '/home/u' }] },
						lifetime: 'Persistent'
					},
					{ capability: 'FilesystemRead', scope: { resources: [] }, lifetime: 'x' },
					{ bogus: true }
				],
				home: '/home/u'
			};
		}
		if (method === 'permission.grant') return { ok: true, path: '/home/u' };
		if (method === 'permission.revoke') return { ok: true, removed: 2 };
		throw new Error(`unexpected ${method}`);
	};
	const snapshot = await listGrants(invoke);
	assert.equal(snapshot.home, '/home/u');
	assert.equal(snapshot.grants.length, 2);
	assert.equal(homeReadGranted(snapshot), true);
	assert.equal(await grantHomeReadAccess(invoke), '/home/u');
	assert.equal(await revokeHomeReadAccess(invoke), 2);
	assert.deepEqual(calls[1], [
		'permission.grant',
		{ capability: 'FilesystemRead', lifetime: 'persistent' }
	]);
	assert.deepEqual(calls[2], ['permission.revoke', { capability: 'FilesystemRead' }]);
});

test('home toggle stays off without a matching grant', () => {
	assert.equal(homeReadGranted({ grants: [], home: '/home/u' }), false);
	assert.equal(homeReadGranted({ grants: [], home: null }), false);
	assert.equal(
		homeReadGranted({
			grants: [{ capability: 'FilesystemWrite', paths: ['/home/u'], lifetime: 'Persistent' }],
			home: '/home/u'
		}),
		false
	);
});
