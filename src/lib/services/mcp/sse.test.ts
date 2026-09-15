import test from 'node:test';
import assert from 'node:assert/strict';

import { parseSseBody, parseSseJsonPayloads } from './sse.ts';

test('SSE bodies split into event blocks with data lines joined', () => {
	const messages = parseSseBody(': comment\n\ndata: {"a":1}\n\ndata: {"b":\ndata: 2}\n\n');
	assert.equal(messages.length, 2);
	assert.deepEqual(messages[0], { data: ['{"a":1}'] });
	assert.deepEqual(messages[1], { data: ['{"b":', '2}'] });
});

test('SSE event names are captured and blank payloads skipped', () => {
	assert.deepEqual(parseSseBody('event: message\ndata: {"x":1}\n\n'), [
		{ event: 'message', data: ['{"x":1}'] }
	]);
	assert.deepEqual(parseSseJsonPayloads('event: ping\n\ndata: {"x":1}\n\n'), [{ x: 1 }]);
	assert.deepEqual(parseSseJsonPayloads(''), []);
});

test('SSE payload parsing throws on malformed JSON', () => {
	assert.throws(() => parseSseJsonPayloads('data: not-json\n\n'), SyntaxError);
});
