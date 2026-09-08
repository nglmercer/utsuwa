import { browser } from '$app/environment';
import { getBridge, HOST_EVENT, isHostEvent } from './bridge';
import {
	initialAgentChatState,
	parseAgentTurnEvent,
	reduceAgentEvent,
	reduceAgentSend,
	sendParams,
	type AgentSendOptions,
	type AgentTurnEvent,
	type TurnDone,
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
export async function sendAgentMessage(
	text: string,
	options: AgentSendOptions = {},
	onDelta?: (fullContent: string) => void
): Promise<TurnDone> {
	const bridge = getBridge();
	if (!bridge) throw new Error('agent.send_message needs the native host bridge');
	attachAgentListener();
	state = reduceAgentSend(state);

	return await new Promise<TurnDone>((resolve, reject) => {
		let fullContent = '';
		let waitingForApproval = false;
		let settled = false;
		const cleanup = () => window.removeEventListener(HOST_EVENT, onEvent);
		const fail = (error: unknown) => {
			if (settled) return;
			settled = true;
			cleanup();
			const message = error instanceof Error ? error.message : String(error);
			state = { ...state, phase: 'idle', error: message };
			reject(new Error(message));
		};
		const onEvent = (event: Event) => {
			if (!isHostEvent(event)) return;
			const detail = (event as CustomEvent).detail;
			const parsed = parseAgentTurnEvent(detail?.event, detail?.data) as AgentTurnEvent | null;
			if (!parsed) return;
			switch (parsed.kind) {
				case 'delta':
					if (waitingForApproval) {
						fullContent = '';
						waitingForApproval = false;
					}
					fullContent += parsed.delta;
					onDelta?.(fullContent);
					break;
				case 'suspended':
					waitingForApproval = true;
					fullContent = parsed.suspended.text;
					onDelta?.(fullContent);
					break;
				case 'done':
					if (settled) return;
					settled = true;
					cleanup();
					onDelta?.(parsed.done.text);
					resolve(parsed.done);
					break;
				case 'failed':
					fail(parsed.error);
					break;
				case 'cancelled':
					fail('agent turn cancelled');
					break;
				case 'tool_started':
				case 'tool_finished':
					break;
			}
		};
		window.addEventListener(HOST_EVENT, onEvent);
		const params = sendParams(text, options);
		bridge.invoke(params.method, params.params).catch(fail);
	});
}

/** Abandon the in-flight turn. No-op without a bridge. */
export async function cancelAgentTurn(): Promise<void> {
	const bridge = getBridge();
	if (!bridge) return;
	await bridge.invoke('agent.cancel', {});
}
