import { browser } from '$app/environment';
import { getBridge } from './bridge';
import {
	listPlugins,
	managePlugin,
	type PluginInfo,
	type PluginOp
} from './plugins';

// Reactive plugin inventory for the Plugins panel (plan Phases 24/25).
// Refreshed on mount and after every lifecycle transition; the host is the
// source of truth, this store is a view of it.
let plugins = $state<PluginInfo[]>([]);
let error = $state<string | null>(null);

export function getPlugins(): PluginInfo[] {
	return plugins;
}

export function getPluginError(): string | null {
	return error;
}

export async function refreshPlugins(): Promise<void> {
	if (!browser) return;
	const bridge = getBridge();
	if (!bridge) {
		error = 'native host is not attached';
		return;
	}
	try {
		plugins = await listPlugins((m, p) => bridge.invoke(m, p));
		error = null;
	} catch (e) {
		error = e instanceof Error ? e.message : 'plugin.list failed';
	}
}

/** Run a lifecycle transition, then refresh. Returns the error to display,
 * or null on success. */
export async function runPluginOp(id: string, op: PluginOp): Promise<string | null> {
	if (!browser) return 'unavailable outside the app';
	const bridge = getBridge();
	if (!bridge) return 'native host is not attached';
	try {
		plugins = await managePlugin((m, p) => bridge.invoke(m, p), op, id);
		error = null;
		return null;
	} catch (e) {
		return e instanceof Error ? e.message : `plugin.${op} failed`;
	}
}
