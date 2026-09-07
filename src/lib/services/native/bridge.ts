// Typed access to the native-host JS bridge (`window.utsuwa`, installed by
// the Rust host as a wry initialization script). Absent in plain browsers —
// every helper degrades to a clear rejection instead of throwing on
// `undefined`.

export interface UtsuwaBridge {
	invoke(method: string, params?: Record<string, unknown>): Promise<unknown>;
}

declare global {
	interface Window {
		utsuwa?: UtsuwaBridge;
	}
}

export function getBridge(): UtsuwaBridge | null {
	if (typeof window === 'undefined') return null;
	return window.utsuwa ?? null;
}

/** Host event channel name the bridge dispatches (`utsuwa-host-event`). */
export const HOST_EVENT = 'utsuwa-host-event';

export interface HostEventDetail {
	event: string;
	data: Record<string, unknown>;
}

export function isHostEvent(e: Event): e is CustomEvent<HostEventDetail> {
	return e instanceof CustomEvent && typeof (e as CustomEvent).type === 'string';
}
