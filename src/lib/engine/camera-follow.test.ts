import test from 'node:test';
import assert from 'node:assert/strict';

import { followPushDelta, FOLLOW_DEADZONE_M } from './camera-follow.ts';

test('follow holds still while she is inside the deadzone', () => {
	assert.deepEqual(followPushDelta(0.1, -0.2, 1 / 60), { x: 0, z: 0 });
	assert.deepEqual(followPushDelta(FOLLOW_DEADZONE_M, 0, 1 / 60), { x: 0, z: 0 });
});

test('follow chases only the excess past the deadzone', () => {
	const push = followPushDelta(1, 0, 1);
	assert.ok(push.x > 0 && push.x < 1);
	assert.equal(push.z, 0);
	// A full-strength frame lands her exactly on the deadzone rim.
	assert.ok(Math.abs(1 - push.x - FOLLOW_DEADZONE_M) < 1e-9);
});

test('follow eases over several frames instead of snapping', () => {
	let offset = 1;
	for (let frame = 0; frame < 30; frame++) {
		offset -= followPushDelta(offset, 0, 1 / 60).x;
	}
	assert.ok(offset > FOLLOW_DEADZONE_M);
	assert.ok(offset < 1);
});

test('follow is NaN-safe and never pushes on a dead frame', () => {
	assert.deepEqual(followPushDelta(NaN, 0, 1 / 60), { x: 0, z: 0 });
	assert.deepEqual(followPushDelta(1, 0, 0), { x: 0, z: 0 });
	assert.deepEqual(followPushDelta(1, 0, -1), { x: 0, z: 0 });
});
