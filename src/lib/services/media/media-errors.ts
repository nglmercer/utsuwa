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

export interface MediaErrorDetails {
	name?: string;
	message?: string;
}

/** Browser/WebView facts that help distinguish permission from platform setup. */
export interface MediaEnvironmentDetails {
	origin?: string;
	isSecureContext?: boolean;
	hasMediaDevices: boolean;
	hasGetUserMedia: boolean;
}

/** A user-facing media failure together with the details needed to diagnose it. */
export interface MediaAccessErrorDetails extends MediaErrorDetails, MediaEnvironmentDetails {
	category: MediaErrorCategory;
	userMessage: string;
}

function getStringProperty(error: unknown, property: 'name' | 'message'): string | undefined {
	if ((typeof error !== 'object' && typeof error !== 'function') || error === null) return undefined;
	const value = (error as Record<string, unknown>)[property];
	return typeof value === 'string' ? value : undefined;
}

/** Read standard error fields without requiring a same-realm DOMException. */
export function getMediaErrorDetails(error: unknown): MediaErrorDetails {
	const details: MediaErrorDetails = {};
	const name = getStringProperty(error, 'name');
	const message = getStringProperty(error, 'message');
	if (name !== undefined) details.name = name;
	if (message !== undefined) details.message = message;
	return details;
}

/** Read the runtime capabilities of the current page without touching media. */
export function getMediaEnvironmentDetails(): MediaEnvironmentDetails {
	const mediaDevices = typeof navigator !== 'undefined' ? navigator.mediaDevices : undefined;
	return {
		origin: typeof window !== 'undefined' ? window.location.origin : undefined,
		isSecureContext: typeof window !== 'undefined' ? window.isSecureContext : undefined,
		hasMediaDevices: !!mediaDevices,
		hasGetUserMedia: typeof mediaDevices?.getUserMedia === 'function'
	};
}

/**
 * Classify a getUserMedia error without relying on a particular DOM realm.
 * Embedded webviews can give us DOMException instances from another realm,
 * so checking the exception name is more portable than `instanceof`.
 */
export function classifyMediaError(error: unknown): MediaErrorCategory {
	switch (getMediaErrorDetails(error).name) {
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
			return `${label} access was denied.`;
		case 'not-found':
			return `No ${deviceName} found. Please connect a ${deviceName}.`;
		case 'busy':
			return `${label} is busy or in use by another app.`;
		case 'unsupported':
			return `${label} access is not supported in this browser.`;
		case 'constraints':
			return `${label} does not meet requirements.`;
		case 'unknown': {
			const message = getMediaErrorDetails(error).message;
			return message ? `${label} error: ${message}` : `Failed to access ${deviceName}`;
		}
	}
}

/** Build the same diagnostic payload for microphone and future camera services. */
export function getMediaAccessErrorDetails(
	device: MediaDeviceKind,
	error: unknown,
	environment: MediaEnvironmentDetails = getMediaEnvironmentDetails()
): MediaAccessErrorDetails {
	const details = getMediaErrorDetails(error);
	return {
		...details,
		...environment,
		category: classifyMediaError(error),
		userMessage: getMediaErrorMessage(device, error)
	};
}
