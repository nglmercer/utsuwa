// Keep this relative so the small detection module can also run in the
// framework-free Node test runner.
import { getBridge } from '../native/bridge.ts';

/** True when the frontend was built with the native packaging flag. */
export function isDesktopBuildExpected(): boolean {
	return typeof __IS_DESKTOP__ !== 'undefined' && __IS_DESKTOP__;
}

/**
 * True only when the native host actually installed a usable IPC bridge.
 * `window.utsuwa` is present in packaged builds and in native dev mode, but
 * is absent from a normal browser. `getBridge()` is SSR-safe.
 */
export function isNativeRuntimeAvailable(): boolean {
	return getBridge() !== null;
}

/** Alias that reads naturally at call sites concerned with host presence. */
export const hasNativeHost = isNativeRuntimeAvailable;

/**
 * True for either a packaged native build or a page running inside the native
 * host. The runtime bridge is deliberately part of this signal: the dev
 * WebView loads Vite without necessarily inheriting UTSUWA_NATIVE.
 */
export function isDesktopBuild(): boolean {
	return isDesktopBuildExpected() || isNativeRuntimeAvailable();
}
