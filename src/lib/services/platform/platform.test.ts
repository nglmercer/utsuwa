import test from 'node:test';
import assert from 'node:assert/strict';

import {
	isDesktopBuild,
	isDesktopBuildExpected,
	isNativeRuntimeAvailable
} from './platform.ts';

const globals = globalThis as Record<string, unknown>;
const originalWindow = globals.window;
const hadDesktopFlag = Object.prototype.hasOwnProperty.call(globals, '__IS_DESKTOP__');
const originalDesktopFlag = globals.__IS_DESKTOP__;

function setDesktopFlag(value: boolean): void {
	globals.__IS_DESKTOP__ = value;
}

function setWindow(value: unknown): void {
	globals.window = value;
}

test.afterEach(() => {
	if (hadDesktopFlag) globals.__IS_DESKTOP__ = originalDesktopFlag;
	else delete globals.__IS_DESKTOP__;
	if (originalWindow === undefined) delete globals.window;
	else globals.window = originalWindow;
});

test('normal browser without the native bridge stays on the web path', () => {
	setDesktopFlag(false);
	setWindow({});

	assert.equal(isDesktopBuildExpected(), false);
	assert.equal(isNativeRuntimeAvailable(), false);
	assert.equal(isDesktopBuild(), false);
});

test('packaged desktop builds remain identifiable even before bridge access', () => {
	setDesktopFlag(true);
	delete globals.window;

	assert.equal(isDesktopBuildExpected(), true);
	assert.equal(isNativeRuntimeAvailable(), false);
	assert.equal(isDesktopBuild(), true);
});

test('native dev WebView detection uses the runtime bridge', () => {
	setDesktopFlag(false);
	setWindow({
		utsuwa: {
			invoke: async () => ({ ok: true })
		}
	});

	assert.equal(isDesktopBuildExpected(), false);
	assert.equal(isNativeRuntimeAvailable(), true);
	assert.equal(isDesktopBuild(), true);
});

test('SSR detection is safe when window is undefined', () => {
	setDesktopFlag(false);
	delete globals.window;

	assert.doesNotThrow(() => isDesktopBuild());
	assert.equal(isNativeRuntimeAvailable(), false);
});
