// Server-side SSRF guard for provider requests. The web deployment proxies
// model-list and chat requests to a client-supplied base URL; without this,
// anyone could point it at internal addresses (cloud metadata, localhost
// services, private ranges). The desktop build talks to providers directly and
// never hits these routes, so this only gates the hosted web path.

// Parse a single IPv4 octet that may be written in decimal, 0x-hex, or 0-octal
// form. Returns null if it isn't a valid integer in the given radix.
function parseOctet(part: string): number | null {
	let value: number;
	if (/^0x[0-9a-f]+$/i.test(part)) value = parseInt(part, 16);
	else if (/^0[0-7]+$/.test(part)) value = parseInt(part, 8);
	else if (/^(?:0|[1-9][0-9]*)$/.test(part)) value = parseInt(part, 10);
	else return null;
	return Number.isNaN(value) ? null : value;
}

// Canonicalize any IPv4 representation to a 32-bit integer, matching how the OS
// resolver (inet_aton) reads them. Handles the encodings that trivially bypass a
// naive dotted-decimal check: integer (2130706433), hex (0x7f000001),
// octal (0177.0.0.1), and short forms (127.1 -> 127.0.0.1). Returns null if the
// host isn't a numeric IPv4 form.
export function ipv4ToInt(host: string): number | null {
	const parts = host.split('.');
	if (parts.length === 0 || parts.length > 4) return null;

	const nums: number[] = [];
	for (const part of parts) {
		const n = parseOctet(part);
		if (n === null || n < 0) return null;
		nums.push(n);
	}

	// inet_aton: the final part absorbs all remaining low-order bytes; earlier
	// parts must each fit in one byte.
	const last = nums[nums.length - 1];
	const leading = nums.slice(0, -1);
	if (leading.some((n) => n > 0xff)) return null;
	const maxLast = 2 ** (8 * (4 - leading.length));
	if (last >= maxLast) return null;

	let result = last;
	for (let i = 0; i < leading.length; i++) {
		result += leading[i] * 2 ** (8 * (3 - i));
	}
	return result >>> 0;
}

function isPrivateIPv4Int(ip: number): boolean {
	const a = (ip >>> 24) & 0xff;
	const b = (ip >>> 16) & 0xff;
	if (a === 127) return true; // loopback
	if (a === 10) return true; // private
	if (a === 172 && b >= 16 && b <= 31) return true; // private
	if (a === 192 && b === 168) return true; // private
	if (a === 169 && b === 254) return true; // link-local + cloud metadata
	if (a === 0) return true; // "this" network
	return false;
}

// Literal IPv4/IPv6 hosts and hostnames that must not be reachable through the
// proxy. Covers loopback, private ranges, link-local (incl. 169.254.169.254
// cloud metadata), and unspecified addresses — across decimal, hex, octal, and
// short-form IPv4 encodings.
export function isPrivateHost(hostname: string): boolean {
	const host = hostname.toLowerCase().replace(/^\[|\]$/g, ''); // strip IPv6 brackets

	if (host === 'localhost' || host.endsWith('.localhost')) return true;
	if (host === '' || host === '::' || host === '::1') return true;

	// IPv6 loopback/link-local/unique-local. Anchor fc/fd to a hextet boundary so
	// real hostnames like "fcbanking.com" aren't misclassified.
	if (host.startsWith('fe80:')) return true;
	if (/^f[cd][0-9a-f]{0,2}:/.test(host)) return true;
	// IPv4-mapped IPv6 (e.g. ::ffff:127.0.0.1) — fall through to the IPv4 check
	const mapped = host.startsWith('::ffff:') ? host.slice(7) : host;

	const ip = ipv4ToInt(mapped);
	if (ip !== null) return isPrivateIPv4Int(ip);

	return false;
}

/** Resolves a hostname to IP strings (v4 and/or v6 literals). */
export type HostResolver = (hostname: string) => Promise<string[]>;

/** True when the host is already a numeric IP literal (fully judged by string checks). */
function isNumericProviderHost(hostname: string): boolean {
	const host = hostname.toLowerCase().replace(/^\[|\]$/g, '');
	if (ipv4ToInt(host) !== null) return true;
	if (host.startsWith('::ffff:') && ipv4ToInt(host.slice(7)) !== null) return true;
	// Any colon means IPv6 literal form (hostnames never contain ':').
	return host.includes(':');
}

// Validate a resolved provider base URL before the server fetches it. Returns the
// parsed URL, or throws if it uses a non-HTTP scheme or targets a private host.
// Set allowPrivate (self-hosters running local models behind the web server) to
// permit loopback/private targets.
export function assertSafeProviderUrl(rawUrl: string, allowPrivate = false): URL {
	let url: URL;
	try {
		url = new URL(rawUrl);
	} catch {
		throw new Error('Invalid provider URL');
	}

	if (url.protocol !== 'http:' && url.protocol !== 'https:') {
		throw new Error('Provider URL must use http or https');
	}

	if (!allowPrivate && isPrivateHost(url.hostname)) {
		throw new Error('Provider URL host is not allowed');
	}

	return url;
}

/**
 * `assertSafeProviderUrl` plus DNS resolution: every answer for a non-numeric
 * hostname must also clear the private-host check, so a hostile name cannot
 * launder a blocked address (DNS rebinding). Use with a redirect-guarded
 * fetch so 3xx targets are re-validated hop by hop.
 */
export async function assertSafeProviderUrlResolved(
	rawUrl: string,
	resolveHost: HostResolver,
	allowPrivate = false
): Promise<URL> {
	const url = assertSafeProviderUrl(rawUrl, allowPrivate);
	if (allowPrivate || isNumericProviderHost(url.hostname)) return url;
	let answers: string[];
	try {
		answers = await resolveHost(url.hostname);
	} catch {
		throw new Error('Provider hostname did not resolve');
	}
	if (answers.length === 0) throw new Error('Provider hostname did not resolve');
	for (const answer of answers) {
		if (isPrivateHost(answer)) {
			throw new Error('Provider hostname resolves to a blocked address');
		}
	}
	return url;
}
