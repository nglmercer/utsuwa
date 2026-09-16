/** Server-only redirect-guarded fetch factory.
 *
 * Wraps `fetch` so the initial URL — and every redirect hop (up to 3) — passes
 * a caller-supplied async validator before any bytes are sent. `Authorization`
 * is dropped when a redirect crosses origins. Non-GET-preserving redirects
 * (301/302/303) downgrade to GET per fetch semantics; 307/308 replay method
 * and body.
 *
 * Both the MCP proxy and the provider routes build on this: same redirect
 * discipline, different per-hop URL policies.
 */
import type { FetchImpl } from '../../lib/services/web/mcp/http-client.ts';

const MAX_REDIRECTS = 3;

/** Validate one request URL; throw to refuse the hop. */
export type UrlValidator = (rawUrl: string) => Promise<unknown>;

function headerRecord(headers: HeadersInit | undefined): Record<string, string> {
	const record: Record<string, string> = {};
	new Headers(headers).forEach((value, key) => {
		record[key] = value;
	});
	return record;
}

export function createRedirectGuardedFetch(validateUrl: UrlValidator): FetchImpl {
	return async (rawUrl: string, init: RequestInit): Promise<Response> => {
		let current = rawUrl;
		let method = (init.method ?? 'GET').toUpperCase();
		let body = init.body;
		let headers = headerRecord(init.headers);
		const startOrigin = new URL(rawUrl).origin;

		for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
			await validateUrl(current);
			const response = await fetch(current, {
				...init,
				method,
				headers,
				body: body as BodyInit | null | undefined,
				redirect: 'manual'
			});
			if (response.status < 300 || response.status >= 400) return response;
			const location = response.headers.get('location');
			if (!location) return response;
			await response.body?.cancel().catch(() => undefined);
			const next = new URL(location, current);
			if (next.origin !== startOrigin) {
				for (const key of Object.keys(headers)) {
					if (key.toLowerCase() === 'authorization') delete headers[key];
				}
			}
			if (response.status === 303 || (response.status !== 307 && response.status !== 308 && method === 'POST')) {
				method = 'GET';
				body = undefined;
				for (const key of Object.keys(headers)) {
					if (key.toLowerCase() === 'content-type' || key.toLowerCase() === 'content-length') {
						delete headers[key];
					}
				}
			}
			current = next.toString();
		}
		throw new Error('proxy: too many redirects');
	};
}
