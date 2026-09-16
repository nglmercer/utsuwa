import test from 'node:test';
import assert from 'node:assert/strict';

import {
	ExpressionController,
	type ExpressionEnvironment,
	type FaceExpressionManager,
	type FaceFrame,
	type VoiceFrame
} from './expression-controller.ts';
import type { PoseBone, PoseNudge } from './procedural-pose-controller.ts';

interface Harness {
	env: ExpressionEnvironment;
	writes: Array<{ name: string; value: number }>;
	updates: number;
	faces: Array<{ expression: string; intensity: number; durationMs: number }>;
	nudges: Array<{ x: number; z: number }>;
	bones: Map<string, PoseBone>;
	relationshipStage: 'stranger' | 'friend';
	talking: boolean;
	manager: FaceExpressionManager;
}

function makeBone(): PoseBone {
	return { rotation: { x: 0, y: 0, z: 0 } };
}

function makeHarness(): Harness {
	const harness: Harness = {
		env: null as unknown as ExpressionEnvironment,
		writes: [],
		updates: 0,
		faces: [],
		nudges: [],
		bones: new Map(),
		relationshipStage: 'friend',
		talking: false,
		manager: null as unknown as FaceExpressionManager
	};
	harness.manager = {
		setValue: (name, value) => {
			harness.writes.push({ name, value });
		},
		update: () => {
			harness.updates += 1;
		}
	};
	const nudge: PoseNudge = (bone, x, z, y = 0) => {
		if (!bone) return;
		harness.nudges.push({ x, z });
		bone.rotation.x += x;
		bone.rotation.z += z;
		bone.rotation.y += y;
	};
	harness.env = {
		getHumanoid: () => ({
			getNormalizedBoneNode: (name: string) => {
				let bone = harness.bones.get(name);
				if (!bone) {
					bone = makeBone();
					harness.bones.set(name, bone);
				}
				return bone;
			}
		}),
		nudge,
		getRelationshipStage: () => harness.relationshipStage,
		shouldTalk: () => harness.talking,
		requestFace: (expression, intensity, durationMs) => {
			harness.faces.push({ expression, intensity, durationMs });
		},
		getExpressionManager: () => harness.manager,
		getAvailableExpressionNames: () => ['happy', 'surprised', 'blink', 'aa']
	};
	return harness;
}

function faceFrame(harness: Harness, overrides: Partial<FaceFrame> = {}): FaceFrame {
	return {
		expressionManager: harness.manager,
		availableExpressions: ['happy', 'surprised', 'sad', 'neutral', 'relaxed'],
		mood: { primary: 'content', intensity: 60, causes: [] },
		photoActive: false,
		...overrides
	};
}

function voiceFrame(harness: Harness, overrides: Partial<VoiceFrame> = {}): VoiceFrame {
	return {
		expressionManager: harness.manager,
		emotePlaying: false,
		visemes: { aa: 0, ee: 0, ih: 0, oh: 0, ou: 0 },
		...overrides
	};
}

function lastWriteFor(harness: Harness, name: string): number | undefined {
	const found = [...harness.writes].reverse().find((write) => write.name === name);
	return found?.value;
}

test('transient faces rise and melt back into the mood', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	controller.stageExpression({ expression: 'surprised', intensity: 1, durationMs: 1000, seq: 1 });
	controller.updateFace(0.2, faceFrame(harness));
	const peak = lastWriteFor(harness, 'surprised');
	assert.ok(peak !== undefined && peak > 0.3, `peak ${peak}`);
	for (let i = 0; i < 60; i++) controller.updateFace(0.05, faceFrame(harness));
	const settled = lastWriteFor(harness, 'surprised');
	assert.ok(settled === undefined || settled < 0.01, `settled ${settled}`);
	// The mood face (content → relaxed) owns the frame again.
	assert.ok((lastWriteFor(harness, 'relaxed') ?? 0) > 0.1);
});

test('clearing mid-flight releases into the fade instead of snapping', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	controller.stageExpression({ expression: 'surprised', intensity: 1, durationMs: 2000, seq: 1 });
	controller.updateFace(0.1, faceFrame(harness));
	const before = lastWriteFor(harness, 'surprised') ?? 0;
	controller.stageExpression(null);
	controller.updateFace(0.05, faceFrame(harness));
	const after = lastWriteFor(harness, 'surprised') ?? 0;
	// Still visible (the release jumps into the envelope's sustain, never
	// to zero), then gone soon after.
	assert.ok(after > 0, `${before} -> ${after}`);
	for (let i = 0; i < 60; i++) controller.updateFace(0.05, faceFrame(harness));
	assert.ok((lastWriteFor(harness, 'surprised') ?? 0) < 0.01);
});

test('held photo expressions are exclusive and restored after borrowing', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	controller.setPhotoExpression(true, 'happy');
	assert.deepEqual(harness.writes.at(-1), { name: 'happy', value: 1 });
	// While the pose is held, the mood system stands down.
	controller.updateFace(0.1, faceFrame(harness, { photoActive: true }));
	// A reaction borrows the held expression, then hands it back whole.
	controller.stageExpression({ expression: 'happy', intensity: 0.8, durationMs: 300, seq: 2 });
	for (let i = 0; i < 20; i++) controller.updateFace(0.05, faceFrame(harness, { photoActive: true }));
	assert.deepEqual(lastWriteFor(harness, 'happy'), 1);
	// Changing the pose clears the old hold.
	controller.setPhotoExpression(true, 'surprised');
	assert.ok(harness.writes.some((write) => write.name === 'happy' && write.value === 0));
});

test('reactions flash a face and kick a decaying pulse', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	controller.stageReaction('head');
	assert.equal(harness.faces.length, 1);
	assert.equal(harness.faces[0].durationMs, 1800);
	const head = harness.bones.get('head');
	assert.ok(head, 'head bone resolved');
	controller.updatePulses(0.1);
	assert.ok(harness.nudges.length > 0, 'pulse applied');
	const kicked = head.rotation.z;
	assert.notEqual(kicked, 0);
	for (let i = 0; i < 40; i++) controller.updatePulses(0.05);
	const nudgesAfterSettle = harness.nudges.length;
	controller.updatePulses(0.05);
	assert.equal(harness.nudges.length, nudgesAfterSettle, 'spent pulses stop writing');
});

test('repeat taps escalate and pulses cap at four', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	for (let i = 0; i < 6; i++) controller.stageReaction('shoulder');
	controller.updatePulses(0.05);
	// Four pulses per frame at most, no matter how many taps landed.
	assert.ok(harness.nudges.length <= 4, `${harness.nudges.length} nudges`);
});

test('reactions are inert without a humanoid', () => {
	const harness = makeHarness();
	harness.env.getHumanoid = () => null;
	const controller = new ExpressionController(harness.env);
	controller.stageReaction('head');
	controller.updatePulses(0.1);
	assert.deepEqual(harness.nudges, []);
	assert.deepEqual(harness.faces, []);
});

test('blink runs on its timer and pauses for emotes', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	// Fast-forward to the first blink window.
	for (let i = 0; i < 400; i++) controller.updateBlinkAndVoice(0.05, voiceFrame(harness));
	const blinkWrites = harness.writes.filter((write) => write.name === 'blink');
	assert.ok(blinkWrites.length > 2, 'blink curve wrote frames');
	assert.ok(blinkWrites.some((write) => write.value > 0.5), 'blink closed');
	assert.equal(blinkWrites.at(-1)?.value, 0, 'blink ends open');
	// While an emote plays, the timer freezes: no new blinks start.
	const countBefore = harness.writes.length;
	for (let i = 0; i < 400; i++) {
		controller.updateBlinkAndVoice(0.05, voiceFrame(harness, { emotePlaying: true }));
	}
	const newBlinks = harness.writes.slice(countBefore).filter((write) => write.name === 'blink');
	assert.deepEqual(newBlinks, [], 'no blinks while emoting');
});

test('visemes apply across VRM 1.0, 0.x, and ARKit names', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	controller.updateBlinkAndVoice(
		0.016,
		voiceFrame(harness, { visemes: { aa: 0.9, ee: 0.1, ih: 0.2, oh: 0.3, ou: 0.4 } })
	);
	assert.equal(lastWriteFor(harness, 'aa'), 0.9);
	assert.equal(lastWriteFor(harness, 'a'), 0.9);
	assert.equal(lastWriteFor(harness, 'e'), 0.1);
	assert.equal(lastWriteFor(harness, 'jawOpen'), 0.9 * 0.7);
	assert.equal(harness.updates, 1, 'manager update runs between blink and visemes');
});

test('reset drops pulses, transient faces, and holds', () => {
	const harness = makeHarness();
	const controller = new ExpressionController(harness.env);
	controller.stageReaction('torso');
	controller.stageExpression({ expression: 'sad', intensity: 1, durationMs: 5000, seq: 1 });
	controller.setPhotoExpression(true, 'happy');
	controller.reset();
	controller.updatePulses(0.1);
	assert.deepEqual(harness.nudges, []);
	const writesBefore = harness.writes.length;
	controller.updateFace(0.1, faceFrame(harness));
	const sadWrites = harness.writes.slice(writesBefore).filter((write) => write.name === 'sad');
	assert.ok(sadWrites.every((write) => write.value < 0.05), 'no transient sad after reset');
});
