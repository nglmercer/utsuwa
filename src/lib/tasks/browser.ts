// Task authority boot: the native host (SQLite) is authoritative whenever
// the bridge exists; the Dexie browser orchestrator is the fallback for
// plain browsers. On host cutover, pending Dexie tasks migrate once to the
// host (originals cancelled so nothing runs twice) and the browser tick
// loop stays off. The avatar receipt listener always starts: it answers
// `avatar.routine.requested` with renderer receipts via `task.event`.
import { runAvatarRoutineAndWait } from '$lib/services/avatar/routine';
import { createBrowserAvatarRunner, createBrowserNotifier } from './avatar-bridge.ts';
import { DexieTaskStore } from './dexie-store.ts';
import { TaskEventBus } from './events.ts';
import { hostTasks, isHostTasksAvailable } from './host.ts';
import { startHostAvatarListener } from './host-avatar.ts';
import { migrateDexieTasksToHost, type MigrationReport } from './migration.ts';
import { TaskOrchestrator } from './orchestrator.ts';

const bus = new TaskEventBus();
const notifier = createBrowserNotifier();
const dexieStore = new DexieTaskStore();

export const taskOrchestrator = new TaskOrchestrator({
	store: dexieStore,
	bus,
	avatarRunner: createBrowserAvatarRunner(bus),
	notifier,
	pollMs: 5000
});

export const taskNotifications = notifier;

export type TaskAuthority = 'host' | 'browser';

export function resolveTaskAuthority(): TaskAuthority {
	return isHostTasksAvailable() ? 'host' : 'browser';
}

let avatarListenerOff: (() => void) | null = null;

export interface BootTasksReport {
	authority: TaskAuthority;
	migration: MigrationReport | null;
}

/** Boot task handling for the app shell. Safe to call once; use `shutdownTasks` on teardown. */
export async function bootTasks(): Promise<BootTasksReport> {
	if (typeof window !== 'undefined' && !avatarListenerOff) {
		avatarListenerOff = startHostAvatarListener({
			events: window,
			runRoutine: (steps, options) =>
				runAvatarRoutineAndWait(steps, { policy: options.policy, timeoutMs: options.timeoutMs }),
			deliver: (eventType, correlationId, payload) =>
				hostTasks().deliverEvent(eventType, correlationId, payload),
			log: (message) => console.warn(`[tasks] ${message}`)
		});
	}
	const authority = resolveTaskAuthority();
	if (authority === 'host') {
		const migration = await migrateDexieTasksToHost({
			store: dexieStore,
			client: hostTasks(),
			log: (message) => console.warn(`[tasks] ${message}`)
		});
		if (migration.migrated.length > 0) {
			console.info(`[tasks] migrated ${migration.migrated.length} Dexie task(s) to host`);
		}
		return { authority, migration };
	}
	taskOrchestrator.start();
	return { authority, migration: null };
}

export function shutdownTasks(): void {
	taskOrchestrator.stop();
	avatarListenerOff?.();
	avatarListenerOff = null;
}
