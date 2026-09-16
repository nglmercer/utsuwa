/** Server-only shared helpers for the MCP proxy routes. */
import {
	McpError,
	mcpErrorHttpStatus,
	type McpHttpServerConfig
} from '../../../lib/services/mcp/types.ts';

/** The proxy is inert unless the deployment explicitly opts in. */
export function isMcpProxyEnabled(flag: string | undefined): boolean {
	return flag === 'server';
}

/** Parse `MCP_HTTP_ALLOWED_HOSTS` (comma/whitespace separated). Unset/empty = unrestricted. */
export function parseHttpAllowedHosts(raw: string | undefined): string[] {
	if (!raw) return [];
	const hosts = raw
		.split(/[,\s]+/)
		.map((host) => host.trim().toLowerCase().replace(/\.$/, ''))
		.filter((host) => host.length > 0);
	return [...new Set(hosts)];
}

/** True when a URL's host is covered by the parsed allowlist. Empty list = allow all. */
export function isHttpHostAllowed(url: string, allowedHosts: string[]): boolean {
	if (allowedHosts.length === 0) return true;
	let host: string;
	try {
		host = new URL(url).hostname.toLowerCase().replace(/\.$/, '');
	} catch {
		return false;
	}
	if (!host) return false;
	return allowedHosts.includes(host);
}

/** Refuse an HTTP MCP server whose host the operator did not allowlist. */
export function assertHttpHostAllowed(
	server: McpHttpServerConfig,
	rawAllowlist: string | undefined
): void {
	if (!isHttpHostAllowed(server.url, parseHttpAllowedHosts(rawAllowlist))) {
		throw new McpError(server.id, 'forbidden', 'MCP HTTP host is not allowlisted');
	}
}

export function mcpErrorResponse(error: unknown): Response {
	if (error instanceof McpError) {
		return Response.json(
			{ error: error.message.slice(0, 500), serverId: error.serverId, kind: error.kind },
			{ status: mcpErrorHttpStatus(error.kind) }
		);
	}
	const message = error instanceof Error ? error.message : 'MCP proxy failed';
	return Response.json({ error: message.slice(0, 500) }, { status: 500 });
}
