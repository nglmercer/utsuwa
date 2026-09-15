// Deterministic handshake coverage with a fake bridge (no WebView).
import test from 'node:test';
import assert from 'node:assert/strict';

import {
	getCachedRuntimeState,
	nativeBoot,
	type RuntimeState
} from './readiness.ts';

const globals = globalThis as Record<string, unknown>;
const originalWindow = globals.window;

function setWindow(value: unknown): void {
	globals.window = value;
}

test.beforeEach(() => {
	nativeBoot.resetForTests();
});

test.afterEach(() => {
	nativeBoot.resetForTests();
	if (originalWindow === undefined) delete globals.window;
	else globals.window = originalWindow;
});

function healthyState(): RuntimeState {
	return {
		bridgeProtocol: 1,
		hostVersion: '0.1.0',
		ready: true,
		platform: 'linux',
		desktopBackend: 'gtk',
		frontendReady: true,
		capabilities: { agent: true, audioCapture: true, storage: true }
	};
}

interface FakeBridge {
	bridgeVersion?: number;
	markReady?: () => number;
	invokeCalls: { method: string; params: unknown }[];
	markReadyCalls: number;
	invoke: (method: string, params?: Record<string, unknown>) => Promise<unknown>;
}

function fakeBridge(overrides: Partial<FakeBridge> & { state?: unknown } = {}): FakeBridge {
	const state = overrides.state === undefined ? healthyState() : overrides.state;
	const bridge: FakeBridge = {
		bridgeVersion: 1,
		invokeCalls: [],
		markReadyCalls: 0,
		...overrides,
		invoke: async (method: string, params: Record<string, unknown> = {}) => {
			bridge.invokeCalls.push({ method, params });
			if (method === 'host.frontend_ready') {
				if (state instanceof Error) throw state;
				return state;
			}
			return { ok: true };
		}
	};
	if (!('markReady' in overrides)) {
		bridge.markReady = () => {
			bridge.markReadyCalls += 1;
			return 2; // pretend two buffered events replayed
		};
	}
	return bridge;
}

function frontendReadyCalls(bridge: FakeBridge): number {
	return bridge.invokeCalls.filter((c) => c.method === 'host.frontend_ready').length;
}

test('successful handshake drains the bridge and caches state', async () => {
	const bridge = fakeBridge();
	setWindow({ utsuwa: bridge });
	const snapshots: string[] = [];
	const unsubscribe = nativeBoot.subscribe((s) => snapshots.push(s.phase));

	const state = await nativeBoot.ensureHandshake();
	assert.equal(state.hostVersion, '0.1.0');
	assert.equal(state.desktopBackend, 'gtk');
	assert.equal(bridge.markReadyCalls, 1);
	assert.deepEqual(getCachedRuntimeState(), state);
	// Phase progression is explicit and ordered.
	assert.deepEqual(snapshots, ['idle', 'bridge-connected', 'runtime-ready', 'ready']);
	// Bootstrap markers reported over diagnostics.
	const kinds = bridge.invokeCalls
		.filter((c) => c.method === 'diagnostics.report')
		.map((c) => (c.params as Record<string, unknown>).kind);
	assert.deepEqual(kinds, ['frontend.bootstrap.begin', 'frontend.bootstrap.ready']);
	unsubscribe();
});

test('concurrent handshake callers share one host round-trip', async () => {
	const bridge = fakeBridge();
	setWindow({ utsuwa: bridge });
	const [a, b] = await Promise.all([nativeBoot.ensureHandshake(), nativeBoot.ensureHandshake()]);
	assert.equal(a, b);
	assert.equal(frontendReadyCalls(bridge), 1);
	assert.equal(bridge.markReadyCalls, 1);
});

test('missing bridge fails fatal with the stable message', async () => {
	setWindow({});
	await assert.rejects(
		nativeBoot.ensureHandshake(),
		/Native host runtime was expected but the Utsuwa IPC bridge is unavailable/
	);
	assert.equal(nativeBoot.getSnapshot().phase, 'fatal');
	assert.equal(getCachedRuntimeState(), null);
});

test('bridge script version mismatch fails fatal', async () => {
	setWindow({ utsuwa: fakeBridge({ bridgeVersion: 99 }) });
	await assert.rejects(nativeBoot.ensureHandshake(), /incompatible native bridge script/);
	assert.equal(nativeBoot.getSnapshot().phase, 'fatal');
});

test('bridge without markReady is treated as stale, not healthy', async () => {
	const bridge = fakeBridge();
	delete (bridge as unknown as Record<string, unknown>).markReady;
	setWindow({ utsuwa: bridge });
	await assert.rejects(nativeBoot.ensureHandshake(), /stale native bridge/);
	assert.equal(bridge.markReadyCalls, 0);
});

test('host protocol mismatch fails fatal', async () => {
	const bad = { ...healthyState(), bridgeProtocol: 99 };
	setWindow({ utsuwa: fakeBridge({ state: bad }) });
	await assert.rejects(nativeBoot.ensureHandshake(), /incompatible native bridge protocol/);
	assert.equal(nativeBoot.getSnapshot().phase, 'fatal');
});

test('host reporting not-ready fails fatal', async () => {
	const bad = { ...healthyState(), ready: false };
	setWindow({ utsuwa: fakeBridge({ state: bad }) });
	await assert.rejects(nativeBoot.ensureHandshake(), /not ready/);
});

test('transport failure fails fatal and retry starts a fresh attempt', async () => {
	const dead = fakeBridge({ state: new Error('transport is dead') });
	setWindow({ utsuwa: dead });
	await assert.rejects(nativeBoot.ensureHandshake(), /transport is dead/);
	assert.equal(nativeBoot.getSnapshot().phase, 'fatal');

	setWindow({ utsuwa: fakeBridge() });
	const state = await nativeBoot.retry();
	assert.equal(state.ready, true);
	assert.equal(nativeBoot.getSnapshot().phase, 'ready');
});

test('dismissing fatal degrades instead of staying stuck', async () => {
	setWindow({});
	await assert.rejects(nativeBoot.ensureHandshake());
	nativeBoot.dismissToDegraded();
	assert.equal(nativeBoot.getSnapshot().phase, 'degraded');
	// Dismissing from any other phase is a no-op.
	nativeBoot.dismissToDegraded();
	assert.equal(nativeBoot.getSnapshot().phase, 'degraded');
});
