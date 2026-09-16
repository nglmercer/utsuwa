// Native MCP settings adapter: the WebView configures MCP servers, Rust
// executes them. Server configs travel through the generic `mcp.servers`
// setting (canonical Rust shape); Bearer [REDACTED] go through the
// write-only `mcp.set_server_token` IPC into the OS secret store and are
// never read back. Framework-free so it runs under `node --test`; the
// reactive settings UI lives in `McpSettings.svelte`.
import {
	fromRustMcpConfigs,
	toRustMcpConfig,
	type McpServerConfig
} from '../mcp/types.ts';

export type NativeInvoke = (
	method: string,
	params?: Record<string, unknown>
) => Promise<unknown>;

/** One row of the `mcp.status` IPC. Never carries credentials. */
export interface NativeMcpStatus {
	id: string;
	name?: string;
	transport: string;
	enabled: boolean;
	connected: boolean;
	tools: number;
	last_error?: string;
	has_token: boolean;
}

function asRecord(value: unknown): Record<string, unknown> | null {
	return typeof value === 'object' && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: null;
}

function bridgeErrorMessage(error: unknown, fallback: string): string {
	const record = asRecord(error);
	const nested = record ? asRecord(record.error) : null;
	const message =
		(typeof record?.message === 'string' && record.message) ||
		(typeof nested?.message === 'string' && nested.message) ||
		(typeof error === 'string' && error) ||
		'';
	return message || fallback;
}

/** Parse one `mcp.status` row. Returns null for unrecognizable rows so the
 * settings UI never renders garbage. */
export function parseNativeMcpStatus(value: unknown): NativeMcpStatus | null {
	const row = asRecord(value);
	if (!row || typeof row.id !== 'string' || !row.id) return null;
	return {
		id: row.id,
		...(typeof row.name === 'string' && row.name ? { name: row.name } : {}),
		transport: typeof row.transport === 'string' ? row.transport : 'unknown',
		enabled: row.enabled !== false,
		connected: row.connected === true,
		tools: typeof row.tools === 'number' && Number.isFinite(row.tools) ? Math.max(0, Math.floor(row.tools)) : 0,
		...(typeof row.last_error === 'string' && row.last_error
			? { last_error: row.last_error.slice(0, 500) }
			: {}),
		has_token: row.has_token === true
	};
}

/** Read the native `mcp.servers` setting into flat UI configs. */
export async function getNativeMcpServers(invoke: NativeInvoke): Promise<McpServerConfig[]> {
	let result: unknown;
	try {
		result = await invoke('settings.get', { key: 'mcp.servers' });
	} catch (error) {
		throw new Error(bridgeErrorMessage(error, 'Could not read native MCP settings'));
	}
	const value = asRecord(result)?.value;
	if (value === null || value === undefined) return [];
	const { servers, dropped } = fromRustMcpConfigs(value);
	if (dropped.length > 0) {
		console.warn(`[mcp] dropped ${dropped.length} unreadable native server(s): ${dropped[0]}`);
	}
	return servers;
}

/** Write flat UI configs to the native `mcp.servers` setting. Bearer tokens
 * are never included (see `setNativeMcpServerToken`). */
export async function setNativeMcpServers(
	invoke: NativeInvoke,
	servers: McpServerConfig[]
): Promise<void> {
	try {
		await invoke('settings.set', {
			key: 'mcp.servers',
			value: servers.map(toRustMcpConfig)
		});
	} catch (error) {
		throw new Error(bridgeErrorMessage(error, 'Could not save native MCP settings'));
	}
}

/** Connection state for every configured server. */
export async function getNativeMcpStatus(invoke: NativeInvoke): Promise<NativeMcpStatus[]> {
	let result: unknown;
	try {
		result = await invoke('mcp.status', {});
	} catch (error) {
		throw new Error(bridgeErrorMessage(error, 'Could not read native MCP status'));
	}
	if (!Array.isArray(result)) throw new Error('Native MCP status was malformed');
	return result.flatMap((row) => {
		const parsed = parseNativeMcpStatus(row);
		return parsed ? [parsed] : [];
	});
}

/** Connect one server now and return its discovered tool names. Used by the
 * settings "test" button; agent turns connect lazily on their own. */
export async function connectNativeMcpServer(
	invoke: NativeInvoke,
	id: string
): Promise<string[]> {
	let result: unknown;
	try {
		result = await invoke('mcp.connect', { id });
	} catch (error) {
		throw new Error(bridgeErrorMessage(error, `Could not connect MCP server '${id}'`));
	}
	const tools = asRecord(result)?.tools;
	if (!Array.isArray(tools) || !tools.every((t): t is string => typeof t === 'string')) {
		throw new Error('Native MCP connect answer was malformed');
	}
	return tools;
}

/** Store (or, when empty, delete) one HTTP server's Bearer [REDACTED] the
 * OS secret store. Returns whether a token is now stored. */
export async function setNativeMcpServerToken(
	invoke: NativeInvoke,
	id: string,
	token: string
): Promise<boolean> {
	let result: unknown;
	try {
		result = await invoke('mcp.set_server_token', { id, token });
	} catch (error) {
		throw new Error(bridgeErrorMessage(error, `Could not save the token for '${id}'`));
	}
	return asRecord(result)?.has_token === true;
}
