import test from 'node:test';
import assert from 'node:assert/strict';

import {
	aiAllowedActions,
	LOCOMOTION_DIRECTIONS,
	TURN_DIRECTIONS,
	WALK_DURATION_MAX_MS,
	WALK_DURATION_MIN_MS
} from '../engine/avatar-actions.ts';
import { DEFAULT_SCENE_ANCHORS } from '../engine/scene-anchors.ts';
import { TOUCH_ZONES } from '../engine/photo-reactions.ts';
import { CAMERA_LIMITS } from '../stores/display-types.ts';
import {
	buildAvatarCatalogExtras,
	buildAvatarOutputContract,
	buildCameraOutputContract,
	buildCameraRules,
	buildCompletionRules,
	buildExpressionOutputContract,
	buildGestureRules,
	oxfordJoin
} from './prompt-contract.ts';

test('avatar contract reflects every AI-allowed action', () => {
	const contract = buildAvatarOutputContract();
	for (const def of aiAllowedActions()) {
		assert.ok(contract.includes(def.id), `contract mentions ${def.id}`);
	}
});

test('avatar contract reflects all directions, anchors, and walk bounds', () => {
	const contract = buildAvatarOutputContract();
	for (const direction of LOCOMOTION_DIRECTIONS) {
		assert.ok(contract.includes(direction), direction);
	}
	for (const direction of TURN_DIRECTIONS) {
		assert.ok(contract.includes(direction), direction);
	}
	for (const anchor of DEFAULT_SCENE_ANCHORS) {
		assert.ok(contract.includes(anchor.id), anchor.id);
	}
	for (const zone of TOUCH_ZONES) {
		assert.ok(contract.includes(zone), zone);
	}
	assert.ok(contract.includes(`${WALK_DURATION_MIN_MS} to ${WALK_DURATION_MAX_MS}`));
});

test('avatar contract tracks custom anchors', () => {
	const contract = buildAvatarOutputContract([
		{ id: 'stage', label: 'stage', x: 0, z: 0 },
		{ id: 'window', label: 'window', x: 1, z: 1 }
	]);
	assert.ok(contract.includes('"anchor_id": "stage|window (goto/sit only)"'));
});

test('camera contract reflects the slider limits', () => {
	const contract = buildCameraOutputContract();
	assert.ok(contract.includes(`${CAMERA_LIMITS.zoom.min} to ${CAMERA_LIMITS.zoom.max}`));
	assert.ok(contract.includes(`${CAMERA_LIMITS.height.min} to ${CAMERA_LIMITS.height.max}`));
	assert.ok(contract.includes(`${CAMERA_LIMITS.fov.min} to ${CAMERA_LIMITS.fov.max}`));
});

test('catalog extras render every parameterized move with generated lists', () => {
	const extras = buildAvatarCatalogExtras();
	const text = extras.join('\n');
	assert.ok(text.includes('- walk: only when the user asks you to move (direction '));
	assert.ok(text.includes('- run: only when the user asks you to run or hurry (same directions, faster)'));
	assert.ok(text.includes('- turn: turn in place (direction '));
	assert.ok(text.includes('- goto: go to a named place (anchor_id '));
	assert.ok(text.includes('- reaction: flinch toward being touched (zone '));
	for (const direction of LOCOMOTION_DIRECTIONS) {
		assert.ok(text.includes(direction), direction);
	}
});

test('expression contract matches the parser stage window', () => {
	const contract = buildExpressionOutputContract();
	assert.ok(contract.includes('"expression_cue"'));
	assert.ok(contract.includes('happy|angry|sad|relaxed|surprised|neutral'));
	assert.ok(contract.includes('500 to 6000'));
});

test('shared rules are stable single-source paragraphs', () => {
	assert.ok(buildGestureRules().startsWith('BODY GESTURE RULES:'));
	assert.ok(buildCameraRules().startsWith('CAMERA RULES:'));
	assert.ok(buildCompletionRules().startsWith('COMPLETION HONESTY:'));
});

test('oxford join formats catalog lists', () => {
	assert.equal(oxfordJoin([]), '');
	assert.equal(oxfordJoin(['a']), 'a');
	assert.equal(oxfordJoin(['a', 'b']), 'a or b');
	assert.equal(oxfordJoin(['a', 'b', 'c']), 'a, b, or c');
	assert.equal(oxfordJoin(['left', 'right', 'forward', 'back']), 'left, right, forward, or back');
});
