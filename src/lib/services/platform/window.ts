export interface WindowPosition {
	x: number;
	y: number;
}

export interface WindowSize {
	width: number;
	height: number;
}

// Window management used to go through the Tauri backend (multi-window
// overlay, always-on-top, click-through). That backend is removed: the
// native host (app-host) owns a single window, so these are documented
// no-ops kept for call-site compatibility. Window behavior that needs a
// host API should grow on the utsuwa IPC bridge instead.

/**
 * Set window position (no backend: no-op).
 */
export async function setWindowPosition(_position: WindowPosition): Promise<void> {
	return;
}

/**
 * Get current window position (no backend: always null).
 */
export async function getWindowPosition(): Promise<WindowPosition | null> {
	return null;
}

/**
 * Set whether the window should ignore cursor events (no backend: no-op).
 */
export async function setIgnoreCursorEvents(_ignore: boolean): Promise<void> {
	return;
}

/**
 * Set window always on top state (no backend: no-op).
 */
export async function setAlwaysOnTop(_alwaysOnTop: boolean): Promise<void> {
	return;
}

/**
 * Show/hide the window (no backend: no-op).
 */
export async function setWindowVisible(_visible: boolean): Promise<void> {
	return;
}

/**
 * Start dragging the window (no backend: no-op).
 */
export async function startDragging(): Promise<void> {
	return;
}
