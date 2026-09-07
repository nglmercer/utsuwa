// Pure permission-request logic for the native-host approval dialog.
// Framework-free so it runs under `node --test`. The reactive store lives
// in `permissions.svelte.ts`; rendering in `PermissionDialog.svelte`.

export type GrantLifetime = 'once' | 'task' | 'session' | 'persistent';

export type LifetimeChoice = GrantLifetime | 'deny';

export interface ResourceSummary {
	kind: 'path' | 'host' | 'executable' | 'application' | 'window' | 'unknown';
	label: string;
}

export interface PermissionRequest {
	id: string;
	principal: string;
	capability: string;
	resource: ResourceSummary;
	reason: string;
}

export type RiskLevel = 'observe' | 'mutate' | 'control';

/** Capabilities that change or act on the world vs. read-only observation. */
export function riskLevel(capability: string): RiskLevel {
	const base = capability.toLowerCase().replace(/[^a-z]/g, '');
	if (/(write|create|delete|move|patch|signal|launch)/.test(base)) return 'mutate';
	if (/(control|capture|spawn|connect|shell)/.test(base)) return 'control';
	return 'observe';
}

/** Human-readable one-liner for a capability id like `FilesystemRead`. */
export function formatCapability(capability: string): string {
	const spaced = capability
		.replace(/([a-z])([A-Z])/g, '$1 $2')
		.replace(/_/g, ' ')
		.toLowerCase();
	return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/** Best-effort parse of the Rust `PendingRequest` JSON from a host event.
 * Returns null when the payload is not a recognizable request — the dialog
 * never renders garbage, and the Rust side keeps the request queued. */
export function parsePermissionRequest(data: unknown): PermissionRequest | null {
	if (typeof data !== 'object' || data === null) return null;
	const d = data as Record<string, unknown>;
	if (typeof d.id !== 'string' || d.id.length === 0) return null;
	if (typeof d.capability !== 'string' || d.capability.length === 0) return null;
	return {
		id: d.id,
		principal: summarizePrincipal(d.principal),
		capability: d.capability,
		resource: summarizeResource(d.resource),
		reason: typeof d.reason === 'string' ? d.reason : ''
	};
}

function summarizePrincipal(principal: unknown): string {
	if (typeof principal === 'string') return principal;
	if (typeof principal !== 'object' || principal === null) return 'Unknown';
	const keys = Object.keys(principal);
	if (keys.length === 0) return 'Unknown';
	const kind = keys[0];
	const value = (principal as Record<string, unknown>)[kind];
	if (kind === 'Agent') return 'Agent';
	if (kind === 'BuiltinTool') return `Tool ${String(value)}`;
	if (kind === 'WasmPlugin') return `Plugin ${String(value)}`;
	if (kind === 'McpServer') return `MCP ${String(value)}`;
	if (kind === 'NativePlugin') return `Native plugin ${String(value)}`;
	return kind;
}

function summarizeResource(resource: unknown): ResourceSummary {
	if (typeof resource === 'string') return { kind: 'unknown', label: resource };
	if (typeof resource !== 'object' || resource === null) {
		return { kind: 'unknown', label: 'Unknown resource' };
	}
	const r = resource as Record<string, unknown>;
	if (typeof r.Path === 'string') return { kind: 'path', label: r.Path };
	if (typeof r.Executable === 'string') return { kind: 'executable', label: r.Executable };
	if (typeof r.Application === 'string') return { kind: 'application', label: r.Application };
	if (typeof r.Window === 'string') return { kind: 'window', label: r.Window };
	if (typeof r.HostPort === 'object' && r.HostPort !== null) {
		const hp = r.HostPort as Record<string, unknown>;
		return { kind: 'host', label: `${String(hp.host)}:${String(hp.port)}` };
	}
	return { kind: 'unknown', label: 'Unknown resource' };
}

/** Dialog headline for a request, e.g. "The assistant wants to modify …". */
export function headlineFor(request: PermissionRequest): string {
	switch (riskLevel(request.capability)) {
		case 'mutate':
			return 'The assistant wants to modify:';
		case 'control':
			return 'The assistant wants to control:';
		default:
			return 'The assistant wants to access:';
	}
}

/** IPC params for answering a request. Deny carries only the id. */
export function replyParams(
	id: string,
	choice: LifetimeChoice
): { method: 'permission.approve' | 'permission.deny'; params: Record<string, string> } {
	if (choice === 'deny') {
		return { method: 'permission.deny', params: { id } };
	}
	return { method: 'permission.approve', params: { id, lifetime: choice } };
}
