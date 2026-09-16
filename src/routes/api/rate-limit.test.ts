import test from 'node:test';
import assert from 'node:assert/strict';

import { checkRateLimit, resetRateLimits, ruleForPath } from './rate-limit.ts';

test('chat is limited tighter than status probes', () => {
	assert.deepEqual(ruleForPath('/api/chat'), { capacity: 8, refillPerSecond: 20 / 60 });
	assert.deepEqual(ruleForPath('/api/mcp/status'), { capacity: 60, refillPerSecond: 120 / 60 });
	assert.deepEqual(ruleForPath('/api/mcp/call'), { capacity: 20, refillPerSecond: 60 / 60 });
});

test('buckets exhaust, refuse with retry-after, then refill', () => {
	resetRateLimits();
	const now = 1_000_000;
	for (let i = 0; i < 8; i++) {
		assert.equal(checkRateLimit('/api/chat', '1.2.3.4', now).allowed, true);
	}
	const refused = checkRateLimit('/api/chat', '1.2.3.4', now);
	assert.equal(refused.allowed, false);
	assert.ok(refused.retryAfterSec >= 1);
	// A different client is unaffected.
	assert.equal(checkRateLimit('/api/chat', '5.6.7.8', now).allowed, true);
	// Refill: 20/min = 1 token per 3s.
	assert.equal(checkRateLimit('/api/chat', '1.2.3.4', now + 3000).allowed, true);
	assert.equal(checkRateLimit('/api/chat', '1.2.3.4', now + 3000).allowed, false);
	resetRateLimits();
});
