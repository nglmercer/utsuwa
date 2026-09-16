// Typed access to the native-host JS bridge (`window.utsuwa`, installed by
// the Rust host as a wry initialization script). Absent in plain browsers —
// every helper degrades to a clear rejection instead of throwing on
// `undefined`.

export interface InvokeOptions {
	/** Per-request timeout in ms (default 60000 in the bridge). */
	timeoutMs?: number;
}

export interface UtsuwaBridge {
	invoke(
		method: string,
		params?: Record<string, unknown>,
		options?: InvokeOptions
	): Promise<unknown>;
	/**
	 * Bridge protocol version. Always present on current hosts; missing on
	 * hosts older than the handshake (treated as incompatible by
	 * `readiness.ts` rather than as a healthy host).
	 */
	bridgeVersion?: number;
	/**
	 * Replay buffered host events (registered after listeners) then
	 * dispatch live. Returns the replayed count.
	 */
	markReady?: () => number;
	/** Whether `markReady()` already ran. */
	isReady?: () => boolean;
}

/**
 * A packaged build is expected to have this bridge before page scripts run.
 * Keep the message stable because it is also used when native routing detects
 * a broken host instead of falling back to a tool-less provider request.
 */
export const NATIVE_BRIDGE_UNAVAILABLE_ERROR =
	'Native host runtime was expected but the Utsuwa IPC bridge is unavailable.';

declare global {
	interface Window {
		utsuwa?: UtsuwaBridge;
	}
}

export function getBridge(): UtsuwaBridge | null {
	if (typeof window === 'undefined') return null;
	const bridge = window.utsuwa;
	return bridge && typeof bridge.invoke === 'function' ? bridge : null;
}

/** Host event channel name the bridge dispatches (`utsuwa-host-event`). */
export const HOST_EVENT = 'utsuwa-host-event';

export interface HostEventDetail {
	event: string;
	data: Record<string, unknown>;
}

export function isHostEvent(e: Event): e is CustomEvent<HostEventDetail> {
	if (!(e instanceof CustomEvent)) return false;
	const detail = (e as CustomEvent).detail as HostEventDetail | null | undefined;
	if (!detail || typeof detail !== 'object') return false;
	if (typeof detail.event !== 'string') return false;
	// The bridge always dispatches `data` as an object (`data || {}`); a
	// foreign CustomEvent on the same channel carries anything, so reject it.
	if (!detail.data || typeof detail.data !== 'object') return false;
	return true;
}
