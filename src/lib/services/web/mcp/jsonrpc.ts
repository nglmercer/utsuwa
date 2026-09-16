// WEB-ONLY EXECUTION — never import from native code paths. Native MCP runs in
// Rust (crates/mcp-runtime); this module serves web chat + SvelteKit routes only.
/** Minimal JSON-RPC 2.0 framing for MCP. Pure: no I/O, no env access. */

export interface JsonRpcRequest {
	jsonrpc: '2.0';
	id: number | string;
	method: string;
	params?: unknown;
}

export interface JsonRpcSuccess {
	jsonrpc: '2.0';
	id: number | string | null;
	result: unknown;
}

export interface JsonRpcFailure {
	jsonrpc: '2.0';
	id: number | string | null;
	error: { code: number; message: string; data?: unknown };
}

export type JsonRpcResponse = JsonRpcSuccess | JsonRpcFailure;

let nextId = 1;

/** Build a JSON-RPC request with a unique numeric id. */
export function rpcRequest(method: string, params?: unknown): JsonRpcRequest {
	return { jsonrpc: '2.0', id: nextId++, method, ...(params === undefined ? {} : { params }) };
}

/** Type-guard for a parsed JSON-RPC success/failure envelope. */
export function isRpcResponse(value: unknown): value is JsonRpcResponse {
	if (!value || typeof value !== 'object') return false;
	const record = value as Record<string, unknown>;
	if (record.jsonrpc !== '2.0') return false;
	if (!('id' in record)) return false;
	if ('result' in record) return true;
	const error = record.error as Record<string, unknown> | undefined;
	return (
		!!error && typeof error === 'object' && typeof error.code === 'number' && typeof error.message === 'string'
	);
}

/** Extract the result payload or throw the RPC error as a plain Error. */
export function unwrapRpcResult<T = unknown>(response: JsonRpcResponse): T {
	if ('result' in response) return response.result as T;
	const error = (response as JsonRpcFailure).error;
	const rpcError = new Error(error.message || `RPC error ${error.code}`) as Error & { rpcCode: number; rpcData?: unknown };
	rpcError.rpcCode = error.code;
	if (error.data !== undefined) rpcError.rpcData = error.data;
	throw rpcError;
}
