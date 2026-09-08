// Turns raw provider failures into short, readable messages. A misconfigured
// base URL usually points at a website, so the failure body is a full HTML
// page — never show that to the user.

const HTML_MARKERS = ['<!doctype', '<html', '<head>', '<body'];
const MAX_ERROR_LENGTH = 240;

function safeEndpointReference(baseURL?: string): string {
	if (!baseURL) return '';
	try {
		const url = new URL(baseURL);
		return `${url.origin}${url.pathname}`.replace(/\/+$/, '');
	} catch {
		return baseURL.split(/[?#]/, 1)[0];
	}
}

export function looksLikeHtml(text: string | null | undefined): boolean {
	if (!text) return false;
	const head = text.slice(0, 500).trim().toLowerCase();
	return HTML_MARKERS.some((marker) => head.includes(marker));
}

export function htmlEndpointError(baseURL?: string): string {
	const safeBaseURL = safeEndpointReference(baseURL);
	const at = safeBaseURL ? ` at ${safeBaseURL}` : '';
	return `The endpoint${at} returned a web page instead of an API response. Double-check the base URL (for OpenAI it's https://api.openai.com/v1/).`;
}

/** Collapse HTML dumps and cap length so an error can't flood the UI. */
export function sanitizeProviderError(message: string, baseURL?: string): string {
	if (looksLikeHtml(message)) return htmlEndpointError(baseURL);
	const safeMessage = baseURL
		? message.replaceAll(baseURL, safeEndpointReference(baseURL))
		: message;
	if (safeMessage.length > MAX_ERROR_LENGTH) return `${safeMessage.slice(0, MAX_ERROR_LENGTH)}…`;
	return safeMessage.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, ' ');
}
