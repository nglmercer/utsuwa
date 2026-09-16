import test from 'node:test';
import assert from 'node:assert/strict';

import {
	DEFAULT_SCENE_ANCHORS,
	listSceneAnchors,
	resolveSceneAnchor,
	sittableAnchors
} from './scene-anchors.ts';

test('default anchors sit inside the walk radius', () => {
	for (const anchor of DEFAULT_SCENE_ANCHORS) {
		assert.ok(Math.hypot(anchor.x, anchor.z) <= 2, anchor.id);
		assert.ok(anchor.id.length > 0 && anchor.label.length > 0);
		if (anchor.sittable) {
			assert.ok((anchor.seatHeight ?? 0) > 0 && (anchor.seatHeight ?? 0) < 1, anchor.id);
		}
	}
});

test('resolves ids and labels, ignoring case and articles', () => {
	assert.equal(resolveSceneAnchor('chair')?.id, 'chair');
	assert.equal(resolveSceneAnchor('Chair')?.id, 'chair');
	assert.equal(resolveSceneAnchor(' the cushion ')?.id, 'cushion');
	assert.equal(resolveSceneAnchor('center')?.id, 'center');
});

test('unknown refs resolve to null, never a guess', () => {
	assert.equal(resolveSceneAnchor('mars'), null);
	assert.equal(resolveSceneAnchor(''), null);
	assert.equal(resolveSceneAnchor(null), null);
	assert.equal(resolveSceneAnchor(42), null);
});

test('listing returns copies and filters sittables', () => {
	const anchors = listSceneAnchors();
	assert.ok(anchors.length >= 3);
	anchors[0].x = 999;
	assert.notEqual(listSceneAnchors()[0].x, 999);
	const sittables = sittableAnchors();
	assert.ok(sittables.length >= 2);
	for (const anchor of sittables) assert.equal(anchor.sittable, true);
});
