import test from 'node:test';
import assert from 'node:assert/strict';

import {
	AVATAR_ACTIONS,
	AVATAR_ACTION_NAMES,
	actionForAnimationUrl,
	aiAllowedActions,
	clampWalkOffset,
	expressionForAnimationUrl,
	isAvatarActionName,
	isLocomotionDirection,
	jumpArcHeight,
	resolveLegacyEmote
} from './avatar-actions.ts';

test('every action has sane metadata', () => {
	assert.ok(AVATAR_ACTION_NAMES.length >= 8);
	for (const name of AVATAR_ACTION_NAMES) {
		const def = AVATAR_ACTIONS[name];
		assert.equal(def.id, name);
		assert.ok(def.label.length > 0);
		assert.ok(def.description.length > 0);
		assert.ok(def.cooldownMs >= 1000, `${name} cooldown`);
		assert.ok(def.mode === 'oneshot' || def.mode === 'loop');
		if (def.source.kind === 'vrma') {
			assert.ok(def.source.url.startsWith('/animations/'), `${name} url`);
			assert.ok(def.source.url.endsWith('.vrma'), `${name} url`);
		}
		if (def.expression) {
			assert.ok(def.expression.intensity > 0 && def.expression.intensity <= 1);
		}
	}
});

test('large gestures require an explicit request', () => {
	for (const name of ['jump', 'dance', 'walk'] as const) {
		assert.equal(AVATAR_ACTIONS[name].explicitRequestOnly, true, name);
	}
	assert.equal(AVATAR_ACTIONS.wave.explicitRequestOnly, undefined);
});

test('prompt catalog only exposes AI-allowed actions', () => {
	const exposed = aiAllowedActions().map((def) => def.id);
	assert.ok(exposed.includes('wave') && exposed.includes('walk'));
	for (const def of aiAllowedActions()) {
		assert.equal(def.aiAllowed, true, def.id);
	}
});

test('name and direction guards accept only known values', () => {
	assert.ok(isAvatarActionName('wave'));
	assert.ok(isAvatarActionName('walk'));
	assert.ok(!isAvatarActionName('vrma_01'));
	assert.ok(!isAvatarActionName('/animations/evil.vrma'));
	assert.ok(!isAvatarActionName(null));
	assert.ok(isLocomotionDirection('left'));
	assert.ok(!isLocomotionDirection('up'));
	assert.ok(!isLocomotionDirection(null));
});

test('legacy emote ids resolve to shipped clips, never to model input', () => {
	assert.equal(resolveLegacyEmote('vrma_01'), '/animations/VRMA_01.vrma');
	assert.equal(resolveLegacyEmote('vrma_07'), '/animations/VRMA_07.vrma');
	assert.equal(resolveLegacyEmote('vrma_99'), null);
	assert.equal(resolveLegacyEmote('/animations/evil.vrma'), null);
});

test('jump arc starts and lands at zero with the apex at mid-flight', () => {
	assert.equal(jumpArcHeight(0, 0.28), 0);
	assert.equal(jumpArcHeight(1, 0.28), 0);
	assert.ok(Math.abs(jumpArcHeight(0.5, 0.28) - 0.28) < 1e-9);
	assert.ok(jumpArcHeight(0.25, 0.28) > 0);
	assert.equal(jumpArcHeight(-1, 0.28), 0);
	assert.equal(jumpArcHeight(2, 0.28), 0);
});

test('walk clamp keeps the avatar inside its radius', () => {
	assert.deepEqual(clampWalkOffset(0.5, 0.5, 2), { x: 0.5, z: 0.5 });
	const rim = clampWalkOffset(3, 4, 2);
	assert.ok(Math.abs(Math.hypot(rim.x, rim.z) - 2) < 1e-9);
	assert.ok(rim.x > 0 && rim.z > 0, 'direction preserved');
});

test('animation URLs reverse-map to their semantic face', () => {
	assert.equal(actionForAnimationUrl('/animations/VRMA_04.vrma')?.id, 'wave');
	assert.deepEqual(expressionForAnimationUrl('/animations/VRMA_04.vrma'), {
		expression: 'happy',
		intensity: 0.6
	});
	// Unmapped legacy clips get no face: mood stays in charge.
	assert.equal(actionForAnimationUrl('/animations/VRMA_01.vrma'), null);
	assert.equal(expressionForAnimationUrl('/animations/VRMA_01.vrma'), null);
});
