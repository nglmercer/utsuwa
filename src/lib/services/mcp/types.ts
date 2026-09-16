/** Shared MCP (Model Context Protocol) shapes for both runtimes.
 *
 * This module is the native/web boundary type: pure config shapes,
 * validation, and Rust translators with zero networking. It is safe to
 * import from browser code, native UI, server routes, and unit tests (no
 * node builtins, no `$env` access — callers inject environment values).
 * MCP *execution* lives in `../web/mcp/` (web-only) and in Rust
 * `crates/mcp-runtime` (native).
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

// ---------------------------------------------------------------------------
// Native runtime boundary
// ---------------------------------------------------------------------------
// On native builds the Svelte UI configures MCP but Rust executes it: server
// configs travel through the `mcp.servers` setting (canonical Rust shape) and
// Bearer [REDACTED] through the write-only `mcp.set_server_token` IPC into
// the OS secret store. These translators are the single normalization point
// between the flat UI shape above and the canonical Rust shape; the Rust
// deserializer additionally accepts legacy shapes for old stored configs.

/** Canonical Rust `McpServerConfig` JSON (what `mcp.servers` holds). */
export interface RustMcpServerConfig {
	id: string;
	name?: string;
	transport:
		| {
				type: 'stdio';
				command: string;
				args?: string[];
				env_allowlist?: string[];
				extra_env?: Record<string, string>;
				cwd?: string;
		  }
		| { type: 'http'; url: string };
	enabled: boolean;
	trust: 'Untrusted' | 'Limited' | 'Trusted';
}

/** Translate a validated UI config to the canonical Rust shape. Bearer tokens
 * are deliberately dropped: they must travel through `mcp.set_server_token`,
 * never through settings JSON. */
export function toRustMcpConfig(server: McpServerConfig): RustMcpServerConfig {
	const base = {
		id: server.id,
		...(server.name ? { name: server.name } : {}),
		enabled: server.enabled,
		trust: 'Untrusted' as const
	};
	if (server.transport === 'http') {
		return { ...base, transport: { type: 'http', url: server.url } };
	}
	return {
		...base,
		transport: {
			type: 'stdio',
			command: server.command,
			...(server.args?.length ? { args: server.args } : {}),
			...(server.env && Object.keys(server.env).length > 0 ? { extra_env: server.env } : {})
		}
	};
}

/** Parse canonical Rust configs back into the flat UI shape. Malformed
 * entries are dropped with reasons (same contract as parseMcpServerConfigs).
 * Trust levels are Rust-owned and not surfaced for editing. */
export function fromRustMcpConfigs(raw: unknown): { servers: McpServerConfig[]; dropped: string[] } {
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
		const name =
			typeof record.name === 'string' && record.name.trim()
				? record.name.trim().slice(0, 128)
				: undefined;
		const enabled = record.enabled !== false;
		const transport = record.transport as Record<string, unknown> | undefined;
		const kind = transport && typeof transport === 'object' ? transport.type : undefined;
		if (kind === 'http') {
			const url = typeof transport?.url === 'string' ? (transport.url as string).trim() : '';
			if (!url) {
				dropped.push(`server '${id}': http transport needs a url`);
				continue;
			}
			servers.push({ transport: 'http', id, ...(name ? { name } : {}), url, enabled });
		} else if (kind === 'stdio') {
			const command =
				typeof transport?.command === 'string' ? (transport.command as string).trim() : '';
			if (!command) {
				dropped.push(`server '${id}': stdio transport needs a command`);
				continue;
			}
			const args = Array.isArray(transport?.args)
				? (transport.args as unknown[]).filter((a): a is string => typeof a === 'string')
				: undefined;
			const extra = transport?.extra_env as Record<string, unknown> | undefined;
			let env: Record<string, string> | undefined;
			if (extra && typeof extra === 'object' && !Array.isArray(extra)) {
				env = {};
				for (const [key, value] of Object.entries(extra)) {
					if (typeof value === 'string') env[key] = value;
				}
			}
			servers.push({
				transport: 'stdio',
				id,
				...(name ? { name } : {}),
				command,
				...(args?.length ? { args } : {}),
				...(env && Object.keys(env).length > 0 ? { env } : {}),
				enabled
			});
		} else {
			dropped.push(`server '${id}': transport.type must be 'http' or 'stdio'`);
		}
	}
	return { servers, dropped };
}
