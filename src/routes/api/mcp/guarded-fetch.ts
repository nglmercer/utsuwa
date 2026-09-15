/** Server-only guarded fetch for proxied MCP HTTP traffic.
 *
 * Every request URL — and every redirect hop (up to 3) — passes the SSRF
 * guard (scheme, literal blocks, DNS resolution). Authorization is dropped
 * when a redirect crosses origins. Non-GET-preserving redirects (301/302/303)
 * downgrade to GET per fetch semantics; 307/308 replay method and body.
 */
import { assertSafeMcpUrl, type HostResolver } from '../../../lib/services/mcp/ssrf-guard.ts';
import type { FetchImpl } from '../../../lib/services/mcp/http-client.ts';

const MAX_REDIRECTS = 3;

function headerRecord(headers: HeadersInit | undefined): Record<string, string> {
	const record: Record<string, string> = {};
	new Headers(headers).forEach((value, key) => {
		record[key] = value;
	});
	return record;
}

export function createGuardedFetch(resolveHost: HostResolver): FetchImpl {
	return async (rawUrl: string, init: RequestInit): Promise<Response> => {
		let current = rawUrl;
		let method = (init.method ?? 'GET').toUpperCase();
		let body = init.body;
		let headers = headerRecord(init.headers);
		const startOrigin = new URL(rawUrl).origin;

		for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
			await assertSafeMcpUrl(current, resolveHost);
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
		throw new Error('MCP proxy: too many redirects');
	};
}
