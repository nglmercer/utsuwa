// Pure permission-request logic for the native-host approval dialog.
// Framework-free so it runs under `node --test`. The reactive store lives
// in `permissions.svelte.ts`; rendering in `PermissionDialog.svelte`.

export type GrantLifetime = 'once' | 'task' | 'session' | 'persistent';

export type LifetimeChoice = GrantLifetime | 'deny';

export interface ResourceSummary {
	kind: 'path' | 'host' | 'executable' | 'process' | 'application' | 'window' | 'unknown';
	label: string;
}

export interface PermissionRequest {
	id: string;
	principal: string;
	capability: string;
	resource: ResourceSummary;
	reason: string;
	/** Sensitive paths/interpreters can only be approved for one use. */
	requiresOnce: boolean;
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
		reason: typeof d.reason === 'string' ? d.reason : '',
		requiresOnce: d.requires_once === true
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
	if (typeof r.Process === 'object' && r.Process !== null) {
		const process = r.Process as Record<string, unknown>;
		const executable = typeof process.executable === 'string' ? process.executable : 'unknown executable';
		const args = Array.isArray(process.args)
			? process.args.filter((arg): arg is string => typeof arg === 'string')
			: [];
		const cwd = typeof process.cwd === 'string' ? process.cwd : 'unknown cwd';
		const env = process.env && typeof process.env === 'object' ? process.env : [];
		const envCount = Array.isArray(env) ? env.length : Object.keys(env).length;
		return {
			kind: 'process',
			label: `${executable}${args.length ? ` ${args.join(' ')}` : ''} (cwd ${cwd}${envCount ? `, ${envCount} env change${envCount === 1 ? '' : 's'}` : ''})`
		};
	}
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

/** One standing grant, as serialized by the host's `permission.grants`. */
export interface StandingGrant {
	capability: string;
	paths: string[];
	lifetime: string;
}

/** Standing grants plus the host-resolved home directory. */
export interface GrantsSnapshot {
	grants: StandingGrant[];
	home: string | null;
}

function parseGrant(item: unknown): StandingGrant | null {
	if (typeof item !== 'object' || item === null) return null;
	const g = item as Record<string, unknown>;
	if (typeof g.capability !== 'string') return null;
	const paths: string[] = [];
	const scope = g.scope as Record<string, unknown> | undefined;
	const resources = scope !== undefined && Array.isArray(scope.resources) ? scope.resources : [];
	for (const r of resources) {
		if (typeof r === 'object' && r !== null && typeof (r as Record<string, unknown>).Path === 'string') {
			paths.push((r as Record<string, unknown>).Path as string);
		}
	}
	return {
		capability: g.capability,
		paths,
		lifetime: typeof g.lifetime === 'string' ? g.lifetime : ''
	};
}

/** Fetch standing grants for the access settings UI. Throws the bridge
 * rejection when the host is unreachable so the panel can show it. */
export async function listGrants(
	invoke: (method: string, params?: Record<string, unknown>) => Promise<unknown>
): Promise<GrantsSnapshot> {
	const result = (await invoke('permission.grants', {})) as Record<string, unknown>;
	const raw = Array.isArray(result.grants) ? result.grants : [];
	return {
		grants: raw.flatMap((item) => {
			const parsed = parseGrant(item);
			return parsed ? [parsed] : [];
		}),
		home: typeof result.home === 'string' ? result.home : null
	};
}

/** True when a FilesystemRead grant covers the home directory — the
 * "model can read any file" toggle state. */
export function homeReadGranted(snapshot: GrantsSnapshot): boolean {
	if (snapshot.home === null) return false;
	return snapshot.grants.some(
		(g) => g.capability === 'FilesystemRead' && g.paths.some((p) => p === snapshot.home)
	);
}

/** Grant the model persistent read access to the home folder (explicit
 * user action). Returns the granted path. Throws on bridge errors. */
export async function grantHomeReadAccess(
	invoke: (method: string, params?: Record<string, unknown>) => Promise<unknown>
): Promise<string> {
	const result = (await invoke('permission.grant', {
		capability: 'FilesystemRead',
		lifetime: 'persistent'
	})) as Record<string, unknown>;
	if (result.ok !== true || typeof result.path !== 'string') {
		throw new Error('permission.grant returned an unexpected payload');
	}
	return result.path;
}

/** Revoke the home-folder read grant. Returns the removed row count. */
export async function revokeHomeReadAccess(
	invoke: (method: string, params?: Record<string, unknown>) => Promise<unknown>
): Promise<number> {
	const result = (await invoke('permission.revoke', {
		capability: 'FilesystemRead'
	})) as Record<string, unknown>;
	if (result.ok !== true || typeof result.removed !== 'number') {
		throw new Error('permission.revoke returned an unexpected payload');
	}
	return result.removed;
}
