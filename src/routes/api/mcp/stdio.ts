/** Server-only stdio MCP transport: spawn a child speaking newline-delimited
 * JSON-RPC, run initialize + one method, then terminate it. Stateless per
 * request (no process pool): each call pays one spawn, and no child outlives
 * its request.
 *
 * Fail-closed server policy, two layers:
 *
 * 1. Operator inventory (`MCP_STDIO_SERVERS`, JSON). Clients only name a
 *    server `id`; the command, argv, and env come from the operator's
 *    inventory entry. Client-supplied command/args/env are discarded, so an
 *    internet caller can never turn an allowlisted interpreter (`uvx`, `npx`,
 *    `python3`) into arbitrary code execution via argv. No entry = refused.
 * 2. Command backstop (`MCP_STDIO_ALLOWED_COMMANDS`, comma-separated): the
 *    resolved command (or its basename) must additionally be listed.
 *    Unset/empty = nothing is allowed.
 *
 * Child environment is minimal: a tiny fixed base plus the inventory entry's
 * `env`. The parent environment is never inherited and the client cannot
 * override `PATH` or inject loader variables.
 */
import { McpError, type McpStdioServerConfig } from '../../../lib/services/mcp/types.ts';

export const MCP_PROTOCOL_VERSION = '2025-06-18';
const DEFAULT_TIMEOUT_MS = 30_000;
const MAX_BUFFER_BYTES = 1024 * 1024;
const BASE_ENV: Record<string, string> =
	process.platform === 'win32'
		? { SystemRoot: process.env.SystemRoot ?? 'C:\\Windows', COMSPEC: process.env.ComSpec ?? 'C:\\Windows\\System32\\cmd.exe' }
		: { PATH: '/usr/bin:/bin', LANG: 'C.UTF-8' };

/** Parse the allowlist env var. Returns null when stdio must be fully refused. */
export function parseAllowedCommands(raw: string | undefined): string[] | null {
	if (!raw) return null;
	const entries = raw
		.split(',')
		.map((entry) => entry.trim())
		.filter((entry) => entry.length > 0 && !entry.includes('\0'));
	return entries.length > 0 ? entries : null;
}

function baseName(command: string): string {
	const parts = command.split(/[/\\]/);
	return parts[parts.length - 1];
}

export function isCommandAllowed(command: string, allowed: string[] | null): boolean {
	if (!allowed) return false;
	if (allowed.includes(command)) return true;
	return allowed.includes(baseName(command));
}

/** One operator-pinned stdio server: exact argv, exact env. */
export interface StdioInventoryEntry {
	id: string;
	command: string;
	args: string[];
	env: Record<string, string>;
}

const MAX_INVENTORY_ENTRIES = 64;
const MAX_INVENTORY_ARGS = 128;
const MAX_INVENTORY_ENV_KEYS = 64;
const MAX_INVENTORY_ARG_CHARS = 4096;
const MAX_INVENTORY_ENV_VALUE_CHARS = 8192;
const ENV_KEY_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*$/;

function cleanInventoryString(value: unknown, maxChars: number): string | null {
	if (typeof value !== 'string') return null;
	if (value.includes('\0') || value.length === 0 || value.length > maxChars) return null;
	return value;
}

/**
 * Parse the operator's stdio inventory (`MCP_STDIO_SERVERS`, a JSON array of
 * `{id, command, args?, env?}`). Malformed entries are dropped; malformed
 * JSON yields an empty inventory. Empty inventory = every stdio server
 * refused. Never throws.
 */
export function parseStdioInventory(raw: string | undefined): StdioInventoryEntry[] {
	if (!raw || !raw.trim()) return [];
	let parsed: unknown;
	try {
		parsed = JSON.parse(raw);
	} catch {
		return [];
	}
	if (!Array.isArray(parsed)) return [];
	const entries: StdioInventoryEntry[] = [];
	const seen = new Set<string>();
	for (const item of parsed) {
		if (entries.length >= MAX_INVENTORY_ENTRIES) break;
		if (!item || typeof item !== 'object' || Array.isArray(item)) continue;
		const record = item as Record<string, unknown>;
		const id = cleanInventoryString(record.id, 128);
		const command = cleanInventoryString(record.command, 512);
		if (!id || !command || seen.has(id)) continue;
		let args: string[] = [];
		if (record.args !== undefined) {
			if (!Array.isArray(record.args) || record.args.length > MAX_INVENTORY_ARGS) continue;
			const cleaned: string[] = [];
			let valid = true;
			for (const arg of record.args) {
				if (typeof arg !== 'string' || arg.includes('\0') || arg.length > MAX_INVENTORY_ARG_CHARS) {
					valid = false;
					break;
				}
				cleaned.push(arg);
			}
			if (!valid) continue;
			args = cleaned;
		}
		const env: Record<string, string> = {};
		if (record.env !== undefined) {
			if (!record.env || typeof record.env !== 'object' || Array.isArray(record.env)) continue;
			const pairs = Object.entries(record.env as Record<string, unknown>);
			if (pairs.length > MAX_INVENTORY_ENV_KEYS) continue;
			let valid = true;
			for (const [key, value] of pairs) {
				if (!ENV_KEY_PATTERN.test(key) || typeof value !== 'string' || value.includes('\0') || value.length > MAX_INVENTORY_ENV_VALUE_CHARS) {
					valid = false;
					break;
				}
				env[key] = value;
			}
			if (!valid) continue;
		}
		seen.add(id);
		entries.push({ id, command, args, env });
	}
	return entries;
}

/**
 * Resolve a client-named stdio server against the operator inventory. The
 * returned config carries the inventory's pinned command/argv/env — the
 * client's values are discarded. Throws `McpError` (forbidden) when the id
 * has no entry.
 */
export function resolveStdioServer(
	server: McpStdioServerConfig,
	inventory: StdioInventoryEntry[]
): McpStdioServerConfig {
	const entry = inventory.find((candidate) => candidate.id === server.id);
	if (!entry) {
		throw new McpError(
			server.id,
			'forbidden',
			`MCP server '${server.id}' is not in the server's stdio inventory`
		);
	}
	return {
		...server,
		command: entry.command,
		args: [...entry.args],
		env: { ...entry.env }
	};
}

/**
 * Route choke point: parse the raw inventory env var, warn when it yields
 * nothing usable, and resolve the client-named server to its pinned argv/env.
 */
export function pinnedStdioServer(
	server: McpStdioServerConfig,
	rawInventory: string | undefined
): McpStdioServerConfig {
	const inventory = parseStdioInventory(rawInventory);
	if (rawInventory && inventory.length === 0) {
		console.warn('MCP proxy: MCP_STDIO_SERVERS parsed to zero entries; refusing stdio servers');
	}
	return resolveStdioServer(server, inventory);
}

export interface StdioCallOptions {
	allowedCommands: string[] | null;
	timeoutMs?: number;
}

interface PendingRead {
	id: number | string;
	resolve: (value: unknown) => void;
	reject: (error: Error) => void;
}

/** Run `method` against a stdio server: spawn, handshake, call, terminate. */
export async function runStdioMethod<T = unknown>(
	server: McpStdioServerConfig,
	method: string,
	params: unknown,
	options: StdioCallOptions
): Promise<T> {
	if (!isCommandAllowed(server.command, options.allowedCommands)) {
		throw new McpError(
			server.id,
			'forbidden',
			`MCP server '${server.id}': stdio command '${server.command}' is not in the server allowlist`
		);
	}
	const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
	const { spawn } = await import('node:child_process');
	const child = spawn(server.command, server.args ?? [], {
		env: { ...BASE_ENV, ...(server.env ?? {}) },
		stdio: ['pipe', 'pipe', 'pipe'],
		windowsHide: true
	});

	let settled = false;
	let bufferedBytes = 0;
	let lineBuffer = '';
	let stderrTail = '';
	let nextId = 1;
	const pending = new Map<number | string, PendingRead>();

	const failAll = (error: Error) => {
		for (const entry of pending.values()) entry.reject(error);
		pending.clear();
	};

	const finish = () => {
		if (settled) return;
		settled = true;
		clearTimeout(timer);
		try {
			child.kill('SIGKILL');
		} catch {
			// Already exited.
		}
	};

	const timer = setTimeout(() => {
		failAll(new McpError(server.id, 'timeout', `MCP server '${server.id}' stdio call timed out after ${timeoutMs}ms`));
		finish();
	}, timeoutMs);
	// Let the process exit naturally in tests if handles linger.
	(timer as unknown as { unref?: () => void }).unref?.();

	child.on('error', (error: Error) => {
		failAll(
			new McpError(server.id, 'transport', `MCP server '${server.id}' failed to spawn: ${error.message}`)
		);
		finish();
	});
	child.on('exit', (code: number | null) => {
		if (pending.size > 0) {
			const tail = stderrTail.trim().slice(-500);
			failAll(
				new McpError(
					server.id,
					'protocol',
					`MCP server '${server.id}' exited (code ${code ?? 'unknown'})${tail ? `: ${tail}` : ''}`
				)
			);
		}
		finish();
	});
	child.stderr?.on('data', (chunk: Buffer) => {
		stderrTail += chunk.toString('utf8').slice(-2000);
		stderrTail = stderrTail.slice(-2000);
	});
	child.stdout?.on('data', (chunk: Buffer) => {
		bufferedBytes += chunk.byteLength;
		if (bufferedBytes > MAX_BUFFER_BYTES) {
			failAll(new McpError(server.id, 'protocol', `MCP server '${server.id}' exceeded the output cap`));
			finish();
			return;
		}
		lineBuffer += chunk.toString('utf8');
		let newline: number;
		while ((newline = lineBuffer.indexOf('\n')) !== -1) {
			const line = lineBuffer.slice(0, newline).trim();
			lineBuffer = lineBuffer.slice(newline + 1);
			if (!line) continue;
			let message: Record<string, unknown>;
			try {
				message = JSON.parse(line) as Record<string, unknown>;
			} catch {
				continue; // Non-JSON chatter (banners, warnings) is skipped.
			}
			if (message.jsonrpc !== '2.0' || !('id' in message)) continue; // Notification: ignore.
			const entry = pending.get(message.id as number | string);
			if (entry) {
				pending.delete(message.id as number | string);
				entry.resolve(message);
			}
		}
	});

	const send = (payload: Record<string, unknown>): Promise<Record<string, unknown>> =>
		new Promise((resolve, reject) => {
			if (settled) {
				reject(new McpError(server.id, 'transport', `MCP server '${server.id}' is no longer running`));
				return;
			}
			const id = payload.id as number | string | undefined;
			if (id !== undefined) pending.set(id, { id, resolve: resolve as (value: unknown) => void, reject });
			child.stdin?.write(`${JSON.stringify(payload)}\n`, (error) => {
				if (error) {
					if (id !== undefined) pending.delete(id);
					reject(
						new McpError(server.id, 'transport', `MCP server '${server.id}' write failed: ${error.message}`)
					);
				} else if (id === undefined) {
					resolve({});
				}
			});
		});

	try {
		const initId = nextId++;
		const init = await send({
			jsonrpc: '2.0',
			id: initId,
			method: 'initialize',
			params: {
				protocolVersion: MCP_PROTOCOL_VERSION,
				capabilities: {},
				clientInfo: { name: 'utsuwa-companion', version: '1.0.0' }
			}
		});
		if (init.error) {
			const rpc = init.error as { code?: number; message?: string };
			throw new McpError(server.id, 'rpc', `MCP server '${server.id}' initialize failed: ${rpc.message ?? rpc.code}`, {
				rpcCode: typeof rpc.code === 'number' ? rpc.code : undefined
			});
		}
		await send({ jsonrpc: '2.0', method: 'notifications/initialized' });
		const callId = nextId++;
		const answer = await send({ jsonrpc: '2.0', id: callId, method, params });
		if (answer.error) {
			const rpc = answer.error as { code?: number; message?: string };
			throw new McpError(server.id, 'rpc', `MCP server '${server.id}' error: ${rpc.message ?? rpc.code}`, {
				rpcCode: typeof rpc.code === 'number' ? rpc.code : undefined
			});
		}
		return answer.result as T;
	} finally {
		finish();
	}
}
