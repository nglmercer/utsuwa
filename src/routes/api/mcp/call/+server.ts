import { json, type RequestHandler } from '@sveltejs/kit';
import { env } from '$env/dynamic/private';
import { isMcpProxyEnabled, mcpErrorResponse } from '../shared.ts';
import { nodeHostResolver } from '../dns.ts';
import { createGuardedFetch } from '../guarded-fetch.ts';
import { parseAllowedCommands, runStdioMethod } from '../stdio.ts';
import {
	McpError,
	parseMcpServerConfigs,
	type McpServerConfig,
	type McpToolResult
} from '../../../../lib/services/mcp/types.ts';
import { McpHttpClient } from '../../../../lib/services/web/mcp/http-client.ts';

/** Cap on proxied tool arguments (JSON-encoded) per call. */
const MAX_ARGUMENTS_BYTES = 64 * 1024;

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
	try {
		const server = singleServer(body);
		if (!server.enabled) {
			throw new McpError(server.id, 'disabled', `MCP server '${server.id}' is disabled`);
		}
		const tool = typeof body.tool === 'string' ? body.tool.trim() : '';
		if (!tool) {
			throw new McpError(server.id, 'protocol', 'Missing tool name');
		}
		const args =
			body.arguments && typeof body.arguments === 'object' && !Array.isArray(body.arguments)
				? (body.arguments as Record<string, unknown>)
				: {};
		if (JSON.stringify(args).length > MAX_ARGUMENTS_BYTES) {
			throw new McpError(server.id, 'protocol', 'Tool arguments exceed the size cap');
		}
		if (server.transport === 'stdio') {
			const result = await runStdioMethod<McpToolResult>(
				server,
				'tools/call',
				{ name: tool, arguments: args },
				{ allowedCommands: parseAllowedCommands(env.MCP_STDIO_ALLOWED_COMMANDS) }
			);
			return json({ result, sessionId: null });
		}
		const client = new McpHttpClient(server, { fetchImpl: createGuardedFetch(nodeHostResolver) });
		if (typeof body.sessionId === 'string' && body.sessionId) client.adoptSession(body.sessionId);
		const result = await client.callTool(tool, args);
		return json({ result, sessionId: client.activeSessionId });
	} catch (error) {
		return mcpErrorResponse(error);
	}
};
