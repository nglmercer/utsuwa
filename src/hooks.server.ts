import type { Handle, RequestEvent } from '@sveltejs/kit';
import { checkRateLimit } from './routes/api/rate-limit.ts';

// Request-body caps by route class. Chat carries scaled JPEG turns as base64
// (up to ~1MB per image after client scaling), so it gets headroom; every
// other API body is small JSON (server configs, tool args are capped again
// in-route).
const BODY_LIMITS: { prefix: string; maxBytes: number }[] = [
	{ prefix: '/api/chat', maxBytes: 12 * 1024 * 1024 },
	{ prefix: '/api/', maxBytes: 512 * 1024 }
];

function bodyLimitFor(pathname: string): number {
	for (const { prefix, maxBytes } of BODY_LIMITS) {
		if (pathname === prefix || pathname.startsWith(`${prefix}/`)) return maxBytes;
	}
	return 512 * 1024;
}

function clientIp(event: RequestEvent): string {
	try {
		return event.getClientAddress();
	} catch {
		return 'unknown';
	}
}

export const handle: Handle = async ({ event, resolve }) => {
	const pathname = event.url.pathname;
	if (event.request.method !== 'GET' && pathname.startsWith('/api/')) {
		const declared = event.request.headers.get('content-length');
		if (declared !== null) {
			const length = Number(declared);
			if (!Number.isFinite(length) || length < 0 || length > bodyLimitFor(pathname)) {
				return new Response(JSON.stringify({ error: 'Request body too large' }), {
					status: 413,
					headers: { 'Content-Type': 'application/json' }
				});
			}
		}
		const verdict = checkRateLimit(pathname, clientIp(event));
		if (!verdict.allowed) {
			return new Response(JSON.stringify({ error: 'Rate limit exceeded' }), {
				status: 429,
				headers: {
					'Content-Type': 'application/json',
					'Retry-After': String(verdict.retryAfterSec)
				}
			});
		}
	}
	return resolve(event);
};
