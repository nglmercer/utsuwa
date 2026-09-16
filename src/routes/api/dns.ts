/** Server-only node DNS resolver for the SSRF guards (MCP + provider routes).
 * Lives beside the routes (never imported by browser code) so the
 * `node:dns` import can never leak into a client bundle.
 */
import type { HostResolver } from '../../lib/services/web/mcp/ssrf-guard.ts';

export const nodeHostResolver: HostResolver = async (hostname: string): Promise<string[]> => {
	const dns = await import('node:dns/promises');
	const [v4, v6] = await Promise.allSettled([dns.resolve4(hostname), dns.resolve6(hostname)]);
	const answers: string[] = [];
	if (v4.status === 'fulfilled') answers.push(...v4.value);
	if (v6.status === 'fulfilled') answers.push(...v6.value);
	if (answers.length === 0) throw new Error(`Could not resolve ${hostname}`);
	return answers;
};
