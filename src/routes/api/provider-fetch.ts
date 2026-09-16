/** Server-only provider fetch: the shared redirect-guarded fetch with the
 * provider URL policy (strict private-host blocking unless the deployment
 * opts into local hosts, plus DNS resolution against rebinding).
 *
 * Every server-side provider request (model catalogs, chat completions) must
 * go through this — never a bare `fetch` — because the base URL is
 * client-supplied and the string-only check cannot see DNS answers or
 * redirect targets.
 */
import {
	assertSafeProviderUrlResolved,
	type HostResolver
} from '$lib/services/providers/url-guard';
import type { FetchImpl } from '$lib/services/web/mcp/http-client';
import { createRedirectGuardedFetch } from './guarded-fetch.ts';

export function createProviderFetch(resolveHost: HostResolver, allowPrivate: boolean): FetchImpl {
	return createRedirectGuardedFetch((rawUrl: string) =>
		assertSafeProviderUrlResolved(rawUrl, resolveHost, allowPrivate)
	);
}

/**
 * Adapt a guarded fetch to the global-fetch shape for HTTP clients typed on
 * it (e.g. xsai `streamText`). Callers pass `(url, init)` positionally; a
 * `Request` input contributes only its URL.
 */
export function asGlobalFetch(fetchImpl: FetchImpl): typeof fetch {
	return (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
		const url =
			typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url;
		return fetchImpl(url, init ?? {});
	};
}
