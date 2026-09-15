/** Streamable HTTP MCP client (browser- and server-safe: no node builtins).
 *
 * Speaks JSON-RPC over POST with `Accept: application/json, text/event-stream`
 * and understands both single-JSON and SSE-stream answers. Manages the
 * `Mcp-Session-Id` lifecycle (capture on initialize, resend, refresh on 404)
 * and probes a trailing-slash URL variant once for strict routers that 404/405
 * the configured shape (seen with Home Assistant frontends).
 */
import { rpcRequest, isRpcResponse, unwrapRpcResult, type JsonRpcResponse } from './jsonrpc.ts';
import { parseSseJsonPayloads } from './sse.ts';
import { McpError, type McpHttpServerConfig, type McpToolDef, type McpToolResult } from './types.ts';

export const MCP_PROTOCOL_VERSION = '2025-06-18';
const MCP_CLIENT_NAME = 'utsuwa-companion';
const MCP_CLIENT_VERSION = '1.0.0';
const DEFAULT_TIMEOUT_MS = 30_000;

export type FetchImpl = (url: string, init: RequestInit) => Promise<Response>;

export interface McpHttpClientOptions {
	fetchImpl?: FetchImpl;
	timeoutMs?: number;
}

function bearerHeaders(config: McpHttpServerConfig): Record<string, string> {
	if (config.bearerToken) return { Authorization: `Bearer ${config.bearerToken}` };
	return {};
}

/** Toggle a trailing slash: `…/mcp` ⇄ `…/mcp/`. Query strings are preserved. */
export function toggleTrailingSlash(rawUrl: string): string | null {
	try {
		const url = new URL(rawUrl);
		if (url.pathname.endsWith('/')) {
			url.pathname = url.pathname.replace(/\/+$/, '') || '/';
			// Toggling root '/' would produce an empty path; not a useful probe.
			if (url.pathname === '/') return null;
		} else {
			url.pathname = `${url.pathname}/`;
		}
		return url.toString();
	} catch {
		return null;
	}
}

export class McpHttpClient {
	private readonly config: McpHttpServerConfig;
	private readonly fetchImpl: FetchImpl;
	private readonly timeoutMs: number;
	private endpoint: string;
	private sessionId: string | null = null;
	private initialized = false;

	constructor(config: McpHttpServerConfig, options: McpHttpClientOptions = {}) {
		this.config = config;
		this.fetchImpl = options.fetchImpl ?? ((url, init) => fetch(url, init));
		this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
		this.endpoint = config.url;
	}

	get serverId(): string {
		return this.config.id;
	}

	get activeUrl(): string {
		return this.endpoint;
	}

	get activeSessionId(): string | null {
		return this.sessionId;
	}

	/** Adopt a caller-held session id (the stateless proxy passes the id
	 * back and forth instead of storing it). Marks the client initialized
	 * so no handshake runs. */
	adoptSession(sessionId: string): void {
		this.sessionId = sessionId;
		this.initialized = true;
	}

	private fail(kind: 'transport' | 'timeout' | 'protocol' | 'rpc', message: string, extra?: { status?: number; rpcCode?: number }): never {
		throw new McpError(this.config.id, kind, message, extra);
	}

	private async postRaw(body: unknown): Promise<Response> {
		const headers: Record<string, string> = {
			'Content-Type': 'application/json',
			Accept: 'application/json, text/event-stream',
			'MCP-Protocol-Version': MCP_PROTOCOL_VERSION,
			...bearerHeaders(this.config)
		};
		if (this.sessionId) headers['Mcp-Session-Id'] = this.sessionId;
		let response: Response;
		try {
			response = await this.fetchImpl(this.endpoint, {
				method: 'POST',
				headers,
				body: JSON.stringify(body),
				signal: AbortSignal.timeout(this.timeoutMs)
			});
		} catch (error) {
			if (error instanceof Error && error.name === 'TimeoutError') {
				this.fail('timeout', `MCP request to '${this.config.id}' timed out after ${this.timeoutMs}ms`);
			}
			throw error instanceof Error ? error : new Error(String(error));
		}
		const session = response.headers.get('mcp-session-id');
		if (session) this.sessionId = session;
		return response;
	}

	/** Parse a POST answer into the JSON-RPC response matching `requestId`.
	 * Returns null for 202 notification acknowledgements (no body). */
	private async readRpcResponse(response: Response, requestId: number | string): Promise<JsonRpcResponse | null> {
		const contentType = response.headers.get('content-type') || '';
		if (response.status === 202) return null;
		if (contentType.includes('text/html')) {
			this.fail('protocol', `MCP server '${this.config.id}' returned a web page, not MCP (wrong URL?)`, { status: response.status });
		}
		if (contentType.includes('text/event-stream')) {
			const text = await response.text();
			let payloads: unknown[];
			try {
				payloads = parseSseJsonPayloads(text);
			} catch {
				this.fail('protocol', `MCP server '${this.config.id}' returned malformed SSE`);
			}
			const match = (payloads as unknown[]).find(
				(p) => isRpcResponse(p) && (p as JsonRpcResponse).id === requestId
			) as JsonRpcResponse | undefined;
			if (match) return match;
			if (payloads.length === 1 && isRpcResponse(payloads[0])) return payloads[0] as JsonRpcResponse;
			this.fail('protocol', `MCP server '${this.config.id}' SSE answer carried no matching response`);
		}
		let parsed: unknown;
		try {
			parsed = await response.json();
		} catch {
			this.fail('protocol', `MCP server '${this.config.id}' returned malformed JSON`, { status: response.status });
		}
		const candidates = Array.isArray(parsed) ? parsed : [parsed];
		const match = candidates.find((p) => isRpcResponse(p) && (p as JsonRpcResponse).id === requestId) as
			| JsonRpcResponse
			| undefined;
		if (match) return match;
		if (candidates.length === 1 && isRpcResponse(candidates[0])) return candidates[0] as JsonRpcResponse;
		this.fail('protocol', `MCP server '${this.config.id}' answer carried no matching response`, { status: response.status });
	}

	/** Single POST attempt. Captures the session id from any response. */
	private async sendOnce(
		method: string,
		params?: unknown
	): Promise<{ wireId: number | string; response: Response }> {
		const wire = rpcRequest(method, params);
		const headers: Record<string, string> = {
			'Content-Type': 'application/json',
			Accept: 'application/json, text/event-stream',
			'MCP-Protocol-Version': MCP_PROTOCOL_VERSION,
			...bearerHeaders(this.config)
		};
		if (this.sessionId) headers['Mcp-Session-Id'] = this.sessionId;
		let response: Response;
		try {
			response = await this.fetchImpl(this.endpoint, {
				method: 'POST',
				headers,
				body: JSON.stringify(wire),
				signal: AbortSignal.timeout(this.timeoutMs)
			});
		} catch (error) {
			if (error instanceof Error && error.name === 'TimeoutError') {
				this.fail('timeout', `MCP request to '${this.config.id}' timed out after ${this.timeoutMs}ms`);
			}
			throw error instanceof Error ? error : new Error(String(error));
		}
		const session = response.headers.get('mcp-session-id');
		if (session) this.sessionId = session;
		return { wireId: wire.id as number | string, response };
	}

	private async parseResponse<T>(response: Response, wireId: number | string): Promise<T> {
		if (!response.ok) {
			const text = await response.text().catch(() => '').then((t) => t.slice(0, 300));
			this.fail('transport', `MCP server '${this.config.id}' HTTP ${response.status}${text ? `: ${text}` : ''}`, {
				status: response.status
			});
		}
		const envelope = await this.readRpcResponse(response, wireId);
		if (!envelope) return undefined as T;
		try {
			return unwrapRpcResult<T>(envelope);
		} catch (error) {
			if (error instanceof Error && 'rpcCode' in error) {
				const rpcCode = (error as Error & { rpcCode: number }).rpcCode;
				this.fail('rpc', `MCP server '${this.config.id}' error: ${error.message}`, { rpcCode });
			}
			throw error;
		}
	}

	/** Probe the trailing-slash URL variant once. Returns true when the
	 * endpoint switched and the caller should retry. */
	private probeSlashVariant(status: number): boolean {
		if (status !== 404 && status !== 405) return false;
		if (this.endpoint !== this.config.url) return false;
		const alternate = toggleTrailingSlash(this.config.url);
		if (!alternate) return false;
		this.endpoint = alternate;
		return true;
	}

	private initializeParams(): Record<string, unknown> {
		return {
			protocolVersion: MCP_PROTOCOL_VERSION,
			capabilities: {},
			clientInfo: { name: MCP_CLIENT_NAME, version: MCP_CLIENT_VERSION }
		};
	}

	/** Run initialize + initialized notification. Idempotent per instance.
	 * Never triggers a session refresh itself, so refresh callers cannot
	 * recurse through it. */
	async initialize(): Promise<{ serverInfo?: unknown; instructions?: unknown }> {
		if (!this.config.enabled) {
			throw new McpError(this.config.id, 'disabled', `MCP server '${this.config.id}' is disabled`);
		}
		let { wireId, response } = await this.sendOnce('initialize', this.initializeParams());
		if (this.probeSlashVariant(response.status)) {
			({ wireId, response } = await this.sendOnce('initialize', this.initializeParams()));
		}
		const result = await this.parseResponse<{
			protocolVersion?: string;
			serverInfo?: unknown;
			instructions?: unknown;
		}>(response, wireId);
		this.initialized = true;
		// Fire-and-forget per spec; failures here must not break the session.
		try {
			await this.postRaw({ jsonrpc: '2.0', method: 'notifications/initialized' });
		} catch {
			// Best effort only.
		}
		return { serverInfo: result?.serverInfo, instructions: result?.instructions };
	}

	/** One JSON-RPC round trip with a bounded retry budget: at most one
	 * session refresh (re-initialize + retry) and at most one slash-probe
	 * retry. The tracked wire id always matches the request actually sent. */
	async callMethod<T = unknown>(method: string, params?: unknown): Promise<T> {
		let { wireId, response } = await this.sendOnce(method, params);
		if ((response.status === 404 || response.status === 410) && this.sessionId) {
			this.sessionId = null;
			this.initialized = false;
			await this.initialize();
			({ wireId, response } = await this.sendOnce(method, params));
		}
		if (this.probeSlashVariant(response.status)) {
			({ wireId, response } = await this.sendOnce(method, params));
		}
		return this.parseResponse<T>(response, wireId);
	}

	private async ensureInitialized(): Promise<void> {
		if (!this.initialized) await this.initialize();
	}

	async listTools(): Promise<McpToolDef[]> {
		await this.ensureInitialized();
		const result = await this.callMethod<{ tools?: McpToolDef[] }>('tools/list', {});
		if (!result || !Array.isArray(result.tools)) {
			this.fail('protocol', `MCP server '${this.config.id}' tools/list answer was malformed`);
		}
		return (result.tools as McpToolDef[]).filter((t) => t && typeof t.name === 'string');
	}

	async callTool(name: string, args?: Record<string, unknown>): Promise<McpToolResult> {
		await this.ensureInitialized();
		const result = await this.callMethod<McpToolResult>('tools/call', {
			name,
			arguments: args ?? {}
		});
		if (!result || !Array.isArray(result.content)) {
			this.fail('protocol', `MCP server '${this.config.id}' tools/call answer was malformed`);
		}
		return result;
	}

	/** Best-effort session termination (HTTP DELETE per spec). */
	async close(): Promise<void> {
		const session = this.sessionId;
		this.sessionId = null;
		this.initialized = false;
		if (!session) return;
		try {
			await this.fetchImpl(this.endpoint, {
				method: 'DELETE',
				headers: { 'Mcp-Session-Id': session, ...bearerHeaders(this.config) },
				signal: AbortSignal.timeout(5000)
			});
		} catch {
			// Best effort only.
		}
	}
}
