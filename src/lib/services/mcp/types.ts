/** Shared MCP (Model Context Protocol) types for the companion chat integration.
 *
 * These types are transport-agnostic and safe to import from browser code,
 * server routes, and unit tests (no node builtins, no `$env` access — callers
 * inject environment-derived values).
 */

/** Streamable HTTP server: the MCP endpoint URL speaks JSON-RPC. */
export interface McpHttpServerConfig {
	transport: 'http';
	/** Stable id used in tool names (`<serverId>__<tool>`) and logs. */
	id: string;
	/** Display name shown in Settings. Defaults to the id. */
	name?: string;
	/** Full endpoint URL, e.g. `https://ha.example.com/mcp`. */
	url: string;
	/** Optional bearer token sent as `Authorization: Bearer …`. */
	bearerToken?: string;
	enabled: boolean;
}

/** Stdio server: a child process speaking JSON-RPC over stdio. Server builds only. */
export interface McpStdioServerConfig {
	transport: 'stdio';
	id: string;
	name?: string;
	/** Executable basename or absolute path. Gated by the server allowlist. */
	command: string;
	args?: string[];
	/** Extra environment variables for the child (minimal base env otherwise). */
	env?: Record<string, string>;
	enabled: boolean;
}

export type McpServerConfig = McpHttpServerConfig | McpStdioServerConfig;

/** One tool advertised by an MCP server. */
export interface McpToolDef {
	name: string;
	description?: string;
	/** JSON Schema for the tool's arguments (may be absent/empty). */
	inputSchema?: Record<string, unknown>;
}

/** Text-ish content block inside a `tools/call` result. */
export interface McpContentBlock {
	type: string;
	text?: string;
	data?: string;
	mimeType?: string;
	[key: string]: unknown;
}

export interface McpToolResult {
	content: McpContentBlock[];
	isError?: boolean;
}

/** Machine-readable failure taxonomy for per-server error surfacing. */
export type McpErrorKind =
	| 'transport'
	| 'timeout'
	| 'protocol'
	| 'rpc'
	| 'disabled'
	| 'forbidden'
	| 'unavailable';

/** HTTP status for a proxy failure kind. */
export function mcpErrorHttpStatus(kind: McpErrorKind): number {
	switch (kind) {
		case 'forbidden':
			return 403;
		case 'timeout':
			return 504;
		case 'disabled':
			return 400;
		default:
			return 502;
	}
}

/** Error carrying the failing server id. Never includes credentials. */
export class McpError extends Error {
	readonly serverId: string;
	readonly kind: McpErrorKind;
	readonly status?: number;
	readonly rpcCode?: number;

	constructor(serverId: string, kind: McpErrorKind, message: string, extra?: { status?: number; rpcCode?: number }) {
		super(message);
		this.name = 'McpError';
		this.serverId = serverId;
		this.kind = kind;
		this.status = extra?.status;
		this.rpcCode = extra?.rpcCode;
	}
}

/** Parse and validate raw persisted/user-supplied server configs. Unknown or
 * malformed entries are dropped (with the reason collected) rather than
 * throwing, so one bad server never breaks the whole list. */
export function parseMcpServerConfigs(raw: unknown): { servers: McpServerConfig[]; dropped: string[] } {
	const servers: McpServerConfig[] = [];
	const dropped: string[] = [];
	if (!Array.isArray(raw)) return { servers, dropped };
	for (const entry of raw) {
		if (!entry || typeof entry !== 'object') {
			dropped.push('entry is not an object');
			continue;
		}
		const record = entry as Record<string, unknown>;
		const id = typeof record.id === 'string' ? record.id.trim() : '';
		if (!id || id.length > 128 || !/^[a-zA-Z0-9\-_.]+$/.test(id)) {
			dropped.push('server id must be 1-128 chars of [a-zA-Z0-9-_.]');
			continue;
		}
		const name = typeof record.name === 'string' && record.name.trim() ? record.name.trim().slice(0, 128) : undefined;
		const enabled = record.enabled !== false;
		if (record.transport === 'http') {
			const url = typeof record.url === 'string' ? record.url.trim() : '';
			if (!url) {
				dropped.push(`server '${id}': http transport needs a url`);
				continue;
			}
			let parsed: URL;
			try {
				parsed = new URL(url);
			} catch {
				dropped.push(`server '${id}': url is not a valid URL`);
				continue;
			}
			if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
				dropped.push(`server '${id}': url must use http or https`);
				continue;
			}
			const bearerToken =
				typeof record.bearerToken === 'string' && record.bearerToken ? record.bearerToken : undefined;
			servers.push({ transport: 'http', id, ...(name ? { name } : {}), url, ...(bearerToken ? { bearerToken } : {}), enabled });
		} else if (record.transport === 'stdio') {
			const command = typeof record.command === 'string' ? record.command.trim() : '';
			if (!command || command.length > 1024 || command.includes('\0')) {
				dropped.push(`server '${id}': stdio transport needs a command (1-1024 chars, no NUL)`);
				continue;
			}
			const args = Array.isArray(record.args)
				? record.args.filter((a): a is string => typeof a === 'string' && !a.includes('\0')).slice(0, 128)
				: undefined;
			let env: Record<string, string> | undefined;
			if (record.env && typeof record.env === 'object' && !Array.isArray(record.env)) {
				env = {};
				for (const [key, value] of Object.entries(record.env as Record<string, unknown>)) {
					if (typeof value === 'string' && !key.includes('\0') && !value.includes('\0')) {
						env[key] = value;
						if (Object.keys(env).length >= 64) break;
					}
				}
			}
			servers.push({
				transport: 'stdio',
				id,
				...(name ? { name } : {}),
				command,
				...(args ? { args } : {}),
				...(env ? { env } : {}),
				enabled
			});
		} else {
			dropped.push(`server '${id}': transport must be 'http' or 'stdio'`);
		}
	}
	return { servers, dropped };
}
