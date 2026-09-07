import { browser } from '$app/environment';
import { getBridge, HOST_EVENT, isHostEvent } from './bridge';
import {
	initialAgentChatState,
	parseAgentTurnEvent,
	reduceAgentEvent,
	reduceAgentSend,
	type AgentChatState
} from './agent';

// Reactive mirror of the host agent turn loop (`agent.send_message` and
// friends). Fed by `agent.turn_*` host events; the permission dialog owns
// approvals, this store only tracks the turn phase so the chat UI can show
// thinking / waiting-for-approval states.
let state = $state<AgentChatState>(initialAgentChatState());
let attached = false;

function onHostEvent(e: Event) {
	if (!isHostEvent(e)) return;
	const detail = (e as CustomEvent).detail;
	if (!detail || typeof detail.event !== 'string') return;
	const turn = parseAgentTurnEvent(detail.event, detail.data);
	if (turn) state = reduceAgentEvent(state, turn);
}

export function attachAgentListener() {
	if (!browser || attached) return;
	attached = true;
	window.addEventListener(HOST_EVENT, onHostEvent);
}

export function agentChatState(): AgentChatState {
	return state;
}

/** Start a host turn. Throws when no native bridge is present (plain browser). */
export async function sendAgentMessage(text: string): Promise<unknown> {
	const bridge = getBridge();
	if (!bridge) throw new Error('agent.send_message needs the native host bridge');
	state = reduceAgentSend(state);
	try {
		return await bridge.invoke('agent.send_message', { text });
	} catch (err) {
		state = { ...state, phase: 'idle', error: err instanceof Error ? err.message : String(err) };
		throw err;
	}
}

/** Abandon the in-flight turn. No-op without a bridge. */
export async function cancelAgentTurn(): Promise<void> {
	const bridge = getBridge();
	if (!bridge) return;
	await bridge.invoke('agent.cancel', {});
}
