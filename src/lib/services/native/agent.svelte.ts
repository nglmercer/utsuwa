import { browser } from '$app/environment';
import { getBridge, NATIVE_BRIDGE_UNAVAILABLE_ERROR } from './bridge';
import { subscribeHostEvents } from './host-events';
import { createTurnWatchdog } from './turn-watchdog';
import {
	AGENT_HARD_TIMEOUT_MS,
	AGENT_NO_PROGRESS_TIMEOUT_MS,
	eventMatchesTurn,
	initialAgentChatState,
	isProgressEvent,
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

export function attachAgentListener() {
	if (!browser || attached) return;
	attached = true;
	subscribeHostEvents(
		(event, data) => parseAgentTurnEvent(event, data),
		(turn) => {
			state = reduceAgentEvent(state, turn);
		}
	);
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
	if (!bridge) throw new Error(NATIVE_BRIDGE_UNAVAILABLE_ERROR);
	attachAgentListener();
	state = reduceAgentSend(state);

	return await new Promise<TurnDone>((resolve, reject) => {
		let fullContent = '';
		let waitingForApproval = false;
		let settled = false;
		// Correlate every event below with our turn. Resolved from the send
		// response just after; until then a null id accepts everything (the
		// old behavior) so an early delta can never hang the turn.
		let turnId: string | null = null;
		const watchdog = createTurnWatchdog({
			hardTimeoutMs: AGENT_HARD_TIMEOUT_MS,
			progressTimeoutMs: AGENT_NO_PROGRESS_TIMEOUT_MS,
			// Watchdog expiry: abandon the stuck turn host-side too, so a late
			// terminal event can never resurrect it. Terminal host events
			// (failed/cancelled) take the plain path — the turn is already dead.
			onTimeout: (message) => {
				if (settled) return;
				cancelAgentTurn().catch(() => {});
				fail(message);
			}
		});
		const unsubscribe = subscribeHostEvents(
			(event, data) => parseAgentTurnEvent(event, data),
			(parsed: AgentTurnEvent) => {
				if (!eventMatchesTurn(parsed, turnId)) return;
				if (isProgressEvent(parsed)) watchdog.progress();
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
						// Paused while the user decides: neither watchdog may
						// fire against think time. Post-resume progress re-arms.
						watchdog.pause();
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
			}
		);
		const cleanup = () => {
			unsubscribe();
			watchdog.stop();
		};
		const fail = (error: unknown) => {
			if (settled) return;
			settled = true;
			cleanup();
			const message = error instanceof Error ? error.message : String(error);
			state = { ...state, phase: 'idle', error: message };
			reject(new Error(message));
		};
		watchdog.start();
		const params = sendParams(text, options);
		bridge
			.invoke(params.method, params.params)
			.then((response) => {
				const id = (response as { turn_id?: unknown } | null)?.turn_id;
				if (typeof id === 'string' && id) turnId = id;
			})
			.catch(fail);
	});
}

/** Abandon the in-flight turn. No-op without a bridge. */
export async function cancelAgentTurn(): Promise<void> {
	const bridge = getBridge();
	if (!bridge) return;
	await bridge.invoke('agent.cancel', {});
}
