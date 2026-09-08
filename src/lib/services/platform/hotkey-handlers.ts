import type { HotkeyConfig } from './hotkeys';

// Event bus for hotkey actions (allows components to subscribe)
type HotkeyEventHandler = () => void;
const eventHandlers: Map<string, Set<HotkeyEventHandler>> = new Map();

/**
 * Subscribe to a hotkey event
 */
export function onHotkeyEvent(event: string, handler: HotkeyEventHandler): () => void {
	if (!eventHandlers.has(event)) {
		eventHandlers.set(event, new Set());
	}
	eventHandlers.get(event)!.add(handler);

	return () => {
		eventHandlers.get(event)?.delete(handler);
	};
}

/**
 * Emit a hotkey event
 */
export function emitHotkeyEvent(event: string): void {
	eventHandlers.get(event)?.forEach((handler) => handler());
}

// Global hotkeys used to go through the Tauri backend. That backend is
// removed, so initialization is a documented no-op kept for call-site
// compatibility. The local event bus above stays: in-app code can still
// emit and subscribe without a global-shortcut backend.
export async function initializeHotkeys(_config?: Partial<HotkeyConfig>): Promise<void> {
	return;
}
