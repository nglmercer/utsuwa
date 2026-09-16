/** Server-only per-IP rate limiter for `/api/*`.
 *
 * Token buckets keyed by client IP + route class. In-memory: correct for the
 * standard single-instance deployment; multi-instance deployments should put
 * authenticated ingress (or a shared limiter) in front — see report.md.
 *
 * Pure except for the clock argument, so the policy is unit-testable under
 * node. `src/hooks.server.ts` is the only production caller.
 */

export interface RateLimitRule {
	/** Bucket capacity (burst). */
	capacity: number;
	/** Sustained refill rate, tokens per second. */
	refillPerSecond: number;
}

const RULES: { prefix: string; rule: RateLimitRule }[] = [
	// LLM streaming: the most expensive operation on the box.
	{ prefix: '/api/chat', rule: { capacity: 8, refillPerSecond: 20 / 60 } },
	// Process-spawning / outbound-fetching proxy calls.
	{ prefix: '/api/mcp/call', rule: { capacity: 20, refillPerSecond: 60 / 60 } },
	{ prefix: '/api/mcp/tools', rule: { capacity: 20, refillPerSecond: 60 / 60 } },
	// Model catalog fetches.
	{ prefix: '/api/providers/models', rule: { capacity: 20, refillPerSecond: 60 / 60 } },
	// Cheap status probes and anything else under /api.
	{ prefix: '/api', rule: { capacity: 60, refillPerSecond: 120 / 60 } }
];

interface Bucket {
	tokens: number;
	updatedAtMs: number;
	rule: RateLimitRule;
}

const buckets = new Map<string, Bucket>();

export function ruleForPath(pathname: string): RateLimitRule {
	for (const { prefix, rule } of RULES) {
		if (pathname === prefix || pathname.startsWith(`${prefix}/`)) {
			return rule;
		}
	}
	return { capacity: 60, refillPerSecond: 2 };
}

export interface RateLimitVerdict {
	allowed: boolean;
	/** Whole seconds the client should wait before retrying (0 when allowed). */
	retryAfterSec: number;
}

/** Take one token for `key` (usually the client IP). Refills lazily. */
export function checkRateLimit(
	pathname: string,
	key: string,
	nowMs: number = Date.now()
): RateLimitVerdict {
	const rule = ruleForPath(pathname);
	const mapKey = `${pathname}|${key}`;
	// Cheap hygiene: drop buckets idle longer than a full refill from empty.
	if (buckets.size > 0 && Math.random() < 0.01) {
		for (const [k, bucket] of buckets) {
			const idleMs = nowMs - bucket.updatedAtMs;
			if (idleMs > (bucket.rule.capacity / bucket.rule.refillPerSecond) * 1000 + 60_000) {
				buckets.delete(k);
			}
		}
	}
	let bucket = buckets.get(mapKey);
	if (!bucket) {
		bucket = { tokens: rule.capacity, updatedAtMs: nowMs, rule };
		buckets.set(mapKey, bucket);
	}
	const elapsedSec = Math.max(0, (nowMs - bucket.updatedAtMs) / 1000);
	bucket.tokens = Math.min(rule.capacity, bucket.tokens + elapsedSec * rule.refillPerSecond);
	bucket.updatedAtMs = nowMs;
	if (bucket.tokens >= 1) {
		bucket.tokens -= 1;
		return { allowed: true, retryAfterSec: 0 };
	}
	const retryAfterSec = Math.max(1, Math.ceil((1 - bucket.tokens) / rule.refillPerSecond));
	return { allowed: false, retryAfterSec };
}

/** Drop all buckets (tests). */
export function resetRateLimits(): void {
	buckets.clear();
}
