/** Server-only guarded fetch for proxied MCP HTTP traffic: the shared
 * redirect-guarded fetch with the MCP URL policy (DNS resolution included).
 */
import { assertSafeMcpUrl, type HostResolver } from '../../../lib/services/web/mcp/ssrf-guard.ts';
import type { FetchImpl } from '../../../lib/services/web/mcp/http-client.ts';
import { createRedirectGuardedFetch } from '../guarded-fetch.ts';

export function createGuardedFetch(resolveHost: HostResolver): FetchImpl {
	return createRedirectGuardedFetch((rawUrl: string) => assertSafeMcpUrl(rawUrl, resolveHost));
}
