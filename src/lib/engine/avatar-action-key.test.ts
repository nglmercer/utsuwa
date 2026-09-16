import test from 'node:test';
import assert from 'node:assert/strict';

import {
	identityFromGestureCue,
	identityFromRoutineStep,
	routineStepGestureKey,
	routineStepReceiptKey,
	serializeAvatarIdentity,
	serializeRoutineIdentity,
	gestureCueKey
} from './avatar-action-key.ts';
import type { GestureCue, AvatarRoutineStep } from './avatar-actions.ts';

test('cue identities serialize to the historical gesture keys', () => {
	const cases: Array<[GestureCue, string]> = [
		[{ type: 'animation', action: 'wave' }, 'animation:wave'],
		[{ type: 'animation', action: 'nod' }, 'animation:nod'],
		[{ type: 'animation', action: 'turn', direction: 'left' }, 'animation:turn:left'],
		[{ type: 'animation', action: 'turn', direction: 'back' }, 'animation:turn:back'],
		[{ type: 'animation', action: 'goto', anchorId: 'chair' }, 'animation:goto:chair'],
		[{ type: 'animation', action: 'sit', anchorId: 'cushion' }, 'animation:sit:cushion'],
		[{ type: 'animation', action: 'sit' }, 'animation:sit'],
		[
			{ type: 'locomotion', action: 'walk', direction: 'left', durationMs: 1200 },
			'locomotion:walk:left'
		],
		[
			{ type: 'locomotion', action: 'run', direction: 'forward', durationMs: 900 },
			'locomotion:run:forward'
		],
		[{ type: 'reaction', zone: 'head' }, 'reaction:head'],
		[{ type: 'emote', id: 'vrma_01' }, 'emote:vrma_01']
	];
	for (const [cue, expected] of cases) {
		assert.equal(gestureCueKey(cue), expected, JSON.stringify(cue));
		assert.equal(
			serializeAvatarIdentity(identityFromGestureCue(cue)),
			expected,
			`identity ${JSON.stringify(cue)}`
		);
	}
});

test('matching cues and steps share one semantic identity', () => {
	const pairs: Array<[GestureCue, AvatarRoutineStep]> = [
		[
			{ type: 'animation', action: 'turn', direction: 'left' },
			{ kind: 'turn', action: 'turn', direction: 'left' }
		],
		[
			{ type: 'animation', action: 'goto', anchorId: 'chair' },
			{ kind: 'goto', action: 'goto', anchorId: 'chair' }
		],
		[
			{ type: 'locomotion', action: 'walk', direction: 'back', durationMs: 500 },
			{ kind: 'walk', action: 'walk', direction: 'back', durationMs: 500 }
		],
		[{ type: 'animation', action: 'jump' }, { kind: 'jump', action: 'jump' }],
		[{ type: 'animation', action: 'wave' }, { kind: 'emote', action: 'wave', url: '/animations/VRMA_04.vrma' }]
	];
	for (const [cue, step] of pairs) {
		assert.equal(
			gestureCueKey(cue),
			routineStepGestureKey(step),
			`${JSON.stringify(cue)} vs ${JSON.stringify(step)}`
		);
	}
});

test('step correlation keeps the historical bare-step defaults', () => {
	assert.equal(routineStepGestureKey({ kind: 'walk', action: 'walk' }), 'locomotion:walk:forward');
	assert.equal(routineStepGestureKey({ kind: 'walk', action: 'run' }), 'locomotion:run:forward');
	assert.equal(routineStepGestureKey({ kind: 'turn', action: 'turn' }), 'animation:turn:back');
	assert.equal(routineStepGestureKey({ kind: 'goto', action: 'goto' }), 'animation:goto:');
});

test('walk and run stay distinct, turn directions stay distinct', () => {
	assert.notEqual(
		routineStepGestureKey({ kind: 'walk', action: 'walk', direction: 'left' }),
		routineStepGestureKey({ kind: 'walk', action: 'run', direction: 'left' })
	);
	assert.notEqual(
		gestureCueKey({ type: 'animation', action: 'turn', direction: 'left' }),
		gestureCueKey({ type: 'animation', action: 'turn', direction: 'right' })
	);
});

test('receipt keys keep the pinned verification format', () => {
	assert.equal(
		routineStepReceiptKey({ kind: 'walk', action: 'walk', direction: 'left' }),
		'walk:walk:left'
	);
	assert.equal(routineStepReceiptKey({ kind: 'emote', action: 'wave' }), 'emote:wave');
	assert.equal(routineStepReceiptKey({ kind: 'jump', action: 'jump' }), 'jump:jump');
	assert.equal(
		routineStepReceiptKey({ kind: 'walk', action: 'run', direction: 'left' }),
		'walk:run:left'
	);
	assert.equal(
		routineStepReceiptKey({ kind: 'turn', action: 'turn', direction: 'back' }),
		'turn:turn:back'
	);
	assert.equal(
		routineStepReceiptKey({ kind: 'return_home', action: 'return_home' }),
		'return_home:return_home'
	);
	assert.equal(
		routineStepReceiptKey({ kind: 'goto', action: 'goto', anchorId: 'chair' }),
		'goto:goto:chair'
	);
	assert.equal(
		routineStepReceiptKey({ kind: 'goto', action: 'goto', x: 0.9, z: 0.35 }),
		'goto:goto:0.90,0.35'
	);
});

test('receipt keys never gain correlation defaults', () => {
	assert.equal(routineStepReceiptKey({ kind: 'turn', action: 'turn' }), 'turn:turn');
	assert.equal(routineStepReceiptKey({ kind: 'walk', action: 'walk' }), 'walk:walk');
	assert.equal(routineStepReceiptKey({ kind: 'goto', action: 'goto' }), 'goto:goto');
});

test('identity round-trips through the serializer struct', () => {
	const identity = identityFromGestureCue({
		type: 'animation',
		action: 'goto',
		anchorId: 'cushion'
	});
	assert.deepEqual(identity, { action: 'goto', channel: 'animation', anchorId: 'cushion' });
	assert.equal(serializeAvatarIdentity({ action: 'x', legacyId: 'vrma_02' }), 'emote:vrma_02');
	assert.equal(serializeAvatarIdentity({ action: 'x', reactionZone: 'hip' }), 'reaction:hip');
	const receipt = serializeRoutineIdentity(identityFromRoutineStep({
		kind: 'procedural',
		action: 'sit',
		anchorId: 'cushion'
	}));
	assert.equal(receipt, 'procedural:sit:cushion');
});
