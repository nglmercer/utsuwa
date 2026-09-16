import test from 'node:test';
import assert from 'node:assert/strict';

import {
	createGestureGateState,
	evaluateGestureGate,
	gestureKey,
	recordGestureExecution,
	type GestureGateContext,
	type GestureGateState
} from './avatar-action-gate.ts';
import type { GestureCue } from './avatar-actions.ts';

const wave: GestureCue = { type: 'animation', action: 'wave' };
const nod: GestureCue = { type: 'animation', action: 'nod' };
const jump: GestureCue = { type: 'animation', action: 'jump' };
const walkLeft: GestureCue = { type: 'locomotion', action: 'walk', direction: 'left', durationMs: 1200 };
const headTap: GestureCue = { type: 'reaction', zone: 'head' };

function ctxAt(
	cue: GestureCue,
	now: number,
	state: GestureGateState,
	overrides: Partial<GestureGateContext> = {}
): GestureGateContext {
	return { cue, now, state, motion: 'idle', busy: false, explicitRequest: false, ...overrides };
}

test('first gesture is allowed and recorded', () => {
	const state = createGestureGateState();
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 1000, state)), { allowed: true });
	recordGestureExecution(state, wave, 1000);
	assert.equal(state.lastKey, 'animation:wave');
});

test('same gesture twice in a row is a duplicate', () => {
	const state = createGestureGateState();
	recordGestureExecution(state, nod, 0);
	assert.deepEqual(evaluateGestureGate(ctxAt(nod, 5000, state)), {
		allowed: false,
		reason: 'duplicate'
	});
	// ...but allowed again after its window (nod: 8s registry, 15s floor).
	assert.deepEqual(evaluateGestureGate(ctxAt(nod, 16000, state)), { allowed: true });
});

test('different gesture inside the global window cools down, then passes', () => {
	const state = createGestureGateState();
	recordGestureExecution(state, wave, 0);
	assert.deepEqual(evaluateGestureGate(ctxAt(nod, 3000, state)), {
		allowed: false,
		reason: 'cooldown'
	});
	assert.deepEqual(evaluateGestureGate(ctxAt(nod, 7000, state)), { allowed: true });
});

test('rejections never extend cooldowns', () => {
	const state = createGestureGateState();
	recordGestureExecution(state, wave, 0);
	// Rejected duplicate at t=5s is NOT recorded...
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 5000, state)).allowed, false);
	// ...so a different gesture at t=7s passes on the original timestamp.
	assert.deepEqual(evaluateGestureGate(ctxAt(nod, 7000, state)), { allowed: true });
});

test('jump and walk need an explicit request', () => {
	const state = createGestureGateState();
	assert.deepEqual(evaluateGestureGate(ctxAt(jump, 1000, state)), {
		allowed: false,
		reason: 'not-explicit'
	});
	assert.deepEqual(evaluateGestureGate(ctxAt(walkLeft, 1000, state)), {
		allowed: false,
		reason: 'not-explicit'
	});
	assert.deepEqual(evaluateGestureGate(ctxAt(jump, 1000, state, { explicitRequest: true })), {
		allowed: true
	});
	assert.deepEqual(evaluateGestureGate(ctxAt(walkLeft, 1000, state, { explicitRequest: true })), {
		allowed: true
	});
});

test('photo mode suppresses everything, even explicit commands', () => {
	const state = createGestureGateState();
	for (const cue of [wave, jump, headTap]) {
		assert.deepEqual(evaluateGestureGate(ctxAt(cue, 1000, state, { motion: 'photo_mode' })), {
			allowed: false,
			reason: 'photo-mode'
		});
		assert.deepEqual(
			evaluateGestureGate(ctxAt(cue, 1000, state, { motion: 'photo_mode', explicitRequest: true })),
			{ allowed: false, reason: 'photo-mode' }
		);
	}
});

test('busy avatar drops conversational cues but yields to explicit ones', () => {
	const state = createGestureGateState();
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 20000, state, { busy: true })), {
		allowed: false,
		reason: 'busy'
	});
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 20000, state, { busy: true, explicitRequest: true })), {
		allowed: true
	});
});

test('explicit commands bypass cooldowns, duplicates, and rate limits', () => {
	const state = createGestureGateState();
	recordGestureExecution(state, wave, 0);
	// Same gesture immediately: conversational is a duplicate, explicit passes.
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 1000, state)).allowed, false);
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 1000, state, { explicitRequest: true })), {
		allowed: true
	});
	// Different gesture inside the global window: same split.
	assert.deepEqual(evaluateGestureGate(ctxAt(nod, 1000, state)).allowed, false);
	assert.deepEqual(evaluateGestureGate(ctxAt(nod, 1000, state, { explicitRequest: true })), {
		allowed: true
	});
	// Photo mode still rejects explicit commands.
	assert.deepEqual(
		evaluateGestureGate(ctxAt(wave, 1000, state, { motion: 'photo_mode', explicitRequest: true })),
		{ allowed: false, reason: 'photo-mode' }
	);
});

test('rate limit caps automatic gestures per minute', () => {
	const state = createGestureGateState();
	// Four well-spaced distinct gestures fill the window.
	const cues: GestureCue[] = [wave, nod, headTap, { type: 'animation', action: 'bow' }];
	cues.forEach((cue, i) => recordGestureExecution(state, cue, i * 8000));
	assert.deepEqual(evaluateGestureGate(ctxAt({ type: 'animation', action: 'shrug' }, 40000, state)), {
		allowed: false,
		reason: 'rate-limit'
	});
	// Explicit commands are exempt from the rate limit...
	assert.deepEqual(
		evaluateGestureGate(
			ctxAt({ type: 'animation', action: 'shrug' }, 40000, state, { explicitRequest: true })
		),
		{ allowed: true }
	);
	// ...and the window slides: a minute later the budget is back.
	assert.deepEqual(evaluateGestureGate(ctxAt({ type: 'animation', action: 'shrug' }, 90000, state)), {
		allowed: true
	});
});

test('thinking motion neither blocks nor auto-generates gestures', () => {
	const state = createGestureGateState();
	// The gate evaluates cues the same under thinking: no auto-allow, no block.
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 1000, state, { motion: 'thinking' })), {
		allowed: true
	});
	// Generation stays absent: nothing in the pipeline constructs a cue from
	// thinking/tool states (no cue source exists outside parsed model output
	// and explicit user commands).
	assert.deepEqual(evaluateGestureGate(ctxAt(wave, 1000, state, { motion: 'tool_running' })), {
		allowed: true
	});
});

test('gesture keys are stable per cue shape', () => {
	assert.equal(gestureKey(wave), 'animation:wave');
	assert.equal(gestureKey(walkLeft), 'locomotion:walk:left');
	assert.equal(gestureKey(headTap), 'reaction:head');
	assert.equal(gestureKey({ type: 'emote', id: 'vrma_03' }), 'emote:vrma_03');
});

test('parameterized cues key by their parameter', () => {
	assert.equal(
		gestureKey({ type: 'animation', action: 'turn', direction: 'left' }),
		'animation:turn:left'
	);
	assert.equal(
		gestureKey({ type: 'animation', action: 'goto', anchorId: 'chair' }),
		'animation:goto:chair'
	);
	assert.equal(
		gestureKey({ type: 'locomotion', action: 'run', direction: 'left', durationMs: 1200 }),
		'locomotion:run:left'
	);
	// Same turn twice is a duplicate; a different direction is not.
	const state = createGestureGateState();
	const left: GestureCue = { type: 'animation', action: 'turn', direction: 'left' };
	const right: GestureCue = { type: 'animation', action: 'turn', direction: 'right' };
	recordGestureExecution(state, left, 0);
	assert.equal(evaluateGestureGate(ctxAt(left, 5000, state, { explicitRequest: true })).allowed, true);
	assert.equal(state.lastKey, 'animation:turn:left');
	assert.deepEqual(evaluateGestureGate(ctxAt(right, 20000, state, { explicitRequest: true })), {
		allowed: true
	});
});
