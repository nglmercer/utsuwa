import test from 'node:test';
import assert from 'node:assert/strict';

import { canShowImages } from './vision.ts';

test('showing images needs an explicitly supported image-input capability', () => {
	assert.equal(canShowImages({ capabilities: { imageInput: 'supported' } }), true);
	assert.equal(canShowImages({ capabilities: { imageInput: 'unsupported' } }), false);
	assert.equal(canShowImages({ capabilities: { imageInput: 'unknown' } }), false);
	assert.equal(canShowImages({ capabilities: {} }), false);
	assert.equal(canShowImages({}), false);
	assert.equal(canShowImages(undefined), false);
	assert.equal(canShowImages(null), false);
});

test('model names never imply vision, however vision-capable they look', () => {
	// A vision-y id with silent or negative metadata must not enable showing.
	for (const name of ['llava:13b', 'gpt-4o', 'qwen2.5-vl-7b', 'super-vision-9000']) {
		assert.equal(canShowImages({ id: name, name }), false);
		assert.equal(
			canShowImages({ id: name, name, capabilities: { imageInput: 'unknown' } }),
			false
		);
	}
	// And a plain id with advertised support does enable it.
	assert.equal(
		canShowImages({
			id: 'plain-name-7b',
			name: 'Plain',
			capabilities: { imageInput: 'supported' }
		}),
		true
	);
});

test('pre-migration cached vision flags keep working until refresh', () => {
	// Old parsers derived `vision` from the same provider metadata.
	assert.equal(canShowImages({ capabilities: { vision: true } }), true);
	assert.equal(canShowImages({ capabilities: { vision: false } }), false);
	// New parsers never emit a mismatched mix; if one ever appears the
	// metadata-derived alias still opens the gate.
	assert.equal(
		canShowImages({ capabilities: { imageInput: 'unsupported', vision: true } }),
		true
	);
});
