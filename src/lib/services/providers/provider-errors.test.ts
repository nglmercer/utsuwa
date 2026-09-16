import test from 'node:test';
import assert from 'node:assert/strict';

import {
	htmlEndpointError,
	looksLikeHtml,
	sanitizeProviderError
} from './provider-errors.ts';

test('looksLikeHtml detects page dumps, not JSON errors', () => {
	assert.equal(looksLikeHtml('<!doctype html><html>'), true);
	assert.equal(looksLikeHtml('<html><body>nope'), true);
	assert.equal(looksLikeHtml('{"error":"bad key"}'), false);
	assert.equal(looksLikeHtml(''), false);
	assert.equal(looksLikeHtml(null), false);
});

test('endpoint references never leak userinfo or queries', () => {
	assert.equal(
		htmlEndpointError('https://user:s3cret@api.example.com/v1/?k=1#frag'),
		'The endpoint at https://api.example.com/v1 returned a web page instead of an API response. Double-check the base URL (for OpenAI it\'s https://api.openai.com/v1/).'
	);
	// Unparseable input keeps no credentials either.
	const broken = 'https://a:b@exa mple.com:bad/';
	const scrubbed = sanitizeProviderError(`fetch failed at ${broken} retry`, broken);
	assert.doesNotMatch(scrubbed, /a:b/);
	assert.match(scrubbed, /https:\/\/exa mple\.com:bad\//);
});

test('sanitizeProviderError collapses HTML and caps length', () => {
	const page = `<html><body>${'x'.repeat(500)}</body></html>`;
	const message = sanitizeProviderError(page, 'https://api.example.com/v1');
	assert.doesNotMatch(message, /<html/);
	assert.ok(message.length <= 280);
	assert.equal(sanitizeProviderError('short', undefined), 'short');
});
