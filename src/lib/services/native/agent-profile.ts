/** Native agent tool-profile setting (`agent.tool_profile`).
 *
 * The profile bounds how many tool definitions ride on every model request:
 * `full` sends everything (slowest, most capable), smaller profiles trade
 * capability for speed. The host reads the setting per turn, so changes apply
 * without a restart; an unset key falls back to the host default (full for
 * cloud providers, simple for local ones).
 *
 * Pure functions over an injected `invoke`, following `permissions.ts`, so
 * unit tests never touch the real bridge.
 */

export const AGENT_TOOL_PROFILE_SETTING = 'agent.tool_profile';

export const AGENT_TOOL_PROFILES = [
	'minimal',
	'simple',
	'standard',
	'developer',
	'computeruse',
	'full'
] as const;

export type AgentToolProfile = (typeof AGENT_TOOL_PROFILES)[number];

/** One-line capability summaries for the Settings dropdown. */
export const AGENT_TOOL_PROFILE_DESCRIPTIONS: Record<AgentToolProfile, string> = {
	minimal: 'Fastest — clock, environment, and limited reads only.',
	simple: 'Small-model friendly — one edit interface, no low-level variants.',
	standard: 'Everyday work — files, HTTP, archives, system facts, documents.',
	developer: 'Standard plus process execution and Git.',
	computeruse: 'Desktop control — screen, input, browser, camera, media.',
	full: 'Everything — slowest prompts, maximum capability.'
};

export type NativeInvoke = (
	method: string,
	params?: Record<string, unknown>
) => Promise<unknown>;

export function isAgentToolProfile(value: unknown): value is AgentToolProfile {
	return (
		typeof value === 'string' &&
		(AGENT_TOOL_PROFILES as readonly string[]).includes(value.trim().toLowerCase())
	);
}

function normalizeProfile(value: unknown): AgentToolProfile | null {
	if (typeof value !== 'string') return null;
	const normalized = value.trim().toLowerCase().replace(/[-_]/g, '');
	if (normalized === 'computeruse') return 'computeruse';
	return isAgentToolProfile(normalized) ? normalized : null;
}

/** Read the persisted profile; `null` means unset (host default applies). */
export async function getAgentToolProfile(invoke: NativeInvoke): Promise<AgentToolProfile | null> {
	const result = (await invoke('settings.get', {
		key: AGENT_TOOL_PROFILE_SETTING
	})) as Record<string, unknown> | null;
	if (!result || !Object.prototype.hasOwnProperty.call(result, 'value')) {
		throw new Error('settings.get returned an unexpected payload');
	}
	return normalizeProfile(result.value);
}

/** Persist the profile; the host picks it up on the next turn. */
export async function setAgentToolProfile(
	invoke: NativeInvoke,
	profile: AgentToolProfile
): Promise<void> {
	const result = (await invoke('settings.set', {
		key: AGENT_TOOL_PROFILE_SETTING,
		value: profile
	})) as Record<string, unknown> | null;
	if (!result || result.ok !== true) {
		throw new Error('settings.set returned an unexpected payload');
	}
}
