import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { HOST_EVENTS } from './host-events.ts';

const here = dirname(fileURLToPath(import.meta.url));
const contract = JSON.parse(
	readFileSync(join(here, '../../../../protocol/ipc-contract.json'), 'utf8')
) as { version: number; methods: Array<{ name: string }>; events: Array<{ name: string }> };

// Every method the WebView invokes (bridge.invoke literals and method
// params across src/). Update this list when the frontend calls a new
// host method; the manifest must list it too.
const FRONTEND_METHODS = [
	'app.version',
	'agent.send_message',
	'agent.cancel',
	'permission.approve',
	'permission.deny',
	'permission.list',
	'permission.grant',
	'permission.revoke',
	'permission.grants',
	'settings.get',
	'settings.set',
	'settings.get_model_provider',
	'settings.set_model_provider',
	'providers.fetch_models',
	'mcp.status',
	'mcp.connect',
	'mcp.set_server_token',
	'audio_capture.start',
	'audio_capture.stop',
	'audio_capture.cancel',
	'camera.activity.status',
	'microphone.activity.status',
	'desktop.share_screen.start',
	'desktop.share_screen.pause',
	'desktop.share_screen.resume',
	'desktop.share_screen.stop',
	'desktop.share_screen.status',
	'desktop.control.enable',
	'desktop.control.disable',
	'desktop.emergency_stop',
	'desktop.emergency_clear',
	'activity.list',
	'plugin.list',
	'plugin.enable',
	'plugin.disable',
	'plugin.update',
	'plugin.remove',
	'host.runtime_state',
	'host.frontend_ready',
	'diagnostics.report',
	'task.create',
	'task.get',
	'task.list',
	'task.cancel',
	'task.review',
	'task.event'
];

test('contract manifest is versioned with unique names', () => {
	assert.equal(contract.version, 1);
	const methods = contract.methods.map((entry) => entry.name);
	const events = contract.events.map((entry) => entry.name);
	assert.equal(new Set(methods).size, methods.length, 'duplicate method');
	assert.equal(new Set(events).size, events.length, 'duplicate event');
	assert.ok(methods.length > 40);
	assert.ok(events.length > 10);
});

test('every consumed host event is in the contract', () => {
	const events = new Set(contract.events.map((entry) => entry.name));
	for (const name of Object.values(HOST_EVENTS)) {
		assert.ok(events.has(name), `contract lists ${name}`);
	}
});

test('every frontend-invoked method is in the contract', () => {
	const methods = new Set(contract.methods.map((entry) => entry.name));
	for (const name of FRONTEND_METHODS) {
		assert.ok(methods.has(name), `contract lists ${name}`);
	}
});
