export type HotkeyAction = 'pushToTalk' | 'toggleOverlay' | 'focusChat';

export interface HotkeyConfig {
	pushToTalk: string;
	toggleOverlay: string;
	focusChat: string;
}

export const DEFAULT_HOTKEYS: HotkeyConfig = {
	pushToTalk: 'Ctrl+Shift+Space',
	toggleOverlay: 'Ctrl+Shift+U',
	focusChat: 'Ctrl+Shift+C'
};

type HotkeyHandler = () => void;
type KeyUpHandler = () => void;

const handlers: Map<HotkeyAction, { onKeyDown: HotkeyHandler; onKeyUp?: KeyUpHandler }> = new Map();
const registeredShortcuts: Map<HotkeyAction, string> = new Map();

/**
 * Register a global hotkey handler. No global-shortcut backend exists
 * (the Tauri backend is removed), so this always reports failure and
 * records nothing. Kept for call-site compatibility.
 */
export async function registerHotkey(
	_action: HotkeyAction,
	_shortcut: string,
	_onKeyDown: HotkeyHandler,
	_onKeyUp?: KeyUpHandler
): Promise<boolean> {
	return false;
}

/**
 * Unregister a global hotkey (no backend: no-op).
 */
export async function unregisterHotkey(_action: HotkeyAction): Promise<void> {
	return;
}

/**
 * Unregister all global hotkeys (no backend: no-op).
 */
export async function unregisterAllHotkeys(): Promise<void> {
	return;
}

/**
 * Check if global hotkeys are supported in the current environment
 */
export function isHotkeysSupported(): boolean {
	return false;
}
