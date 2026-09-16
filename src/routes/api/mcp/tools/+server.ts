import { json, type RequestHandler } from '@sveltejs/kit';
import { env } from '$env/dynamic/private';
import { assertHttpHostAllowed, isMcpProxyEnabled, mcpErrorResponse } from '../shared.ts';
import { nodeHostResolver } from '../../dns.ts';
import { createGuardedFetch } from '../guarded-fetch.ts';
import { parseAllowedCommands, pinnedStdioServer, runStdioMethod } from '../stdio.ts';
import {
	McpError,
	parseMcpServerConfigs,
	type McpServerConfig
} from '../../../../lib/services/mcp/types.ts';
import { McpHttpClient } from '../../../../lib/services/web/mcp/http-client.ts';

function singleServer(body: Record<string, unknown>): McpServerConfig {
	const { servers, dropped } = parseMcpServerConfigs([body.server]);
	if (servers.length !== 1) {
		throw new McpError('proxy', 'protocol', `Invalid server config: ${dropped[0] ?? 'unknown'}`);
	}
	return servers[0];
}

export const POST: RequestHandler = async ({ request }) => {
	if (!isMcpProxyEnabled(env.MCP_ENABLED)) {
		return json({ error: 'MCP proxy is disabled on this server' }, { status: 403 });
	}
	let body: Record<string, unknown>;
	try {
		body = (await request.json()) as Record<string, unknown>;
	} catch {
		return json({ error: 'Request body must be JSON' }, { status: 400 });
	}
	// Backstop for chunked bodies without a content-length.
	if (JSON.stringify(body).length > 512 * 1024) {
		return json({ error: 'Request body too large' }, { status: 413 });
	}
	try {
		const server = singleServer(body);
		if (!server.enabled) {
			throw new McpError(server.id, 'disabled', `MCP server '${server.id}' is disabled`);
		}
		if (server.transport === 'stdio') {
			const pinned = pinnedStdioServer(server, env.MCP_STDIO_SERVERS);
			const result = await runStdioMethod<{ tools?: unknown }>(pinned, 'tools/list', {}, {
				allowedCommands: parseAllowedCommands(env.MCP_STDIO_ALLOWED_COMMANDS)
			});
			const tools = Array.isArray(result?.tools) ? result.tools : [];
			return json({ tools, sessionId: null });
		}
		assertHttpHostAllowed(server, env.MCP_HTTP_ALLOWED_HOSTS);
		const client = new McpHttpClient(server, { fetchImpl: createGuardedFetch(nodeHostResolver) });
		if (typeof body.sessionId === 'string' && body.sessionId) client.adoptSession(body.sessionId);
		const tools = await client.listTools();
		// The session stays alive server-side: the caller passes the id back
		// on later calls, so this route must NOT close it here.
		return json({ tools, sessionId: client.activeSessionId });
	} catch (error) {
		return mcpErrorResponse(error);
	}
};
