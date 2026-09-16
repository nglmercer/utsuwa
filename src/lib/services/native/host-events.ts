// Native host push events: one constant table plus one subscription
// helper shared by every WebView listener. Pure and node-safe (no `$app`
// imports): the browser guard is a `typeof window` check and tests inject
// an explicit EventTarget.
//
// Application-specific parsing stays in the caller's pure `parse`
// function; this module only owns the channel mechanics (guard, validate,
// dispatch, cleanup).
import { HOST_EVENT, isHostEvent } from './bridge.ts';

// Every `utsuwa-host-event` name the frontend consumes. The Rust side owns
// the same strings (see `protocol/ipc-contract.json`); adding an event
// means adding it here, not scattering a new literal.
export const HOST_EVENTS = {
	AGENT_TEXT_DELTA: 'agent.text_delta',
	AGENT_TOOL_STARTED: 'agent.tool_started',
	AGENT_TOOL_FINISHED: 'agent.tool_finished',
	AGENT_DONE: 'agent.turn_done',
	AGENT_FAILED: 'agent.turn_failed',
	AGENT_CANCELLED: 'agent.turn_cancelled',
	AGENT_SUSPENDED: 'agent.turn_suspended',

	TASK_STEP_COMPLETED: 'task.step_completed',
	TASK_TERMINAL: 'task.terminal',

	AVATAR_ROUTINE_REQUESTED: 'avatar.routine.requested',
	AVATAR_ROUTINE_COMPLETED: 'avatar.routine.completed',

	PERMISSION_REQUESTED: 'permission.requested',
	PERMISSION_DISMISSED: 'permission.dismissed',

	CAMERA_ACTIVITY_CHANGED: 'camera.activity.changed',
	MICROPHONE_ACTIVITY_CHANGED: 'microphone.activity.changed',
	SCREEN_SHARE_CHANGED: 'desktop.share_screen.changed',
	AUDIO_CAPTURE: 'audio.capture',

	APP_READY: 'app.ready'
} as const;

export type HostEventName = (typeof HOST_EVENTS)[keyof typeof HOST_EVENTS];

export type HostEventTarget = Pick<EventTarget, 'addEventListener' | 'removeEventListener'>;

function defaultTarget(): HostEventTarget | null {
	if (typeof window === 'undefined') return null;
	return window;
}

/**
 * Subscribe to validated host events. `parse` maps one (name, payload)
 * pair to the caller's typed event (or null to ignore); `handler` runs
 * only for parsed events. Returns an unsubscribe function; without a
 * target (plain SSR, no window) it subscribes to nothing and the
 * unsubscribe is a no-op.
 */
export function subscribeHostEvents<T>(
	parse: (event: string, data: Record<string, unknown>) => T | null,
	handler: (event: T) => void,
	target?: HostEventTarget | null
): () => void {
	const events = target === undefined ? defaultTarget() : target;
	if (!events) return () => {};
	const onEvent = (event: Event) => {
		if (!isHostEvent(event)) return;
		const parsed = parse(event.detail.event, event.detail.data);
		if (parsed !== null && parsed !== undefined) handler(parsed);
	};
	events.addEventListener(HOST_EVENT, onEvent);
	return () => {
		events.removeEventListener(HOST_EVENT, onEvent);
	};
}
