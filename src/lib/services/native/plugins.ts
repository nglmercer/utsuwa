// Pure plugin-panel logic for plan Phases 24 (lifecycle) and 25 (trust).
// Framework-free so it runs under `node --test`. The reactive store lives
// in `plugins.svelte.ts`; the Rust side is `plugin_list` / `plugin_manage`
// in `crates/app-host/src/dispatcher.rs`.
//
//   invoke('plugin.list', {}) -> PluginInfo[]
//   invoke('plugin.enable' | 'plugin.disable' | 'plugin.update' | 'plugin.remove', { id })
//     -> { ok: true }
//
// Lifecycle vs authority: enabling loads guest code and registers its tools
// behind policy + per-invocation tickets. It never grants OS authority.

export interface PluginInfo {
	id: string;
	name: string;
	version: string;
	trust: string;
	/** Host runtime kind: 'wasm', or 'native' for unsafe plugins. Absent
	 * on older hosts — treated as sandboxed WASM. */
	runtime?: string;
	state: string;
	tools: string[];
}

export type PluginOp = 'enable' | 'disable' | 'update' | 'remove';

export type InvokeFn = (method: string, params?: Record<string, unknown>) => Promise<unknown>;

/** Parse one host plugin record into presentation shape. Returns null for
 * anything unrecognizable — the panel never renders garbage. */
export function parsePluginInfo(data: unknown): PluginInfo | null {
	if (typeof data !== 'object' || data === null) return null;
	const d = data as Record<string, unknown>;
	if (
		typeof d.id !== 'string' ||
		typeof d.name !== 'string' ||
		typeof d.version !== 'string' ||
		typeof d.trust !== 'string' ||
		typeof d.state !== 'string' ||
		!Array.isArray(d.tools) ||
		!d.tools.every((t) => typeof t === 'string')
	) {
		return null;
	}
	const info: PluginInfo = {
		id: d.id,
		name: d.name,
		version: d.version,
		trust: d.trust,
		state: d.state,
		tools: [...(d.tools as string[])]
	};
	if (typeof d.runtime === 'string') info.runtime = d.runtime;
	return info;
}

/** True for unsafe (native) plugins: code that runs outside the WASM
 * sandbox and needs explicit user approval before enabling. */
export function isUnsafe(plugin: PluginInfo): boolean {
	return plugin.runtime === 'native';
}

/** True when the plugin is serving tools to the agent this turn. */
export function isServing(plugin: PluginInfo): boolean {
	return plugin.state === 'enabled';
}

/** IPC method for a lifecycle operation. */
export function pluginMethod(op: PluginOp): string {
	return `plugin.${op}`;
}

/** List every known plugin. Throws the bridge rejection when the host is
 * unreachable so the panel can show it. */
export async function listPlugins(invoke: InvokeFn): Promise<PluginInfo[]> {
	const result = await invoke('plugin.list', {});
	if (!Array.isArray(result)) throw new Error('plugin.list returned a non-array');
	return result.flatMap((item) => {
		const parsed = parsePluginInfo(item);
		return parsed ? [parsed] : [];
	});
}

/** Run one lifecycle transition, then return the refreshed list. */
export async function managePlugin(
	invoke: InvokeFn,
	op: PluginOp,
	id: string
): Promise<PluginInfo[]> {
	await invoke(pluginMethod(op), { id });
	return listPlugins(invoke);
}
