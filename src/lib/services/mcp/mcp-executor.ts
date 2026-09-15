/** Executes MCP tools for the companion chat loop, either directly (desktop
 * builds talk to HTTP MCP servers from the webview) or through the same-origin
 * `/api/mcp` proxy (web builds).
 *
 * Responsibilities: per-server client/session caching, model-facing tool
 * naming (`<server>__<tool>`), the 8000-char result cap, and the
 * never-auto-execute confirm list. Transport and protocol errors surface per
 * server id so the Settings UI and the chat loop can report them precisely.
 */
import { McpHttpClient, type FetchImpl } from './http-client.ts';
import { McpError, type McpServerConfig, type McpToolDef, type McpToolResult } from './types.ts';

export const MAX_TOOL_RESULT_CHARS = 8000;

/** Model-facing tool definition (OpenAI function shape; converted for Anthropic). */
export interface ChatToolDefinition {
	name: string;
	description?: string;
	parameters?: Record<string, unknown>;
}

/** Sanitize one name segment to the provider function-name charset. */
export function sanitizeToolNameSegment(segment: string): string {
	return segment.replace(/[^a-zA-Z0-9_-]/g, '_').slice(0, 48) || 'tool';
}

/** Join server + tool into the model-facing name, parsed back by splitToolName. */
export function joinToolName(serverId: string, toolName: string): string {
	return `${sanitizeToolNameSegment(serverId)}__${sanitizeToolNameSegment(toolName)}`.slice(0, 64);
}

export function splitToolName(fullName: string): { serverId: string; tool: string } | null {
	const separator = fullName.indexOf('__');
	if (separator <= 0 || separator + 2 >= fullName.length) return null;
	return { serverId: fullName.slice(0, separator), tool: fullName.slice(separator + 2) };
}

/** Flatten MCP content blocks into model-readable text. */
export function toolResultToText(result: McpToolResult): string {
	const parts: string[] = [];
	for (const block of result.content ?? []) {
		if (typeof block.text === 'string') {
			parts.push(block.text);
		} else if (block.type === 'image') {
			parts.push(`[image omitted${block.mimeType ? `: ${block.mimeType}` : ''}]`);
		} else if (typeof block.data === 'string') {
			parts.push(`[${block.type || 'data'} omitted]`);
		} else {
			parts.push(`[${block.type || 'unknown'} content omitted]`);
		}
	}
	const text = parts.join('\n');
	return result.isError ? `Tool error: ${text}` : text;
}

export function capResultText(text: string, maxChars: number = MAX_TOOL_RESULT_CHARS): string {
	if (text.length <= maxChars) return text;
	return `${text.slice(0, maxChars)}…[truncated to ${maxChars} chars]`;
}

export type McpExecutorMode = 'direct' | 'proxy';

export interface McpExecutorOptions {
	mode: McpExecutorMode;
	/** Used for proxy POSTs (and direct clients when provided, e.g. tests). */
	fetchImpl?: FetchImpl;
	/** Same-origin prefix for proxy routes (default ''). */
	proxyBase?: string;
	/** Full (`server__tool`) or bare tool names that are never auto-executed. */
	confirmTools?: string[];
	toolTimeoutMs?: number;
}

interface ResolvedTool {
	serverId: string;
	serverTool: string;
	def: McpToolDef;
}

export class McpToolExecutor {
	private readonly servers: McpServerConfig[];
	private readonly options: McpExecutorOptions;
	private readonly directClients = new Map<string, McpHttpClient>();
	private readonly proxySessions = new Map<string, string>();
	private readonly resolved = new Map<string, ResolvedTool>();
	private readonly serverErrors = new Map<string, string>();

	constructor(servers: McpServerConfig[], options: McpExecutorOptions) {
		this.servers = servers;
		this.options = options;
	}

	/** Latest per-server failure, keyed by server id (cleared on success). */
	get errors(): ReadonlyMap<string, string> {
		return this.serverErrors;
	}

	private get fetchImpl(): FetchImpl {
		return this.options.fetchImpl ?? ((url, init) => fetch(url, init));
	}

	private noteError(serverId: string, error: unknown): void {
		this.serverErrors.set(serverId, error instanceof Error ? error.message.slice(0, 300) : String(error).slice(0, 300));
	}

	private clearError(serverId: string): void {
		this.serverErrors.delete(serverId);
	}

	/** All tools from every enabled, reachable server. Unreachable servers are
	 * skipped (recorded in `errors`) so one down server never breaks chat. */
	async definitions(): Promise<ChatToolDefinition[]> {
		const definitions: ChatToolDefinition[] = [];
		this.resolved.clear();
		for (const server of this.servers) {
			if (!server.enabled) continue;
			if (server.transport === 'stdio' && this.options.mode === 'direct') {
				this.noteError(server.id, 'stdio servers need the web proxy (unavailable in this build)');
				continue;
			}
			try {
				const tools = this.options.mode === 'direct' ? await this.listDirect(server) : await this.listViaProxy(server);
				this.clearError(server.id);
				for (const tool of tools) {
					const fullName = joinToolName(server.id, tool.name);
					if (this.resolved.has(fullName)) continue;
					this.resolved.set(fullName, { serverId: server.id, serverTool: tool.name, def: tool });
					definitions.push({
						name: fullName,
						description: tool.description || `MCP tool ${tool.name} on server ${server.id}`,
						parameters:
							tool.inputSchema && typeof tool.inputSchema === 'object'
								? (tool.inputSchema as Record<string, unknown>)
								: { type: 'object', properties: {} }
					});
				}
			} catch (error) {
				this.noteError(server.id, error);
			}
		}
		return definitions;
	}

	private directClient(serverId: string): McpHttpClient | undefined {
		return this.directClients.get(serverId);
	}

	private async listDirect(server: McpServerConfig): Promise<McpToolDef[]> {
		if (server.transport !== 'http') throw new McpError(server.id, 'unavailable', 'stdio is unavailable for direct calls');
		let client = this.directClient(server.id);
		if (!client) {
			client = new McpHttpClient(server, {
				...(this.options.fetchImpl ? { fetchImpl: this.options.fetchImpl } : {}),
				...(this.options.toolTimeoutMs !== undefined ? { timeoutMs: this.options.toolTimeoutMs } : {})
			});
			this.directClients.set(server.id, client);
		}
		return client.listTools();
	}

	private async proxyPost<T>(path: string, payload: Record<string, unknown>): Promise<T> {
		const base = this.options.proxyBase ?? '';
		const response = await this.fetchImpl(`${base}/api/mcp/${path}`, {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify(payload)
		});
		let parsed: Record<string, unknown> | null = null;
		try {
			parsed = (await response.json()) as Record<string, unknown>;
		} catch {
			// Fall through to the generic error below.
		}
		if (!response.ok || !parsed || typeof parsed !== 'object') {
			const message =
				parsed && typeof parsed.error === 'string' ? parsed.error : `MCP proxy ${path} failed (${response.status})`;
			throw new McpError(typeof parsed?.serverId === 'string' ? (parsed.serverId as string) : 'proxy', 'transport', message.slice(0, 300));
		}
		return parsed as T;
	}

	private async listViaProxy(server: McpServerConfig): Promise<McpToolDef[]> {
		const answer = await this.proxyPost<{ tools?: McpToolDef[]; sessionId?: string | null }>('tools', {
			server,
			sessionId: this.proxySessions.get(server.id) ?? null
		});
		if (answer.sessionId) this.proxySessions.set(server.id, answer.sessionId);
		return Array.isArray(answer.tools) ? answer.tools : [];
	}

	/** Execute one model-facing tool by full name. Always resolves to capped
	 * text (failures become readable results, never throws), except for
	 * unknown tool names, which throw. */
	async execute(fullName: string, argsText: string): Promise<string> {
		const resolved = this.resolved.get(fullName);
		if (!resolved) throw new Error(`Unknown tool '${fullName}'`);
		const confirmList = this.options.confirmTools ?? [];
		if (confirmList.includes(fullName) || confirmList.includes(resolved.serverTool)) {
			return `Tool '${fullName}' requires confirmation and was not executed automatically.`;
		}
		let args: Record<string, unknown>;
		try {
			const parsed: unknown = argsText.trim() ? JSON.parse(argsText) : {};
			if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
				return `Tool '${fullName}' arguments must be a JSON object.`;
			}
			args = parsed as Record<string, unknown>;
		} catch {
			return `Tool '${fullName}' arguments were not valid JSON and the call was not executed.`;
		}
		const server = this.servers.find((s) => s.id === resolved.serverId);
		if (!server || !server.enabled) return `Tool '${fullName}' is no longer available.`;
		try {
			const result =
				this.options.mode === 'direct'
					? await this.callDirect(server, resolved.serverTool, args)
					: await this.callViaProxy(server, resolved.serverTool, args);
			this.clearError(server.id);
			return capResultText(toolResultToText(result));
		} catch (error) {
			this.noteError(server.id, error);
			const message = error instanceof Error ? error.message : String(error);
			return `Tool '${fullName}' failed: ${message.slice(0, 500)}`;
		}
	}

	private async callDirect(
		server: McpServerConfig,
		tool: string,
		args: Record<string, unknown>
	): Promise<McpToolResult> {
		if (server.transport !== 'http') {
			throw new McpError(server.id, 'unavailable', 'stdio is unavailable for direct calls');
		}
		let client = this.directClient(server.id);
		if (!client) {
			client = new McpHttpClient(server, {
				...(this.options.fetchImpl ? { fetchImpl: this.options.fetchImpl } : {}),
				...(this.options.toolTimeoutMs !== undefined ? { timeoutMs: this.options.toolTimeoutMs } : {})
			});
			this.directClients.set(server.id, client);
		}
		return client.callTool(tool, args);
	}

	private async callViaProxy(
		server: McpServerConfig,
		tool: string,
		args: Record<string, unknown>
	): Promise<McpToolResult> {
		const answer = await this.proxyPost<{ result?: McpToolResult; sessionId?: string | null }>(
			'call',
			{
				server,
				tool,
				arguments: args,
				sessionId: this.proxySessions.get(server.id) ?? null
			}
		);
		if (answer.sessionId) this.proxySessions.set(server.id, answer.sessionId);
		if (!answer.result || !Array.isArray(answer.result.content)) {
			throw new McpError(server.id, 'protocol', `MCP proxy answer for '${tool}' was malformed`);
		}
		return answer.result;
	}
}
