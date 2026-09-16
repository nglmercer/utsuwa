import { browser } from '$app/environment';
import { chatHintStore } from '$lib/stores/chat-hint.svelte';
import { HOST_EVENT, isHostEvent } from './bridge';
import { parseTaskProgressEvent, toastMessageFor, truncateToast } from './task-events';

// Background-task results surfacing: the host emits `task.step_completed`
// per finished agent step plus one `task.terminal`, and this listener turns
// each into a transient chat toast. Without it, interval readings and other
// background output sit in SQLite, visible only to drivers that poll.
let attached = false;

function onHostEvent(e: Event) {
	if (!isHostEvent(e)) return;
	const detail = (e as CustomEvent).detail;
	if (!detail || typeof detail.event !== 'string') return;
	const parsed = parseTaskProgressEvent(detail.event, detail.data);
	if (!parsed) return;
	const message = toastMessageFor(parsed);
	if (!message) return;
	chatHintStore.showHint(truncateToast(message));
}

export function attachTaskEventsListener() {
	if (!browser || attached) return;
	attached = true;
	window.addEventListener(HOST_EVENT, onHostEvent);
}
