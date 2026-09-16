import test from 'node:test';
import assert from 'node:assert/strict';

import {
	AVATAR_ACTIONS,
	AVATAR_ACTION_NAMES,
	actionForAnimationUrl,
	aiAllowedActions,
	clampWalkOffset,
	computeAvatarSpatial,
	computeSitOffsetY,
	expressionForAnimationUrl,
	isAvatarActionName,
	isLocomotionActionName,
	isLocomotionDirection,
	isTurnDirection,
	jumpArcHeight,
	resolveLegacyEmote,
	shortAngleDelta,
	turnTargetYaw,
	yawToFacePoint
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
	for (const name of [
		'jump',
		'dance',
		'walk',
		'run',
		'return_home',
		'face_camera',
		'turn',
		'goto',
		'sit',
		'stand'
	] as const) {
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
	assert.ok(isAvatarActionName('run'));
	assert.ok(isAvatarActionName('return_home'));
	assert.ok(isAvatarActionName('goto'));
	assert.ok(!isAvatarActionName('vrma_01'));
	assert.ok(!isAvatarActionName('/animations/evil.vrma'));
	assert.ok(!isAvatarActionName(null));
	assert.ok(isLocomotionDirection('left'));
	assert.ok(!isLocomotionDirection('up'));
	assert.ok(!isLocomotionDirection(null));
	assert.ok(isLocomotionActionName('walk'));
	assert.ok(isLocomotionActionName('run'));
	assert.ok(!isLocomotionActionName('jump'));
	assert.ok(isTurnDirection('left'));
	assert.ok(isTurnDirection('back'));
	assert.ok(!isTurnDirection('forward'));
	assert.ok(!isTurnDirection('up'));
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

test('yaw helpers agree on facing and turns', () => {
	// Avatar at origin, camera at +Z: facing yaw is 0.
	assert.ok(Math.abs(yawToFacePoint(0, 0, 0, 2)) < 1e-9);
	// Camera to screen-right (+X): yaw +90°.
	assert.ok(Math.abs(yawToFacePoint(0, 0, 2, 0) - Math.PI / 2) < 1e-9);
	assert.ok(Math.abs(shortAngleDelta(Math.PI * 3) - Math.PI) < 1e-9);
	assert.ok(Math.abs(shortAngleDelta(-Math.PI * 3) + Math.PI) < 1e-9);
	assert.ok(Math.abs(turnTargetYaw(0, 'left') - Math.PI / 2) < 1e-9);
	assert.ok(Math.abs(turnTargetYaw(0, 'right') + Math.PI / 2) < 1e-9);
	assert.ok(Math.abs(shortAngleDelta(turnTargetYaw(0.5, 'back') - 0.5 - Math.PI)) < 1e-9);
});

test('spatial snapshot reports rim and facing error', () => {
	const centered = computeAvatarSpatial(0, 0, 0, 0, 2);
	assert.equal(centered.distFromHome, 0);
	assert.equal(centered.atRim, false);
	assert.ok(centered.facingErrorDeg < 1);
	const rim = computeAvatarSpatial(2, 0, 0, 0, 2);
	assert.equal(rim.atRim, true);
	// Facing +Z while the viewer stands at −Z: 180° away.
	const away = computeAvatarSpatial(0, 0, 0, 0, -2);
	assert.ok(Math.abs(away.facingErrorDeg - 180) < 1e-6);
	// Garbage never poisons the prompt.
	const nan = computeAvatarSpatial(NaN, NaN, NaN, NaN, NaN);
	assert.equal(nan.distFromHome, 0);
	assert.equal(nan.atRim, false);
});

test('sit offset drops the hips to seat height, clamped', () => {
	assert.ok(Math.abs(computeSitOffsetY(0.9, 0.45) + 0.45) < 1e-9);
	assert.equal(computeSitOffsetY(0.9, 1.5), 0);
	assert.equal(computeSitOffsetY(1.5, 0.1), -0.8);
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
