// Browser boundaries for the task orchestrator: the avatar runner bridges
// routine steps to the VRM store (resolving on the routine completion
// ledger), and the notifier records firings for UI/callbacks. Browser-only:
// imports the Svelte store, so no node test — covered by typecheck and the
// dev-tools self-test instead.
import { runAvatarRoutineAndWait } from '$lib/services/avatar/routine';
import { routineTimeoutMs } from './executor.ts';
import { makeEvent, type TaskEventBus } from './events.ts';
import type { AvatarRoutineRunner, NotificationSink } from './executor.ts';
import type { AvatarRoutineStepInput, DurableTask, NotificationStepInput } from './types.ts';

export function createBrowserAvatarRunner(bus: TaskEventBus): AvatarRoutineRunner {
	return {
		run: async (input: AvatarRoutineStepInput) => {
			// The executor's own timeout fires first on a stuck routine; this
			// longer bound only guarantees the wait is eventually freed.
			const result = await runAvatarRoutineAndWait(input.steps, {
				policy: input.policy,
				timeoutMs: routineTimeoutMs(input) + 5000
			});
			bus.emit(
				makeEvent('avatar.completed', Date.now(), {
					sourceId: result.routineId,
					correlationId: result.routineId,
					payload: { status: result.status, completed: result.completed }
				})
			);
			if (result.status === 'completed') {
				return { completed: result.completed, detail: { endPosition: result.endPosition } };
			}
			const failures = result.failures.map((f) => `${f.key} (${f.reason})`).join(', ');
			throw new Error(
				`routine ${result.routineId} ended ${result.status}: ${failures || 'no steps completed'}`
			);
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
