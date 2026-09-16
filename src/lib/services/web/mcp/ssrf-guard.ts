// WEB-ONLY EXECUTION — never import from native code paths. Native MCP runs in
// Rust (crates/mcp-runtime); this module serves web chat + SvelteKit routes only.
/** Server-side SSRF guard for the MCP proxy.
 *
 * Policy (narrower than the provider-URL guard on purpose): link-local,
 * loopback, unspecified, and multicast targets are always blocked, while
 * private LAN ranges stay reachable so self-hosted servers like Home
 * Assistant keep working through the proxy. Hostnames are resolved and
 * EVERY resolved address is checked (DNS-rebinding protection); redirect
 * targets are validated hop by hop by the guarded fetch.
 *
 * Pure except for the injected DNS resolver — no node imports here, so unit
 * tests and shared code stay portable. The node-backed resolver lives in
 * the server-only `src/routes/api/mcp/dns` module.
 */
import { ipv4ToInt } from '../../providers/url-guard.ts';

/** Strip IPv6 brackets and any `%zone` suffix, lowercase. */
function normalizeHost(hostname: string): string {
	let host = hostname.trim().toLowerCase();
	if (host.startsWith('[') && host.endsWith(']')) host = host.slice(1, -1);
	const zone = host.indexOf('%');
	if (zone !== -1) host = host.slice(0, zone);
	return host;
}

function isBlockedIPv4Int(ip: number): boolean {
	const a = (ip >>> 24) & 0xff;
	const b = (ip >>> 16) & 0xff;
	if (a === 127) return true; // loopback
	if (a === 0) return true; // "this" network
	if (a >= 224 && a <= 239) return true; // multicast
	if (a === 169 && b === 254) return true; // link-local + cloud metadata
	if (a >= 240) return true; // reserved
	return false;
}

/** Literal-host check: names and numeric IPs that must never be fetched.
 * Returns true when blocked. Private LAN (10/8, 172.16/12, 192.168/16,
 * fc00::/7) is deliberately ALLOWED — see the module doc. */
export function isBlockedMcpHost(hostname: string): boolean {
	const host = normalizeHost(hostname);
	if (!host) return true;
	if (host === 'localhost' || host.endsWith('.localhost')) return true;
	if (host === '::' || host === '::1') return true;
	// IPv6 link-local (fe80::/10) and multicast (ff00::/8).
	if (host === 'fe80' || host.startsWith('fe80:')) return true;
	if (/^fe[89ab][0-9a-f]{0,2}:/.test(host)) return true;
	if (host.startsWith('ff')) {
		// Must be a hex group, not a hostname like "ffbanking.com".
		if (/^ff[0-9a-f]{0,2}:/.test(host) || /^ff[0-9a-f]{1,4}$/.test(host)) return true;
	}
	// IPv4-mapped IPv6 (::ffff:1.2.3.4) — judge the embedded v4 address.
	const mapped = host.startsWith('::ffff:') ? host.slice(7) : host;
	// IPv4-compatible / 6to4 / Teredo forms embed a v4 tail after the last ':'.
	const tail = mapped.includes(':') ? (mapped.split(':').pop() ?? '') : mapped;
	const ip = ipv4ToInt(tail.includes('.') ? tail : mapped);
	if (ip !== null) return isBlockedIPv4Int(ip);
	// Non-numeric hostnames pass the literal check; DNS resolution below has
	// the final word.
	return false;
}

/** Resolves a hostname to IP strings (v4 and/or v6 literals). */
export type HostResolver = (hostname: string) => Promise<string[]>;

/** Validate a full MCP server URL: http(s) scheme, literal check, then DNS
 * resolution with every answer checked. Returns the parsed URL. */
export async function assertSafeMcpUrl(rawUrl: string, resolveHost: HostResolver): Promise<URL> {
	let url: URL;
	try {
		url = new URL(rawUrl);
	} catch {
		throw new Error('Invalid MCP server URL');
	}
	if (url.protocol !== 'http:' && url.protocol !== 'https:') {
		throw new Error('MCP server URL must use http or https');
	}
	if (isBlockedMcpHost(url.hostname)) {
		throw new Error('MCP server host is not allowed');
	}
	// Numeric literals were fully judged above; resolve everything else so a
	// hostile name cannot launder a blocked address (DNS rebinding).
	if (!isNumericHost(url.hostname)) {
		let answers: string[];
		try {
			answers = await resolveHost(url.hostname);
		} catch {
			throw new Error('MCP server hostname did not resolve');
		}
		if (answers.length === 0) throw new Error('MCP server hostname did not resolve');
		for (const answer of answers) {
			if (isBlockedMcpHost(answer)) {
				throw new Error('MCP server host resolves to a blocked address');
			}
		}
	}
	return url;
}

function isNumericHost(hostname: string): boolean {
	const host = normalizeHost(hostname);
	if (ipv4ToInt(host) !== null) return true;
	if (host.startsWith('::ffff:') && ipv4ToInt(host.slice(7)) !== null) return true;
	// Any colon means IPv6 literal form (hostnames never contain ':').
	return host.includes(':');
}
