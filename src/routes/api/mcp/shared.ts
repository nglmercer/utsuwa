/** Server-only shared helpers for the MCP proxy routes. */
import { McpError, mcpErrorHttpStatus } from '../../../lib/services/mcp/types.ts';

/** The proxy is inert unless the deployment explicitly opts in. */
export function isMcpProxyEnabled(flag: string | undefined): boolean {
	return flag === 'server';
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
