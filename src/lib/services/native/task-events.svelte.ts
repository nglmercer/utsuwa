import { browser } from '$app/environment';
import { chatHintStore } from '$lib/stores/chat-hint.svelte';
import { subscribeHostEvents } from './host-events';
import { parseTaskProgressEvent, toastMessageFor, truncateToast } from './task-events';

// Background-task results surfacing: the host emits `task.step_completed`
// per finished agent step plus one `task.terminal`, and this listener turns
// each into a transient chat toast. Without it, interval readings and other
// background output sit in SQLite, visible only to drivers that poll.
let attached = false;

export function attachTaskEventsListener() {
	if (!browser || attached) return;
	attached = true;
	subscribeHostEvents(
		(event, data) => parseTaskProgressEvent(event, data),
		(parsed) => {
			const message = toastMessageFor(parsed);
			if (!message) return;
			chatHintStore.showHint(truncateToast(message));
		}
	);
}
