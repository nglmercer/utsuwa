/**
 * True only in the native desktop build, decided at build time (see
 * vite.config.ts: UTSUWA_NATIVE=1). Baked in so routing decisions never
 * depend on runtime-injected globals. This never races.
 */
export function isDesktopBuild(): boolean {
	return typeof __IS_DESKTOP__ !== 'undefined' && __IS_DESKTOP__;
}
