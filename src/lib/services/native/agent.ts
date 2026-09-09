// Pure native-agent chat logic for the host-driven turn loop.
// Framework-free so it runs under `node --test`. The reactive store lives
// in `agent.svelte.ts`; the Rust side is `crates/app-host/src/agent_runtime.rs`.
//
// Turn lifecycle over the bridge:
//   invoke('agent.send_message', { text }) -> { accepted: true }
//   ... `agent.turn_done` { text, executed, tool_steps, truncated }
//   ... `agent.turn_suspended` { text, request_id } (a `permission.requested`
//       event carries the matching approval request for the dialog)
//   ... `agent.turn_failed` { error }
//   ... `agent.turn_cancelled` {}
// invoke('agent.cancel', {}) abandons the in-flight turn.

export interface ExecutedStep {
	id: string;
	name: string;
	output: unknown;
}

export type NativeToolStepStatus = 'success' | 'failed' | 'denied';

/** Authoritative native result for every completed tool attempt. */
export interface NativeToolStep {
	id: string;
	name: string;
	status: NativeToolStepStatus;
	ok: boolean;
	output?: unknown;
	error?: string;
}

export interface TurnDone {
	text: string;
	executed: ExecutedStep[];
	toolSteps: NativeToolStep[];
	truncated: boolean;
}

export interface TurnSuspended {
	text: string;
	request_id: string;
	toolSteps: NativeToolStep[];
}

export interface AgentHistoryMessage {
	role: 'system' | 'user' | 'assistant';
	content: unknown;
}

export interface AgentSendOptions {
	history?: AgentHistoryMessage[];
	systemPrompt?: string;
	appendUserMessage?: boolean;
}

export type AgentTurnEvent =
	| { kind: 'delta'; delta: string }
	| { kind: 'tool_started'; id: string; name: string }
	| { kind: 'tool_finished'; id: string; name: string; ok: boolean }
	| { kind: 'done'; done: TurnDone }
	| { kind: 'suspended'; suspended: TurnSuspended }
	| { kind: 'failed'; error: string }
	| { kind: 'cancelled' };

/** Parse one host turn event into typed state. Returns null for anything
 * else (permission requests, app events, garbage) — the caller ignores it. */
export function parseAgentTurnEvent(event: string, data: unknown): AgentTurnEvent | null {
	const d = data as Record<string, unknown> | null;
	switch (event) {
		case 'agent.text_delta':
			return typeof d?.delta === 'string' ? { kind: 'delta', delta: d.delta } : null;
		case 'agent.tool_started':
			return typeof d?.id === 'string' && typeof d?.name === 'string'
				? { kind: 'tool_started', id: d.id, name: d.name }
				: null;
		case 'agent.tool_finished':
			return typeof d?.id === 'string' && typeof d?.name === 'string' && typeof d?.ok === 'boolean'
				? { kind: 'tool_finished', id: d.id, name: d.name, ok: d.ok }
				: null;
		case 'agent.turn_done': {
			if (typeof d?.text !== 'string') return null;
			return {
				kind: 'done',
				done: {
					text: d.text,
					executed: parseExecutedSteps(d.executed),
					toolSteps: parseToolSteps(d.tool_steps, d.executed),
					truncated: d.truncated === true
				}
			};
		}
		case 'agent.turn_suspended': {
			if (typeof d?.text !== 'string' || typeof d?.request_id !== 'string') return null;
			return {
				kind: 'suspended',
				suspended: {
					text: d.text,
					request_id: d.request_id,
					toolSteps: parseToolSteps(d.tool_steps, [])
				}
			};
		}
		case 'agent.turn_failed':
			return { kind: 'failed', error: typeof d?.error === 'string' ? d.error : 'unknown error' };
		case 'agent.turn_cancelled':
			return { kind: 'cancelled' };
		default:
			return null;
	}
}

function parseExecutedSteps(value: unknown): ExecutedStep[] {
	if (!Array.isArray(value)) return [];
	const out: ExecutedStep[] = [];
	for (const item of value) {
		if (typeof item !== 'object' || item === null) continue;
		const step = item as Record<string, unknown>;
		if (typeof step.id !== 'string' || typeof step.name !== 'string') continue;
		out.push({ id: step.id, name: step.name, output: step.output });
	}
	return out;
}

function parseToolSteps(value: unknown, executedValue: unknown): NativeToolStep[] {
	if (!Array.isArray(value)) {
		return parseExecutedSteps(executedValue).map((step) => ({
			id: step.id,
			name: step.name,
			status: 'success',
			ok: true,
			output: step.output
		}));
	}
	const out: NativeToolStep[] = [];
	for (const item of value) {
		if (typeof item !== 'object' || item === null) continue;
		const step = item as Record<string, unknown>;
		if (typeof step.id !== 'string' || typeof step.name !== 'string' || typeof step.ok !== 'boolean') continue;
		const status = step.status;
		const parsedStatus: NativeToolStepStatus =
			status === 'denied' ? 'denied' : status === 'failed' || step.ok === false ? 'failed' : 'success';
		out.push({
			id: step.id,
			name: step.name,
			status: parsedStatus,
			ok: step.ok,
			...(step.output !== null && step.output !== undefined ? { output: step.output } : {}),
			...(typeof step.error === 'string' ? { error: step.error } : {})
		});
	}
	return out;
}

/** IPC params for starting a turn. The host rejects empty/oversize text. */
export function sendParams(
	text: string,
	options: AgentSendOptions = {}
): { method: 'agent.send_message'; params: Record<string, unknown> } {
	const params: Record<string, unknown> = { text };
	if (options.history?.length) params.history = options.history;
	if (options.systemPrompt !== undefined) params.system_prompt = options.systemPrompt;
	if (options.appendUserMessage !== undefined) params.append_user_message = options.appendUserMessage;
	return { method: 'agent.send_message', params };
}

/** One-line status for the turn indicator. */
export function statusFor(state: AgentChatState): string {
	switch (state.phase) {
		case 'idle':
			return 'Idle';
		case 'running':
			return 'Thinking…';
		case 'suspended':
			return 'Waiting for your approval';
	}
}

export type AgentPhase = 'idle' | 'running' | 'suspended';

export interface AgentChatState {
	phase: AgentPhase;
	/** Latest assistant text (partial while suspended, final when done). */
	latest: string;
	/** Tool steps executed by the latest finished turn. */
	executed: ExecutedStep[];
	/** All completed native tool attempts, including failed/denied calls. */
	toolSteps: NativeToolStep[];
	/** Pending approval request id while suspended. */
	requestId: string | null;
	/** Failure message, if the last turn failed. */
	error: string | null;
}

export function initialAgentChatState(): AgentChatState {
	return { phase: 'idle', latest: '', executed: [], toolSteps: [], requestId: null, error: null };
}

/** Pure state transition for one parsed turn event. */
export function reduceAgentEvent(state: AgentChatState, event: AgentTurnEvent): AgentChatState {
	switch (event.kind) {
		case 'delta':
			return { ...state, phase: 'running', latest: state.latest + event.delta, error: null };
		case 'tool_started':
		case 'tool_finished':
			return { ...state, phase: 'running' };
		case 'done':
			return {
				phase: 'idle',
				latest: event.done.text,
				executed: event.done.executed,
				toolSteps: event.done.toolSteps,
				requestId: null,
				error: null
			};
		case 'suspended':
			return {
				...state,
				phase: 'suspended',
				latest: event.suspended.text,
				toolSteps: event.suspended.toolSteps,
				requestId: event.suspended.request_id
			};
		case 'failed':
			return { ...state, phase: 'idle', requestId: null, error: event.error };
		case 'cancelled':
			return { ...state, phase: 'idle', requestId: null };
	}
}

/** State transition for a locally-sent message (turn starts running). */
export function reduceAgentSend(state: AgentChatState): AgentChatState {
	return { ...state, phase: 'running', latest: '', executed: [], toolSteps: [], requestId: null, error: null };
}
