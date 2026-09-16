// Pure native-agent chat logic for the host-driven turn loop.
// Framework-free so it runs under `node --test`. The reactive store lives
// in `agent.svelte.ts`; the Rust side is `crates/app-host/src/runtime.rs`
// (+ `runtime/` for session/turn/authorization/prompts/providers).
//
// Turn lifecycle over the bridge:
//   invoke('agent.send_message', { text }) -> { accepted: true, turn_id }
//   ... `agent.turn_done` { turn_id, text, executed, tool_steps, truncated }
//   ... `agent.turn_suspended` { turn_id, text, request_id } (a
//       `permission.requested` event carries the matching approval request
//       for the dialog)
//   ... `agent.turn_failed` { turn_id, error }
//   ... `agent.turn_cancelled` { turn_id }
// invoke('agent.cancel', {}) abandons the in-flight turn.
// Every event carries the originating turn id; promises ignore events for
// other turns (a cancelled turn id never resolves a bystander).
import { HOST_EVENTS } from './host-events.ts';

export interface ExecutedStep {
	id: string;
	name: string;
	output: unknown;
}

export type NativeToolStepStatus = 'success' | 'retry' | 'failed' | 'denied';

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
	| { kind: 'delta'; delta: string; turnId: string | null }
	| { kind: 'tool_started'; id: string; name: string; turnId: string | null }
	| { kind: 'tool_finished'; id: string; name: string; ok: boolean; turnId: string | null }
	| { kind: 'done'; done: TurnDone; turnId: string | null }
	| { kind: 'suspended'; suspended: TurnSuspended; turnId: string | null }
	| { kind: 'failed'; error: string; turnId: string | null }
	| { kind: 'cancelled'; turnId: string | null };

// Watchdog budgets for an interactive turn. The hard budget bounds the whole
// turn; the no-progress budget bounds silence between progress signals.
// Suspension (awaiting the user's approval) pauses both: a turn waiting on
// the user is not stuck, and killing it would strand the approval dialog.
export const AGENT_HARD_TIMEOUT_MS = 180_000;
export const AGENT_NO_PROGRESS_TIMEOUT_MS = 45_000;

/** Parse one host turn event into typed state. Returns null for anything
 * else (permission requests, app events, garbage) — the caller ignores it. */
export function parseAgentTurnEvent(event: string, data: unknown): AgentTurnEvent | null {
	const d = data as Record<string, unknown> | null;
	const turnId = typeof d?.turn_id === 'string' ? d.turn_id : null;
	switch (event) {
		case HOST_EVENTS.AGENT_TEXT_DELTA:
			return typeof d?.delta === 'string' ? { kind: 'delta', delta: d.delta, turnId } : null;
		case HOST_EVENTS.AGENT_TOOL_STARTED:
			return typeof d?.id === 'string' && typeof d?.name === 'string'
				? { kind: 'tool_started', id: d.id, name: d.name, turnId }
				: null;
		case HOST_EVENTS.AGENT_TOOL_FINISHED:
			return typeof d?.id === 'string' && typeof d?.name === 'string' && typeof d?.ok === 'boolean'
				? { kind: 'tool_finished', id: d.id, name: d.name, ok: d.ok, turnId }
				: null;
		case HOST_EVENTS.AGENT_DONE: {
			if (typeof d?.text !== 'string') return null;
			return {
				kind: 'done',
				done: {
					text: d.text,
					executed: parseExecutedSteps(d.executed),
					toolSteps: parseToolSteps(d.tool_steps, d.executed),
					truncated: d.truncated === true
				},
				turnId
			};
		}
		case HOST_EVENTS.AGENT_SUSPENDED: {
			if (typeof d?.text !== 'string' || typeof d?.request_id !== 'string') return null;
			return {
				kind: 'suspended',
				suspended: {
					text: d.text,
					request_id: d.request_id,
					toolSteps: parseToolSteps(d.tool_steps, [])
				},
				turnId
			};
		}
		case HOST_EVENTS.AGENT_FAILED:
			return {
				kind: 'failed',
				error: typeof d?.error === 'string' ? d.error : 'unknown error',
				turnId
			};
		case HOST_EVENTS.AGENT_CANCELLED:
			return { kind: 'cancelled', turnId };
		default:
			return null;
	}
}

/** True when this event belongs to `turnId`. Events without an id (older
 * hosts) and unknown ids (unparseable send response) match anything, so a
 * missing id degrades to the old unfiltered behavior instead of a hang. */
export function eventMatchesTurn(event: AgentTurnEvent, turnId: string | null): boolean {
	if (turnId === null || event.turnId === null) return true;
	return event.turnId === turnId;
}

/** True for events that prove the turn is alive: text, tool start/finish.
 * Suspension is handled separately (it pauses the watchdogs, not resets). */
export function isProgressEvent(event: AgentTurnEvent): boolean {
	return event.kind === 'delta' || event.kind === 'tool_started' || event.kind === 'tool_finished';
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
			status === 'denied'
				? 'denied'
				: status === 'retry'
					? 'retry'
					: status === 'failed' || step.ok === false
						? 'failed'
						: 'success';
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
