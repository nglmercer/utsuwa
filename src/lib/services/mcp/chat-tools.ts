/** Shared setup between the web chat transports and MCP tool use.
 *
 * Both the browser `direct` transport and the `/api/chat` server route funnel
 * through here: given the user's MCP settings plus the deployment's proxy
 * availability, `resolveMcpChatTools` either returns a primed executor with
 * tool definitions or `null` (chat then runs exactly as before). No `$env` or
 * store access — callers inject environment-derived values so this stays
 * unit-testable under node.
 */
import { McpToolExecutor, type ChatToolDefinition, type McpExecutorMode } from './mcp-executor.ts';
import type { FetchImpl } from './http-client.ts';
import type { McpServerConfig } from './types.ts';

export interface McpChatTools {
	executor: McpToolExecutor;
	definitions: ChatToolDefinition[];
}

export interface ResolveMcpChatToolsOptions {
	/** User-level kill switch (Settings > MCP Tools). Off by default. */
	enabled: boolean;
	servers: McpServerConfig[];
	mode: McpExecutorMode;
	/** Whether `/api/mcp` answered enabled. Ignored in `direct` mode. */
	proxyAvailable: boolean;
	/** Full (`server__tool`) or bare tool names that are never auto-executed. */
	confirmTools?: string[];
	fetchImpl?: FetchImpl;
	toolTimeoutMs?: number;
}

/** Prime MCP tools for one chat turn, or return null when MCP is inactive. */
export async function resolveMcpChatTools(
	options: ResolveMcpChatToolsOptions
): Promise<McpChatTools | null> {
	const { enabled, servers, mode, proxyAvailable, confirmTools, fetchImpl, toolTimeoutMs } = options;
	if (!enabled) return null;
	const active = servers.filter((server) => server.enabled);
	if (active.length === 0) return null;
	if (mode === 'proxy' && !proxyAvailable) return null;
	const executor = new McpToolExecutor(active, {
		mode,
		...(fetchImpl ? { fetchImpl } : {}),
		...(confirmTools ? { confirmTools } : {}),
		...(toolTimeoutMs !== undefined ? { toolTimeoutMs } : {})
	});
	const definitions = await executor.definitions();
	if (definitions.length === 0) return null;
	return { executor, definitions };
}

/** Desktop webviews may call HTTP MCP servers directly; web builds go through
 * the same-origin proxy (browser CORS would otherwise block LAN servers). */
export function selectMcpExecutorMode(desktopBuild: boolean): McpExecutorMode {
	return desktopBuild ? 'direct' : 'proxy';
}

/** Merge confirm lists (user + deployment env), deduplicated and capped. */
export function mergeConfirmTools(...lists: (string[] | undefined)[]): string[] {
	const merged: string[] = [];
	const seen = new Set<string>();
	for (const list of lists) {
		for (const entry of list ?? []) {
			const name = entry.trim();
			if (!name || name.length > 128 || seen.has(name)) continue;
			seen.add(name);
			merged.push(name);
			if (merged.length >= 256) return merged;
		}
	}
	return merged;
}

interface ProxyStatusCache {
	at: number;
	available: boolean;
}

let proxyStatusCache: ProxyStatusCache | null = null;

/** Cached `GET /api/mcp/status` probe. Failures mean unavailable (fail-closed). */
export async function isMcpProxyAvailable(
	fetchImpl: FetchImpl = (url, init) => fetch(url, init),
	ttlMs = 60_000,
	nowMs = Date.now()
): Promise<boolean> {
	if (proxyStatusCache && nowMs - proxyStatusCache.at < ttlMs) {
		return proxyStatusCache.available;
	}
	let available = false;
	try {
		const response = await fetchImpl('/api/mcp/status', { method: 'GET' });
		if (response.ok) {
			const data = (await response.json()) as { enabled?: unknown };
			available = data.enabled === true;
		}
	} catch {
		available = false;
	}
	proxyStatusCache = { at: nowMs, available };
	return available;
}

/** Reset the status cache (tests, and right after the user toggles MCP). */
export function clearMcpProxyCache(): void {
	proxyStatusCache = null;
}
