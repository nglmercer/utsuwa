/** The capture device a media request is trying to use. */
export type MediaDeviceKind = 'microphone' | 'camera';

/** Device-independent categories for getUserMedia failures. */
export type MediaErrorCategory =
	| 'permission-denied'
	| 'not-found'
	| 'busy'
	| 'unsupported'
	| 'constraints'
	| 'unknown';

function getStringProperty(error: unknown, property: 'name' | 'message'): string | undefined {
	if ((typeof error !== 'object' && typeof error !== 'function') || error === null) return undefined;
	const value = (error as Record<string, unknown>)[property];
	return typeof value === 'string' ? value : undefined;
}

/**
 * Classify a getUserMedia error without relying on a particular DOM realm.
 * Embedded webviews can give us DOMException instances from another realm,
 * so checking the exception name is more portable than `instanceof`.
 */
export function classifyMediaError(error: unknown): MediaErrorCategory {
	switch (getStringProperty(error, 'name')) {
		case 'NotAllowedError':
		case 'SecurityError':
			return 'permission-denied';
		case 'NotFoundError':
			return 'not-found';
		case 'NotReadableError':
		case 'AbortError':
			return 'busy';
		case 'NotSupportedError':
		case 'TypeError':
			return 'unsupported';
		case 'OverconstrainedError':
		case 'ConstraintNotSatisfiedError':
			return 'constraints';
		default:
			return 'unknown';
	}
}

/**
 * Turn a media error into a user-facing message while keeping the category
 * reusable for both microphone and future camera capture.
 */
export function getMediaErrorMessage(device: MediaDeviceKind, error: unknown): string {
	const label = device === 'microphone' ? 'Microphone' : 'Camera';
	const deviceName = device;

	switch (classifyMediaError(error)) {
		case 'permission-denied':
			return `${label} access denied. Check system permissions.`;
		case 'not-found':
			return `No ${deviceName} found. Please connect a ${deviceName}.`;
		case 'busy':
			return `${label} is busy or in use by another app.`;
		case 'unsupported':
			return `${label} access is not supported in this browser.`;
		case 'constraints':
			return `${label} does not meet requirements.`;
		case 'unknown': {
			const message = getStringProperty(error, 'message');
			return message ? `${label} error: ${message}` : `Failed to access ${deviceName}`;
		}
	}
}
