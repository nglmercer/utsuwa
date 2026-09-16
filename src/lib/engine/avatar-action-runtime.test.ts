import test from 'node:test';
import assert from 'node:assert/strict';

import {
	actionToRoutineStep,
	cueToRoutineStep,
	routineStepToAvatarRequest
} from './avatar-action-runtime.ts';
import { AVATAR_ACTIONS } from './avatar-actions.ts';

test('registry actions convert to the correct routine step', () => {
	assert.deepEqual(actionToRoutineStep('nod'), { kind: 'procedural', action: 'nod' });
	assert.deepEqual(actionToRoutineStep('jump'), { kind: 'jump', action: 'jump' });
	assert.deepEqual(actionToRoutineStep('return_home'), {
		kind: 'return_home',
		action: 'return_home'
	});
	assert.deepEqual(actionToRoutineStep('face_camera'), {
		kind: 'face_camera',
		action: 'face_camera'
	});
	assert.deepEqual(actionToRoutineStep('turn', { direction: 'left' }), {
		kind: 'turn',
		action: 'turn',
		direction: 'left'
	});
	assert.deepEqual(actionToRoutineStep('turn'), {
		kind: 'turn',
		action: 'turn',
		direction: 'back'
	});
	assert.deepEqual(actionToRoutineStep('sit', { anchorId: 'chair' }), {
		kind: 'procedural',
		action: 'sit',
		anchorId: 'chair'
	});
});

test('registry VRMA actions carry their clip URL', () => {
	for (const [action, url] of [
		['wave', '/animations/VRMA_04.vrma'],
		['celebrate', '/animations/VRMA_07.vrma'],
		['dance', '/animations/VRMA_05.vrma']
	] as const) {
		assert.deepEqual(actionToRoutineStep(action), { kind: 'emote', action, url });
		assert.equal(AVATAR_ACTIONS[action].execution.kind, 'vrma');
	}
});

test('walk/run default to a forward default-duration step', () => {
	assert.deepEqual(actionToRoutineStep('walk'), {
		kind: 'walk',
		action: 'walk',
		direction: 'forward',
		durationMs: 1200
	});
	assert.deepEqual(actionToRoutineStep('run', { direction: 'back', durationMs: 500 }), {
		kind: 'walk',
		action: 'run',
		direction: 'back',
		durationMs: 500
	});
	// Out-of-range durations clamp to the shared window.
	assert.deepEqual(actionToRoutineStep('walk', { direction: 'left', durationMs: 99999 }), {
		kind: 'walk',
		action: 'walk',
		direction: 'left',
		durationMs: 3000
	});
});

test('goto needs a target, coordinate gotos pass through', () => {
	assert.equal(actionToRoutineStep('goto'), null);
	assert.deepEqual(actionToRoutineStep('goto', { anchorId: 'chair' }), {
		kind: 'goto',
		action: 'goto',
		anchorId: 'chair'
	});
	assert.deepEqual(actionToRoutineStep('goto', { x: 0.9, z: 0.35 }), {
		kind: 'goto',
		action: 'goto',
		x: 0.9,
		z: 0.35
	});
});

test('cues convert to routine steps, reactions and emotes do not', () => {
	assert.deepEqual(cueToRoutineStep({ type: 'animation', action: 'nod' }), {
		kind: 'procedural',
		action: 'nod'
	});
	assert.deepEqual(
		cueToRoutineStep({ type: 'animation', action: 'turn', direction: 'right' }),
		{ kind: 'turn', action: 'turn', direction: 'right' }
	);
	assert.deepEqual(
		cueToRoutineStep({ type: 'animation', action: 'goto', anchorId: 'cushion' }),
		{ kind: 'goto', action: 'goto', anchorId: 'cushion' }
	);
	assert.equal(cueToRoutineStep({ type: 'animation', action: 'goto' }), null);
	assert.deepEqual(
		cueToRoutineStep({ type: 'locomotion', action: 'run', direction: 'left', durationMs: 800 }),
		{ kind: 'walk', action: 'run', direction: 'left', durationMs: 800 }
	);
	assert.equal(cueToRoutineStep({ type: 'reaction', zone: 'head' }), null);
	assert.equal(cueToRoutineStep({ type: 'emote', id: 'vrma_01' }), null);
});

test('routine steps convert to renderer invocations', () => {
	assert.deepEqual(routineStepToAvatarRequest({ kind: 'jump', action: 'jump' }), {
		kind: 'jump',
		action: 'jump'
	});
	assert.deepEqual(
		routineStepToAvatarRequest({ kind: 'procedural', action: 'sit', anchorId: 'chair' }),
		{ kind: 'procedural', action: 'sit', anchorId: 'chair' }
	);
	assert.deepEqual(
		routineStepToAvatarRequest({ kind: 'walk', action: 'walk', direction: 'back', durationMs: 700 }),
		{ kind: 'walk', action: 'walk', direction: 'back', durationMs: 700 }
	);
	assert.deepEqual(
		routineStepToAvatarRequest({ kind: 'goto', action: 'goto', anchorId: 'chair' }),
		{ kind: 'goto', action: 'goto', anchorId: 'chair' }
	);
	// Emote steps and targetless gotos have no action request.
	assert.equal(
		routineStepToAvatarRequest({ kind: 'emote', action: 'wave', url: '/animations/VRMA_04.vrma' }),
		null
	);
	assert.equal(routineStepToAvatarRequest({ kind: 'goto', action: 'goto' }), null);
});
