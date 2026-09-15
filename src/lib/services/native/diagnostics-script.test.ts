// Boots the REAL diagnostics script (`crates/app-host/src/diagnostics.js`)
// in a `node:vm` sandbox and asserts its forwarding behavior.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const here = dirname(fileURLToPath(import.meta.url));
const DIAGNOSTICS_JS = readFileSync(
	join(here, '../../../../crates/app-host/src/diagnostics.js'),
	'utf8'
);

interface Report {
	method: string;
	params: Record<string, unknown>;
}

interface Loaded {
	invoked: Report[];
	windowListeners: Record<string, ((e: any) => void)[]>;
	docListeners: Record<string, ((e: any) => void)[]>;
	originalErrors: unknown[][];
	sandbox: Record<string, unknown>;
}

function loadDiagnostics(bridge: unknown): Loaded {
	const invoked: Report[] = [];
	const windowListeners: Record<string, ((e: any) => void)[]> = {};
	const docListeners: Record<string, ((e: any) => void)[]> = {};
	const originalErrors: unknown[][] = [];
	const fakeWindow: any = {
		utsuwa: bridge,
		location: { href: 'companion://app/app' },
		addEventListener: (type: string, fn: (e: any) => void) => {
			(windowListeners[type] ??= []).push(fn);
		}
	};
	const fakeDocument: any = {
		addEventListener: (type: string, fn: (e: any) => void) => {
			(docListeners[type] ??= []).push(fn);
		}
	};
	const fakeConsole = {
		error: (...args: unknown[]) => {
			originalErrors.push(args);
		}
	};
	const sandbox: Record<string, unknown> = {
		window: fakeWindow,
		document: fakeDocument,
		console: fakeConsole
	};
	if (bridge && typeof (bridge as any).invoke === 'function') {
		const realInvoke = (bridge as any).invoke.bind(bridge);
		(bridge as any).invoke = (method: string, params: Record<string, unknown>) => {
			invoked.push({ method, params });
			return realInvoke(method, params);
		};
	}
	vm.createContext(sandbox);
	vm.runInContext(DIAGNOSTICS_JS, sandbox, { filename: 'diagnostics.js' });
	return { invoked, windowListeners, docListeners, originalErrors, sandbox };
}

function fakeBridge() {
	return {
		invoke: () => Promise.resolve({ ok: true })
	};
}

function fire(listeners: Record<string, ((e: any) => void)[]>, type: string, event: any): void {
	for (const fn of listeners[type] ?? []) fn(event);
}

test('window errors are forwarded to diagnostics.report', () => {
	const { invoked, windowListeners } = loadDiagnostics(fakeBridge());
	fire(windowListeners, 'error', {
		message: 'boom',
		filename: 'companion://app/_app/x.js',
		lineno: 12,
		error: { stack: 'at f (x.js:12)' }
	});
	assert.equal(invoked.length, 1);
	assert.equal(invoked[0].method, 'diagnostics.report');
	assert.equal(invoked[0].params.kind, 'window.error');
	assert.equal(invoked[0].params.message, 'boom');
	assert.equal(invoked[0].params.line, 12);
});

test('unhandled rejections are forwarded', () => {
	const { invoked, windowListeners, sandbox } = loadDiagnostics(fakeBridge());
	// VM-realm Error so the script's `instanceof Error` behaves as in-page.
	const reason = vm.runInContext('new Error("nope")', sandbox);
	fire(windowListeners, 'unhandledrejection', { reason });
	assert.equal(invoked.length, 1);
	assert.equal(invoked[0].params.kind, 'unhandledrejection');
	assert.equal(invoked[0].params.message, 'nope');
});

test('console.error keeps original behavior and forwards', () => {
	const { invoked, originalErrors, sandbox } = loadDiagnostics(fakeBridge());
	vm.runInContext(`console.error('load failed', { code: 7 })`, sandbox);
	assert.equal(originalErrors.length, 1);
	// Cross-realm object: compare structurally, not by prototype.
	assert.equal(originalErrors[0][0], 'load failed');
	assert.equal((originalErrors[0][1] as { code: number }).code, 7);
	assert.equal(invoked.length, 1);
	assert.equal(invoked[0].method, 'diagnostics.report');
	assert.equal(invoked[0].params.kind, 'console.error');
	assert.match(String(invoked[0].params.message), /load failed/);
});

test('lifecycle markers are forwarded', () => {
	const { invoked, windowListeners, docListeners } = loadDiagnostics(fakeBridge());
	fire(docListeners, 'DOMContentLoaded', {});
	fire(windowListeners, 'load', {});
	const kinds = invoked.map((r) => r.params.kind);
	assert.deepEqual(kinds, ['domcontentloaded', 'load']);
	assert.equal(invoked[0].params.url, 'companion://app/app');
});

test('reporting never throws without a bridge', () => {
	const { windowListeners, docListeners } = loadDiagnostics(undefined);
	fire(windowListeners, 'error', { message: 'x' });
	fire(windowListeners, 'unhandledrejection', { reason: 'y' });
	fire(docListeners, 'DOMContentLoaded', {});
	// No bridge, no crash: success is reaching this line.
});

test('installs exactly once', () => {
	const { windowListeners } = loadDiagnostics(fakeBridge());
	assert.equal(windowListeners['error']?.length, 1);
	assert.equal(windowListeners['unhandledrejection']?.length, 1);
});
