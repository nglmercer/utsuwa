// Browser task orchestrator singleton: Dexie durability, avatar bridge,
// notification sink. Started once from the app shell; tasks submitted here
// survive close/reopen via IndexedDB and resume on the next boot.
import { DexieTaskStore } from './dexie-store.ts';
import { TaskEventBus } from './events.ts';
import { createBrowserAvatarRunner, createBrowserNotifier } from './avatar-bridge.ts';
import { TaskOrchestrator } from './orchestrator.ts';

const bus = new TaskEventBus();
const notifier = createBrowserNotifier();

export const taskOrchestrator = new TaskOrchestrator({
	store: new DexieTaskStore(),
	bus,
	avatarRunner: createBrowserAvatarRunner(bus),
	notifier,
	pollMs: 5000
});

export const taskNotifications = notifier;
