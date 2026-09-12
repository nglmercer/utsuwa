import { browser } from '$app/environment';
import { getBridge, HOST_EVENT, isHostEvent } from './bridge';

export interface ScreenDisplay {
	id: string;
	name: string;
	width: number;
	height: number;
	scale_factor: number;
}

export interface ScreenWindow {
	id: string;
	title: string;
	app: string;
}

export interface ScreenShareStatus {
	available: boolean;
	backend: string;
	sharing: boolean;
	paused: boolean;
	control_enabled: boolean;
	session_id: string | null;
	target: unknown;
	started_at: string | null;
	displays: ScreenDisplay[];
	windows: ScreenWindow[];
}

let status = $state<ScreenShareStatus | null>(null);
let busy = $state(false);
let error = $state<string | null>(null);
let attached = false;

function parseStatus(value: unknown): ScreenShareStatus | null {
	if (typeof value !== 'object' || value === null) return null;
	const raw = value as Record<string, unknown>;
	const displays = Array.isArray(raw.displays)
		? raw.displays.flatMap((display) => {
			if (typeof display !== 'object' || display === null) return [];
			const item = display as Record<string, unknown>;
			if (typeof item.id !== 'string' || typeof item.name !== 'string') return [];
			return [
				{
					id: item.id,
					name: item.name,
					width: typeof item.width === 'number' ? item.width : 0,
					height: typeof item.height === 'number' ? item.height : 0,
					scale_factor: typeof item.scale_factor === 'number' ? item.scale_factor : 1
				}
			];
		})
		: [];
	const windows = Array.isArray(raw.windows)
		? raw.windows.flatMap((window) => {
			if (typeof window !== 'object' || window === null) return [];
			const item = window as Record<string, unknown>;
			if (typeof item.id !== 'string' || typeof item.title !== 'string') return [];
			return [
				{
					id: item.id,
					title: item.title,
					app: typeof item.app === 'string' ? item.app : ''
				}
			];
		})
		: [];
	return {
		available: raw.available === true,
		backend: typeof raw.backend === 'string' ? raw.backend : 'desktop.stub',
		sharing: raw.sharing === true,
		paused: raw.paused === true,
		control_enabled: raw.control_enabled === true,
		session_id: typeof raw.session_id === 'string' ? raw.session_id : null,
		target: raw.target ?? null,
		started_at: typeof raw.started_at === 'string' ? raw.started_at : null,
		displays,
		windows
	};
}

async function call(method: string, params: Record<string, unknown> = {}) {
	const bridge = getBridge();
	if (!bridge) throw new Error('Screen sharing is available in the native desktop app only.');
	return bridge.invoke(method, params);
}

export function screenShareStatus(): ScreenShareStatus | null {
	return status;
}

export function screenShareBusy(): boolean {
	return busy;
}

export function screenShareError(): string | null {
	return error;
}

export async function refreshScreenShareStatus(): Promise<ScreenShareStatus | null> {
	try {
		const next = parseStatus(await call('desktop.share_screen.status'));
		if (next) status = next;
		error = null;
		return next;
	} catch (reason) {
		if (getBridge()) error = reason instanceof Error ? reason.message : 'Unable to read screen-sharing status.';
		return status;
	}
}

export async function startScreenShare(target: Record<string, unknown> = { type: 'desktop' }) {
	busy = true;
	error = null;
	try {
		const next = parseStatus(await call('desktop.share_screen.start', { target, max_fps: 2 }));
		if (!next) throw new Error('The host returned an invalid screen-sharing status.');
		status = next;
		return next;
	} catch (reason) {
		error = reason instanceof Error ? reason.message : 'Unable to start screen sharing.';
		return null;
	} finally {
		busy = false;
	}
}

async function update(method: string) {
	busy = true;
	error = null;
	try {
		const next = parseStatus(await call(method));
		if (!next) throw new Error('The host returned an invalid screen-sharing status.');
		status = next;
		return next;
	} catch (reason) {
		error = reason instanceof Error ? reason.message : 'The screen-sharing request failed.';
		return null;
	} finally {
		busy = false;
	}
}

export function pauseScreenShare() {
	return update('desktop.share_screen.pause');
}

export function resumeScreenShare() {
	return update('desktop.share_screen.resume');
}

export function stopScreenShare() {
	return update('desktop.share_screen.stop');
}

export function setDesktopControl(enabled: boolean) {
	return update(enabled ? 'desktop.control.enable' : 'desktop.control.disable');
}

function onHostEvent(event: Event) {
	if (!isHostEvent(event)) return;
	const detail = (event as CustomEvent).detail;
	if (detail?.event === 'desktop.share_screen.changed') void refreshScreenShareStatus();
}

export function attachScreenShareListener() {
	if (!browser || attached) return;
	attached = true;
	window.addEventListener(HOST_EVENT, onHostEvent);
	void refreshScreenShareStatus();
}
