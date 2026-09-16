import test from 'node:test';
import assert from 'node:assert/strict';

import {
	WALK_DURATION_DEFAULT_MS,
	WALK_DURATION_MAX_MS,
	WALK_DURATION_MIN_MS,
	clampWalkDuration,
	parseAvatarCommand
} from './avatar-commands.ts';

test('parses single-step avatar commands', () => {
	assert.deepEqual(parseAvatarCommand('Jump!'), {
		steps: [{ kind: 'jump', action: 'jump' }],
		pureAvatarCommand: true
	});
	assert.deepEqual(parseAvatarCommand('can you jump?')?.steps, [{ kind: 'jump', action: 'jump' }]);
	assert.deepEqual(parseAvatarCommand('Wave at me')?.steps, [
		{ kind: 'emote', action: 'wave', url: '/animations/VRMA_04.vrma' }
	]);
	assert.deepEqual(parseAvatarCommand('nod if you agree')?.steps, [
		{ kind: 'procedural', action: 'nod' }
	]);
	assert.deepEqual(parseAvatarCommand('shake your head')?.steps, [
		{ kind: 'procedural', action: 'shake_head' }
	]);
	assert.deepEqual(parseAvatarCommand('take a bow')?.steps, [
		{ kind: 'procedural', action: 'bow' }
	]);
});

test('parses multi-step walk sequences with durations', () => {
	const plan = parseAvatarCommand(
		'walk left for 3 seconds, then right for 3 seconds, then forward for 3 seconds, then back for 3 seconds'
	);
	assert.deepEqual(plan, {
		steps: [
			{ kind: 'walk', action: 'walk', direction: 'left', durationMs: 3000 },
			{ kind: 'walk', action: 'walk', direction: 'right', durationMs: 3000 },
			{ kind: 'walk', action: 'walk', direction: 'forward', durationMs: 3000 },
			{ kind: 'walk', action: 'walk', direction: 'back', durationMs: 3000 }
		],
		pureAvatarCommand: true
	});
});

test('supports separators, mixed durations, and jump-then-walk', () => {
	const plan = parseAvatarCommand('jump then walk left 2s and nod');
	assert.ok(plan?.pureAvatarCommand);
	assert.deepEqual(
		plan?.steps.map((s) => s.action),
		['jump', 'walk', 'nod']
	);
	assert.equal(plan?.steps[1].durationMs, 2000);
	const dotted = parseAvatarCommand('Bow. Wave.');
	assert.equal(dotted?.steps.length, 2);
});

test('clamps walk durations and defaults bare walks', () => {
	assert.equal(parseAvatarCommand('walk left 99 seconds')?.steps[0].durationMs, 3000);
	assert.equal(parseAvatarCommand('walk left 0 seconds')?.steps[0].durationMs, 300);
	assert.deepEqual(parseAvatarCommand('walk')?.steps, [
		{ kind: 'walk', action: 'walk', direction: 'forward', durationMs: 1200 }
	]);
});

test('detects mixed avatar + conversation requests', () => {
	const plan = parseAvatarCommand('walk left for 2 seconds and tell me a joke');
	assert.ok(plan);
	assert.equal(plan.pureAvatarCommand, false);
	assert.equal(plan.steps.length, 1);
	assert.ok(plan.remainingText?.includes('tell me a joke'), plan.remainingText);
});

test('polite filler stays pure', () => {
	assert.equal(parseAvatarCommand('jump, please')?.pureAvatarCommand, true);
	assert.equal(parseAvatarCommand('could you nod?')?.pureAvatarCommand, true);
});

test('parses Japanese commands', () => {
	assert.deepEqual(parseAvatarCommand('ジャンプして！')?.steps, [{ kind: 'jump', action: 'jump' }]);
	assert.deepEqual(parseAvatarCommand('左に歩いて')?.steps, [
		{ kind: 'walk', action: 'walk', direction: 'left', durationMs: 1200 }
	]);
	assert.deepEqual(parseAvatarCommand('踊って')?.steps[0].action, 'dance');
});

test('ignores ordinary chat and near-miss words', () => {
	for (const text of [
		'How was your day?',
		'The jumper cables are in the car',
		'She is a beautiful dancer',
		'the microwave beeped',
		'I walked to school',
		'',
		'bowling night was great'
	]) {
		assert.equal(parseAvatarCommand(text), null, text);
	}
	assert.equal(parseAvatarCommand(null as unknown as string), null);
});

test('clampWalkDuration locks renderer-safe bounds', () => {
	assert.equal(clampWalkDuration(1200), 1200);
	assert.equal(clampWalkDuration(50), WALK_DURATION_MIN_MS);
	assert.equal(clampWalkDuration(99999), WALK_DURATION_MAX_MS);
	// Non-finite durations must fall back, never propagate: NaN remainingMs
	// freezes a walk step (never <= 0) AND defeats the watchdog (NaN compare).
	assert.equal(clampWalkDuration(NaN), WALK_DURATION_DEFAULT_MS);
	assert.equal(clampWalkDuration(Infinity), WALK_DURATION_DEFAULT_MS);
	assert.equal(clampWalkDuration(-Infinity), WALK_DURATION_DEFAULT_MS);
});
