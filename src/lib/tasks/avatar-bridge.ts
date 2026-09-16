// Browser boundaries for the task orchestrator: the avatar runner bridges
// routine steps to the VRM store (resolving on the routine completion
// ledger), and the notifier records firings for UI/callbacks. Browser-only:
// imports the Svelte store, so no node test — covered by typecheck and the
// dev-tools self-test instead.
import { vrmStore } from '$lib/stores/vrm.svelte';
import { routineTimeoutMs } from './executor.ts';
import { makeEvent, type TaskEventBus } from './events.ts';
import type { AvatarRoutineRunner, NotificationSink } from './executor.ts';
import type { AvatarRoutineStepInput, DurableTask, NotificationStepInput } from './types.ts';

export function createBrowserAvatarRunner(bus: TaskEventBus): AvatarRoutineRunner {
	return {
		run: async (input: AvatarRoutineStepInput) => {
			const routineId = vrmStore.requestAvatarRoutine(input.steps);
			// The executor's own timeout fires first on a stuck routine; this
			// longer bound only guarantees the listener is eventually freed.
			const bound = routineTimeoutMs(input) + 5000;
			return new Promise<{ completed: string[]; detail?: unknown }>((resolve, reject) => {
				const timer = setTimeout(() => {
					off();
					reject(new Error(`routine ${routineId} produced no result within ${bound}ms`));
				}, bound);
				const off = vrmStore.onRoutineResult((result) => {
					if (result.routineId !== routineId) return;
					clearTimeout(timer);
					off();
					bus.emit(
						makeEvent('avatar.completed', Date.now(), {
							sourceId: routineId,
							correlationId: routineId,
							payload: { status: result.status, completed: result.completed }
						})
					);
					if (result.status === 'done') {
						resolve({ completed: result.completed, detail: { endPosition: result.endPosition } });
					} else {
						reject(new Error(`routine ${routineId} was cancelled`));
					}
				});
			});
		}
	};
}

export interface TaskNotification {
	taskId: string;
	title: string;
	body: string;
	firedAt: number;
}

type NotificationListener = (notification: TaskNotification) => void;

export function createBrowserNotifier(): NotificationSink & {
	recent: TaskNotification[];
	onNotification: (listener: NotificationListener) => () => void;
} {
	const recent: TaskNotification[] = [];
	const listeners = new Set<NotificationListener>();
	return {
		recent,
		onNotification(listener: NotificationListener) {
			listeners.add(listener);
			return () => {
				listeners.delete(listener);
			};
		},
		async notify(input: NotificationStepInput, task: DurableTask): Promise<void> {
			const fired: TaskNotification = {
				taskId: task.id,
				title: input.title,
				body: input.body,
				firedAt: Date.now()
			};
			recent.unshift(fired);
			if (recent.length > 20) recent.length = 20;
			for (const listener of [...listeners]) {
				try {
					listener(fired);
				} catch {
					// Listener failures must not fail the task step.
				}
			}
		}
	};
}
