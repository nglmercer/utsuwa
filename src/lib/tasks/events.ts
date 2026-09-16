// In-memory task event bus. Waiting tasks persist their `waitFor` matcher, so
// subscriptions never need persistence themselves: every emitted event is
// matched against waiting rows, including rows restored after a restart.
// Pure and node-safe.
import { newTaskId, type TaskEvent, type TaskEventType } from './types.ts';

export type TaskEventListener = (event: TaskEvent) => void;

export function newEventId(): string {
	return newTaskId();
}

export function makeEvent(
	type: TaskEventType,
	now: number,
	options: Partial<Pick<TaskEvent, 'sourceId' | 'correlationId' | 'payload'>> = {}
): TaskEvent {
	return {
		id: newEventId(),
		type,
		sourceId: options.sourceId,
		correlationId: options.correlationId,
		payload: options.payload,
		createdAt: now
	};
}

export class TaskEventBus {
	private listeners = new Map<TaskEventType | '*', Set<TaskEventListener>>();

	on(type: TaskEventType | '*', listener: TaskEventListener): () => void {
		let set = this.listeners.get(type);
		if (!set) {
			set = new Set();
			this.listeners.set(type, set);
		}
		set.add(listener);
		return () => {
			set.delete(listener);
		};
	}

	emit(event: TaskEvent): void {
		for (const key of [event.type, '*'] as const) {
			const set = this.listeners.get(key);
			if (!set) continue;
			for (const listener of [...set]) {
				try {
					listener(event);
				} catch {
					// A failing listener must not break the emitter or siblings.
				}
			}
		}
	}
}

// A persisted wait matcher against an emitted event. Correlation narrows to
// one waiter (same routine id); without it any event of the type matches.
export function eventMatchesWait(
	event: TaskEvent,
	waitFor: { eventType?: string; correlationId?: string }
): boolean {
	if (!waitFor.eventType || event.type !== waitFor.eventType) return false;
	if (waitFor.correlationId !== undefined && event.correlationId !== waitFor.correlationId) {
		return false;
	}
	return true;
}
