import test from 'node:test';
import assert from 'node:assert/strict';

import {
	WALK_DURATION_DEFAULT_MS,
	WALK_DURATION_MAX_MS,
	WALK_DURATION_MIN_MS,
	clampWalkDuration,
	isExplicitLocomotionAsk,
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
		'bowling night was great',
		'go on, tell me more',
		'move along with the story',
		'my face hurts today',
		'I understand how you feel',
		'go to mars right now'
	]) {
		assert.equal(parseAvatarCommand(text), null, text);
	}
	assert.equal(parseAvatarCommand(null as unknown as string), null);
});

test('parses run, move, and go-with-direction as locomotion', () => {
	assert.deepEqual(parseAvatarCommand('run left 2s')?.steps, [
		{ kind: 'walk', action: 'run', direction: 'left', durationMs: 2000 }
	]);
	assert.deepEqual(parseAvatarCommand('move right')?.steps, [
		{ kind: 'walk', action: 'walk', direction: 'right', durationMs: 1200 }
	]);
	assert.deepEqual(parseAvatarCommand('go to the left')?.steps, [
		{ kind: 'walk', action: 'walk', direction: 'left', durationMs: 1200 }
	]);
	// Bare-direction inheritance preserves run.
	const plan = parseAvatarCommand('run left then right');
	assert.deepEqual(
		plan?.steps.map((s) => s.action),
		['run', 'run']
	);
	assert.equal(plan?.steps[1].direction, 'right');
});

test('parses return-home, face-camera, and turns', () => {
	assert.deepEqual(parseAvatarCommand('come back!')?.steps, [
		{ kind: 'return_home', action: 'return_home' }
	]);
	assert.deepEqual(parseAvatarCommand('go home')?.steps, [
		{ kind: 'return_home', action: 'return_home' }
	]);
	assert.deepEqual(parseAvatarCommand('face me')?.steps, [
		{ kind: 'face_camera', action: 'face_camera' }
	]);
	assert.deepEqual(parseAvatarCommand('turn left')?.steps, [
		{ kind: 'turn', action: 'turn', direction: 'left' }
	]);
	assert.deepEqual(parseAvatarCommand('turn around')?.steps, [
		{ kind: 'turn', action: 'turn', direction: 'back' }
	]);
});

test('parses goto and sit-on-anchor sequences', () => {
	assert.deepEqual(parseAvatarCommand('go to the chair')?.steps, [
		{ kind: 'goto', action: 'goto', anchorId: 'chair' }
	]);
	assert.deepEqual(parseAvatarCommand('sit down')?.steps, [
		{ kind: 'procedural', action: 'sit' }
	]);
	assert.deepEqual(parseAvatarCommand('sit on the chair')?.steps, [
		{ kind: 'goto', action: 'goto', anchorId: 'chair' },
		{ kind: 'procedural', action: 'sit', anchorId: 'chair' }
	]);
	assert.deepEqual(parseAvatarCommand('stand up')?.steps, [
		{ kind: 'procedural', action: 'stand' }
	]);
});

test('parses front/back/up/down direction synonyms', () => {
	const forward = ['move front', 'walk to the front', 'go to the front', 'move closer', 'go down'];
	for (const text of forward) {
		assert.deepEqual(parseAvatarCommand(text)?.steps, [
			{ kind: 'walk', action: 'walk', direction: 'forward', durationMs: 1200 }
		], text);
	}
	const back = ['move to backside', 'walk behind', 'go behind', 'go up', 'move up', 'go farther'];
	for (const text of back) {
		assert.deepEqual(parseAvatarCommand(text)?.steps, [
			{ kind: 'walk', action: 'walk', direction: 'back', durationMs: 1200 }
		], text);
	}
});

test('parses approach phrasing and rejects unknown targets', () => {
	assert.deepEqual(parseAvatarCommand('walk up to me')?.steps, [
		{ kind: 'walk', action: 'walk', direction: 'forward', durationMs: 1200 }
	]);
	assert.deepEqual(parseAvatarCommand('go up to the chair')?.steps, [
		{ kind: 'goto', action: 'goto', anchorId: 'chair' }
	]);
	assert.equal(parseAvatarCommand('walk up to the window'), null);
	assert.equal(parseAvatarCommand("it's up to me"), null);
});

test('up/down idioms never steer her', () => {
	for (const text of ['give up', 'wake up', 'look up the word', 'calm down', 'lie down']) {
		assert.equal(parseAvatarCommand(text), null, text);
	}
	// Idiom after a real walk: the walk stands, the idiom stays conversational.
	const plan = parseAvatarCommand('walk left then calm down');
	assert.equal(plan?.steps.length, 1);
	assert.equal(plan?.pureAvatarCommand, false);
	const whatsUp = parseAvatarCommand("walk, what's up");
	assert.equal(whatsUp?.steps.length, 1);
	assert.equal(whatsUp?.steps[0].direction, 'forward');
});

test('isExplicitLocomotionAsk gates model walks on user asks', () => {
	for (const text of [
		'walk left',
		'walk',
		'move to backside',
		'go up',
		'walk to the front',
		'walk up to the window',
		'walk left and tell me a joke'
	]) {
		assert.equal(isExplicitLocomotionAsk(text), true, text);
	}
	for (const text of [
		'how are you?',
		'',
		'go on, tell me more',
		'move along with the story',
		"it's up to me",
		'give up',
		'calm down',
		'turn up the volume',
		'explain step by step',
		null as unknown as string
	]) {
		assert.equal(isExplicitLocomotionAsk(text), false, String(text));
	}
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
