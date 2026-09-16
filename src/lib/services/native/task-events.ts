// Pure background-task event parsing for the WebView.
// Framework-free so it runs under `node --test`. The reactive listener
// lives in `task-events.svelte.ts`; the Rust side emits from
// `task-host` (`TASK_STEP_COMPLETED_EVENT` / `TASK_TERMINAL_EVENT`).
//
// Events over the bridge:
//   `task.step_completed` { task_id, step_id, title, text }
//   `task.terminal` { task_id, title, status, text }
import { HOST_EVENTS } from './host-events.ts';

export interface TaskStepEvent {
	kind: 'step';
	taskId: string;
	stepId: string;
	title: string;
	text: string;
}

export interface TaskTerminalEvent {
	kind: 'terminal';
	taskId: string;
	title: string;
	status: string;
	text: string;
}

export type TaskProgressEvent = TaskStepEvent | TaskTerminalEvent;

/** Parse one host task event. Null for anything else — caller ignores it. */
export function parseTaskProgressEvent(
	event: string,
	data: unknown
): TaskProgressEvent | null {
	const d = data as Record<string, unknown> | null;
	if (event === HOST_EVENTS.TASK_STEP_COMPLETED) {
		if (
			typeof d?.task_id !== 'string' ||
			typeof d?.step_id !== 'string' ||
			typeof d?.title !== 'string' ||
			typeof d?.text !== 'string'
		) {
			return null;
		}
		return { kind: 'step', taskId: d.task_id, stepId: d.step_id, title: d.title, text: d.text };
	}
	if (event === HOST_EVENTS.TASK_TERMINAL) {
		if (
			typeof d?.task_id !== 'string' ||
			typeof d?.title !== 'string' ||
			typeof d?.status !== 'string' ||
			typeof d?.text !== 'string'
		) {
			return null;
		}
		return { kind: 'terminal', taskId: d.task_id, title: d.title, status: d.status, text: d.text };
	}
	return null;
}

/** One-line toast message for a task event. Empty text stays empty so the
 * caller can skip Instruments-without-output instead of toasting blank. */
export function toastMessageFor(event: TaskProgressEvent): string {
	const text = event.text.trim();
	if (!text) return '';
	if (event.kind === 'step') return text;
	return event.status === 'completed'
		? `Done: ${event.title}\n${text}`
		: `${event.title} ${event.status}\n${text}`;
}

/** Cap toast bodies so a long model turn can't flood the hint surface. */
export function truncateToast(message: string, maxChars = 280): string {
	if (message.length <= maxChars) return message;
	return `${message.slice(0, maxChars - 1).trimEnd()}…`;
}
