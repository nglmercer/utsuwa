// Boots the REAL native bridge (`crates/app-host/src/bridge.js`) inside a
// `node:vm` sandbox with a fake window, so handshake/buffer/timeout behavior
// is covered against the shipped script rather than a copy of it.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const here = dirname(fileURLToPath(import.meta.url));
const BRIDGE_JS = readFileSync(
	join(here, '../../../../crates/app-host/src/bridge.js'),
	'utf8'
);

interface Posted {
	id: string;
	method: string;
	params: Record<string, unknown>;
}

interface Loaded {
	bridge: any;
	posted: Posted[];
	dispatched: { event: string; data: unknown }[];
	fakeWindow: any;
}

function loadBridge(options: { withIpc?: boolean; earlyEvents?: unknown } = {}): Loaded {
	const posted: Posted[] = [];
	const dispatched: { event: string; data: unknown }[] = [];
	const fakeWindow: any = {
		dispatchEvent: (e: { type: string; detail: { event: string; data: unknown } }) => {
			dispatched.push({ event: e.detail.event, data: e.detail.data });
			return true;
		}
	};
	if (options.withIpc !== false) {
		fakeWindow.ipc = {
			postMessage: (msg: string) => {
				posted.push(JSON.parse(msg) as Posted);
			}
		};
	}
	if (options.earlyEvents !== undefined) {
		fakeWindow.__utsuwaEarlyEvents = options.earlyEvents;
	}
	const sandbox: Record<string, unknown> = {
		window: fakeWindow,
		CustomEvent: globalThis.CustomEvent,
		setTimeout,
		clearTimeout,
		console
	};
	vm.createContext(sandbox);
	vm.runInContext(BRIDGE_JS, sandbox, { filename: 'bridge.js' });
	return { bridge: fakeWindow.utsuwa, posted, dispatched, fakeWindow };
}

test('bridge exposes protocol version 1', () => {
	const { bridge } = loadBridge();
	assert.equal(bridge.bridgeVersion, 1);
	assert.equal(bridge.isReady(), false);
});

test('events emitted before markReady are buffered and replayed in order', () => {
	const { bridge, dispatched } = loadBridge();
	bridge.__emit('app.ready', { version: '0.1.0' });
	bridge.__emit('agent.turn_done', {});
	assert.equal(dispatched.length, 0);
	assert.equal(bridge.__bufferedCount(), 2);

	const replayed = bridge.markReady();
	assert.equal(replayed, 2);
	assert.equal(bridge.isReady(), true);
	assert.deepEqual(
		dispatched.map((d) => d.event),
		['app.ready', 'agent.turn_done']
	);
	assert.deepEqual(dispatched[0].data, { version: '0.1.0' });

	// Live dispatch after ready: no buffering.
	bridge.__emit('agent.text_delta', { delta: 'hi' });
	assert.equal(dispatched.length, 3);
	assert.equal(bridge.__bufferedCount(), 0);
});

test('markReady is idempotent (reload-safe drain)', () => {
	const { bridge, dispatched } = loadBridge();
	bridge.__emit('app.ready', {});
	assert.equal(bridge.markReady(), 1);
	assert.equal(bridge.markReady(), 0);
	assert.equal(dispatched.length, 1);
});

test('pre-bridge events stashed by Rust are drained on load', () => {
	const { bridge, dispatched, fakeWindow } = loadBridge({
		earlyEvents: [
			{ event: 'app.ready', data: { version: '9' } },
			{ event: 'broken' }, // missing data degrades, entry kept
			'junk' // non-object entries are skipped
		]
	});
	assert.equal(fakeWindow.__utsuwaEarlyEvents, undefined);
	assert.equal(bridge.__bufferedCount(), 2);
	bridge.markReady();
	assert.deepEqual(
		dispatched.map((d) => d.event),
		['app.ready', 'broken']
	);
	assert.deepEqual(dispatched[0].data, { version: '9' });
});

test('event buffer is bounded and keeps the newest events', () => {
	const { bridge, dispatched } = loadBridge();
	for (let i = 0; i < 505; i++) bridge.__emit(`e${i}`, {});
	bridge.markReady();
	assert.equal(dispatched.length, 500);
	assert.equal(dispatched[0].event, 'e5');
	assert.equal(dispatched[499].event, 'e504');
});

test('invoke posts the envelope and resolves via __resolve', async () => {
	const { bridge, posted } = loadBridge();
	const result = bridge.invoke('host.runtime_state', { a: 1 });
	assert.equal(posted.length, 1);
	assert.equal(posted[0].method, 'host.runtime_state');
	assert.deepEqual(posted[0].params, { a: 1 });
	assert.match(posted[0].id, /.+/);
	bridge.__resolve(posted[0].id, true, { ready: true });
	assert.deepEqual(await result, { ready: true });
	assert.equal(bridge.__pendingCount(), 0);
});

test('invoke rejects with the host error code', async () => {
	const { bridge, posted } = loadBridge();
	const result = bridge.invoke('agent.cancel', {});
	bridge.__resolve(posted[0].id, false, { code: 'cancelled', message: 'no turn' });
	await assert.rejects(result, (error: any) => {
		assert.equal(error.message, 'no turn');
		assert.equal(error.code, 'cancelled');
		return true;
	});
});

test('invoke rejects when the native transport is missing', async () => {
	const { bridge } = loadBridge({ withIpc: false });
	await assert.rejects(bridge.invoke('app.version', {}), /no native host/);
	assert.equal(bridge.__pendingCount(), 0);
});

test('invoke rejects on timeout instead of pending forever', async () => {
	const { bridge } = loadBridge();
	await assert.rejects(bridge.invoke('agent.send_message', {}, { timeoutMs: 30 }), (error: any) => {
		assert.match(error.message, /timed out/);
		assert.equal(error.code, 'timeout');
		return true;
	});
	assert.equal(bridge.__pendingCount(), 0);
});

test('late resolve after timeout is ignored', async () => {
	const { bridge, posted } = loadBridge();
	const result = bridge.invoke('x', {}, { timeoutMs: 20 });
	await assert.rejects(result, /timed out/);
	// Must not throw and must not resurrect the pending entry.
	bridge.__resolve(posted[0].id, true, {});
	assert.equal(bridge.__pendingCount(), 0);
});

test('unknown resolve ids are ignored', () => {
	const { bridge } = loadBridge();
	bridge.__resolve('no-such-id', true, {});
	assert.equal(bridge.__pendingCount(), 0);
});
