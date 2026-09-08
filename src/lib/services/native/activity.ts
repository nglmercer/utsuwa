// Pure activity-feed logic for the plan Phase 35 Activity panel.
// Framework-free so it runs under `node --test`. The reactive store lives
// in `activity.svelte.ts`. Records come from the host `activity.list` IPC
// method (Rust `audit_core::AuditRecord`: time, tool, status, resource,
// principal, duration); details are redacted at record time.

export interface ActivityRecord {
	timestamp_ms: number;
	principal: string;
	capability: string | null;
	resource: string | null;
	outcome: string;
	detail: string;
	duration_ms: number | null;
}

/** Parse one Rust audit record into presentation shape. Returns null for
 * anything unrecognizable — the panel never renders garbage. */
export function parseActivityRecord(data: unknown): ActivityRecord | null {
	if (typeof data !== 'object' || data === null) return null;
	const d = data as Record<string, unknown>;
	if (typeof d.timestamp_ms !== 'number' || typeof d.outcome !== 'string') return null;
	return {
		timestamp_ms: d.timestamp_ms,
		principal: summarizePrincipal(d.principal),
		capability: typeof d.capability === 'string' ? d.capability : null,
		resource: summarizeResource(d.resource),
		outcome: d.outcome,
		detail: typeof d.detail === 'string' ? d.detail : '',
		duration_ms: typeof d.duration_ms === 'number' ? d.duration_ms : null
	};
}

function summarizePrincipal(principal: unknown): string {
	if (typeof principal === 'string') return principal;
	if (typeof principal !== 'object' || principal === null) return 'Unknown';
	const keys = Object.keys(principal);
	if (keys.length === 0) return 'Unknown';
	const kind = keys[0];
	const value = (principal as Record<string, unknown>)[kind];
	if (kind === 'Agent' && typeof value === 'string') return `Agent ${value.slice(0, 8)}`;
	if (kind === 'Agent') return 'Agent';
	if (kind === 'BuiltinTool') return `Tool ${String(value)}`;
	if (kind === 'WasmPlugin') return `Plugin ${String(value)}`;
	if (kind === 'McpServer') return `MCP ${String(value)}`;
	if (kind === 'NativePlugin') return `Native plugin ${String(value)}`;
	return kind;
}

function summarizeResource(resource: unknown): string | null {
	if (resource === null || resource === undefined) return null;
	if (typeof resource === 'string') return resource;
	if (typeof resource !== 'object') return null;
	const r = resource as Record<string, unknown>;
	if (typeof r.Path === 'string') return r.Path;
	if (typeof r.Executable === 'string') return r.Executable;
	if (typeof r.Application === 'string') return r.Application;
	if (typeof r.Window === 'string') return r.Window;
	if (typeof r.HostPort === 'object' && r.HostPort !== null) {
		const hp = r.HostPort as Record<string, unknown>;
		return `${String(hp.host)}:${String(hp.port)}`;
	}
	if (typeof r.McpTool === 'object' && r.McpTool !== null) {
		const t = r.McpTool as Record<string, unknown>;
		return `MCP ${String(t.server)} / ${String(t.tool)}`;
	}
	return null;
}

/** One-line summary, e.g. "14:02 · Agent · Filesystem read · Approved". */
export function headlineFor(record: ActivityRecord): string {
	const time = new Date(record.timestamp_ms);
	const hh = String(time.getHours()).padStart(2, '0');
	const mm = String(time.getMinutes()).padStart(2, '0');
	const parts = [`${hh}:${mm}`, record.principal];
	if (record.capability) parts.push(formatCapability(record.capability));
	parts.push(formatOutcome(record.outcome));
	return parts.join(' · ');
}

function formatCapability(capability: string): string {
	const spaced = capability.replace(/([a-z])([A-Z])/g, '$1 $2').replace(/_/g, ' ').toLowerCase();
	return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

function formatOutcome(outcome: string): string {
	const spaced = outcome.replace(/([a-z])([A-Z])/g, '$1 $2');
	return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/** Human duration, e.g. "42 ms", "3.1 s". Null when unmeasured. */
export function formatDuration(duration_ms: number | null): string | null {
	if (duration_ms === null) return null;
	if (duration_ms < 1000) return `${duration_ms} ms`;
	return `${(duration_ms / 1000).toFixed(1)} s`;
}

/** Parse an `activity.list` result payload into records (newest first as
 * sent by the host). Non-array payloads yield []. */
export function parseActivityList(result: unknown): ActivityRecord[] {
	if (!Array.isArray(result)) return [];
	const out: ActivityRecord[] = [];
	for (const item of result) {
		const record = parseActivityRecord(item);
		if (record) out.push(record);
	}
	return out;
}
