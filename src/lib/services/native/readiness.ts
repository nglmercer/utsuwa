// Deterministic frontend/native handshake.
//
// The host used to emit a one-shot `app.ready` event right after creating
// the WebView — before page scripts (let alone Svelte listeners) existed —
// so readiness was lossy by construction. The handshake fixes that:
//
//   1. Rust installs the bridge (which buffers early host events).
//   2. The page loads and registers `utsuwa-host-event` listeners
//      synchronously during mount.
//   3. `ensureHandshake()` invokes `host.frontend_ready`; Rust marks the
//      frontend ready and answers with its full runtime state.
//   4. `ensureHandshake()` calls `bridge.markReady()`, replaying buffered
//      events in order to the now-registered listeners.
//
// Ordering rule: attach listeners synchronously during component mount and
// they are guaranteed to observe the replay (the handshake always awaits at
// least one IPC round-trip, so all synchronous mount work wins the race).
// Listeners attached later (user actions, async callbacks) must use
// `getCachedRuntimeState()` instead of relying on the `app.ready` event.
//
// No timeouts-as-synchronization anywhere: every step is causally ordered.
// Framework-free so the Node test runner can exercise it directly.

import {
	getBridge,
	NATIVE_BRIDGE_UNAVAILABLE_ERROR,
	type UtsuwaBridge
} from './bridge.ts';

/** Must match `BRIDGE_PROTOCOL_VERSION` (Rust) and `BRIDGE_VERSION` (bridge.js). */
export const EXPECTED_BRIDGE_PROTOCOL = 1;

/** Hard ceiling for the whole handshake (transport-dead detection). */
export const HANDSHAKE_TIMEOUT_MS = 15000;

export interface RuntimeCapabilities {
	agent: boolean;
	audioCapture: boolean;
	storage: boolean;
}

export interface RuntimeState {
	bridgeProtocol: number;
	hostVersion: string;
	ready: boolean;
	platform: string;
	desktopBackend: string;
	frontendReady: boolean;
	capabilities: RuntimeCapabilities;
}

export type BootPhase =
	| 'idle'
	| 'bridge-connected'
	| 'runtime-ready'
	| 'ready'
	| 'degraded'
	| 'fatal';

export interface BootSnapshot {
	phase: BootPhase;
	/** Human-readable detail for the fatal/degraded screen. */
	detail: string;
	state: RuntimeState | null;
}

export class HandshakeError extends Error {
	constructor(message: string) {
		super(message);
		this.name = 'HandshakeError';
	}
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null;
}

function parseRuntimeState(raw: unknown): RuntimeState {
	if (!isRecord(raw)) throw new HandshakeError('host returned a malformed runtime state');
	const capabilities = isRecord(raw.capabilities) ? raw.capabilities : {};
	const state: RuntimeState = {
		bridgeProtocol: typeof raw.bridgeProtocol === 'number' ? raw.bridgeProtocol : -1,
		hostVersion: typeof raw.hostVersion === 'string' ? raw.hostVersion : 'unknown',
		ready: raw.ready === true,
		platform: typeof raw.platform === 'string' ? raw.platform : 'unknown',
		desktopBackend: typeof raw.desktopBackend === 'string' ? raw.desktopBackend : 'unknown',
		frontendReady: raw.frontendReady === true,
		capabilities: {
			agent: capabilities.agent === true,
			audioCapture: capabilities.audioCapture === true,
			storage: capabilities.storage === true
		}
	};
	if (state.bridgeProtocol !== EXPECTED_BRIDGE_PROTOCOL) {
		throw new HandshakeError(
			`incompatible native bridge protocol (host: ${state.bridgeProtocol}, expected: ${EXPECTED_BRIDGE_PROTOCOL})`
		);
	}
	if (!state.ready) throw new HandshakeError('host reported it is not ready');
	return state;
}

function checkBridgeShape(bridge: UtsuwaBridge): void {
	if (bridge.bridgeVersion !== EXPECTED_BRIDGE_PROTOCOL) {
		throw new HandshakeError(
			`incompatible native bridge script (bridge: ${String(bridge.bridgeVersion)}, expected: ${EXPECTED_BRIDGE_PROTOCOL}); the host and frontend builds are mismatched`
		);
	}
	if (typeof bridge.markReady !== 'function') {
		throw new HandshakeError(
			'stale native bridge: markReady() is missing; the host and frontend builds are mismatched'
		);
	}
}

/** Fire-and-forget bootstrap marker over `diagnostics.report`. Never throws. */
export function reportBootstrapMarker(
	marker: 'frontend.bootstrap.begin' | 'frontend.bootstrap.ready' | 'frontend.bootstrap.degraded',
	extra: Record<string, unknown> = {}
): void {
	try {
		const bridge = getBridge();
		if (!bridge) return;
		void bridge
			.invoke('diagnostics.report', { kind: marker, ...extra }, { timeoutMs: 15000 })
			.catch(() => {});
	} catch {
		/* diagnostics must never break boot */
	}
}

type Listener = (snapshot: BootSnapshot) => void;

class NativeBoot {
	private snapshot: BootSnapshot = { phase: 'idle', detail: '', state: null };
	private listeners = new Set<Listener>();
	private handshake: Promise<RuntimeState> | null = null;

	getSnapshot(): BootSnapshot {
		return this.snapshot;
	}

	subscribe(listener: Listener): () => void {
		this.listeners.add(listener);
		listener(this.snapshot);
		return () => {
			this.listeners.delete(listener);
		};
	}

	private set(snapshot: BootSnapshot): void {
		this.snapshot = snapshot;
		for (const listener of this.listeners) {
			try {
				listener(snapshot);
			} catch {
				/* a broken UI listener must not break boot */
			}
		}
	}

	/** Test/reset hook: forget cached handshake state. */
	resetForTests(): void {
		this.handshake = null;
		this.snapshot = { phase: 'idle', detail: '', state: null };
	}

	/**
	 * Run the deterministic handshake (idempotent: concurrent and repeat
	 * callers share one attempt). On success the bridge is drained and the
	 * runtime state cached; on failure the phase becomes `fatal` with an
	 * actionable detail string.
	 */
	ensureHandshake(): Promise<RuntimeState> {
		if (!this.handshake) {
			this.handshake = this.runHandshake().catch((error: unknown) => {
				// Allow an explicit retry to start a fresh attempt.
				this.handshake = null;
				throw error;
			});
		}
		return this.handshake;
	}

	private async runHandshake(): Promise<RuntimeState> {
		reportBootstrapMarker('frontend.bootstrap.begin');
		this.set({ phase: 'bridge-connected', detail: '', state: null });
		try {
			const bridge = getBridge();
			if (!bridge) throw new HandshakeError(NATIVE_BRIDGE_UNAVAILABLE_ERROR);
			checkBridgeShape(bridge);
			const raw = await bridge.invoke(
				'host.frontend_ready',
				{},
				{ timeoutMs: HANDSHAKE_TIMEOUT_MS }
			);
			const state = parseRuntimeState(raw);
			this.set({ phase: 'runtime-ready', detail: '', state });
			const replayed = bridge.markReady ? bridge.markReady() : 0;
			this.set({ phase: 'ready', detail: '', state });
			reportBootstrapMarker('frontend.bootstrap.ready', {
				message: `handshake ok, replayed ${replayed} buffered event(s)`
			});
			return state;
		} catch (error) {
			const detail = error instanceof Error ? error.message : String(error);
			this.set({ phase: 'fatal', detail, state: null });
			reportBootstrapMarker('frontend.bootstrap.degraded', { message: detail });
			throw error;
		}
	}

	/** Dismiss a fatal boot into `degraded` (user chose to continue limited). */
	dismissToDegraded(): void {
		if (this.snapshot.phase !== 'fatal') return;
		this.set({ phase: 'degraded', detail: this.snapshot.detail, state: null });
	}

	/** Retry after fatal (clears the cached attempt and reruns). */
	retry(): Promise<RuntimeState> {
		this.handshake = null;
		return this.ensureHandshake();
	}
}

export const nativeBoot = new NativeBoot();

/** Cached handshake result, or null before/after a failed handshake. */
export function getCachedRuntimeState(): RuntimeState | null {
	return nativeBoot.getSnapshot().state;
}
