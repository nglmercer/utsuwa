import test from 'node:test';
import assert from 'node:assert/strict';

import {
	MotionController,
	resolveGotoTarget,
	type MotionCompletion,
	type MotionEnvironment,
	type MotionRoot
} from './motion-controller.ts';
import type { PoseHumanoid, PoseNudge } from './procedural-pose-controller.ts';
import { VRM0_MOTION_BASIS } from './humanoid-motion-basis.ts';

interface Bone {
	rotation: { x: number; y: number; z: number };
}

function makeRoot(): MotionRoot & { position: { x: number; y: number; z: number } } {
	return { position: { x: 0, y: 0, z: 0 }, yaw: 0 };
}

function makeHumanoid(): PoseHumanoid & { bones: Map<string, Bone> } {
	const bones = new Map<string, Bone>();
	return {
		bones,
		getNormalizedBoneNode: (name: string) => {
			let bone = bones.get(name);
			if (!bone) {
				bone = { rotation: { x: 0, y: 0, z: 0 } };
				bones.set(name, bone);
			}
			return bone;
		}
	};
}

function makeEnv(overrides: Partial<MotionEnvironment> = {}): MotionEnvironment & {
	root: MotionRoot;
	nudges: number;
	humanoidImpl: PoseHumanoid & { bones: Map<string, Bone> };
} {
	const root = makeRoot();
	const humanoidImpl = makeHumanoid();
	const env = {
		root,
		humanoidImpl,
		nudges: 0,
		humanoid: () => humanoidImpl as PoseHumanoid,
		nudge: ((bone, x, z, y = 0) => {
			if (!bone) return;
			env.nudges += 1;
			bone.rotation.x += x;
			bone.rotation.z += z;
			bone.rotation.y += y;
		}) satisfies PoseNudge,
		cameraPosition: (out: { x: number; y: number; z: number }) => {
			out.x = 0;
			out.y = 1;
			out.z = 2;
			return true;
		},
		// Hips world height tracks the root like the live bone measurement.
		measureHipsY: () => 0.9 + root.position.y,
		log: () => {},
		...overrides
	};
	return env;
}

test('walk moves toward the viewer and completes', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	motion.startWalk('forward', 300);
	assert.equal(motion.isBusy(false), true);
	let completions = motion.update(0.1);
	assert.deepEqual(completions, []);
	assert.ok(env.root.position.z > 0, 'moved toward the camera');
	completions = motion.update(0.25);
	assert.deepEqual(completions, [{ type: 'locomotion', routine: true }]);
	assert.equal(motion.isBusy(false), false);
});

test('run is faster than walk', () => {
	const walkEnv = makeEnv();
	const runEnv = makeEnv();
	const walk = new MotionController(walkEnv);
	const run = new MotionController(runEnv);
	walk.startWalk('forward', 1000);
	run.startRun('forward', 1000);
	walk.update(0.5);
	run.update(0.5);
	assert.ok(runEnv.root.position.z > walkEnv.root.position.z);
});

test('goto arrives at the anchor, return-home at the center', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	env.root.position.x = 1;
	const target = resolveGotoTarget({ anchorId: 'chair' });
	assert.ok(target);
	motion.startGoto(target.x, target.z, `goto:${target.label}`);
	for (let i = 0; i < 200 && motion.isBusy(false); i++) motion.update(0.05);
	assert.ok(
		Math.hypot(env.root.position.x - target.x, env.root.position.z - target.z) < 0.07,
		'arrived at the anchor'
	);
	motion.startReturnHome();
	for (let i = 0; i < 200 && motion.isBusy(false); i++) motion.update(0.05);
	assert.ok(Math.hypot(env.root.position.x, env.root.position.z) < 0.07);
});

test('goto target resolution prefers anchors, then coordinates', () => {
	assert.deepEqual(resolveGotoTarget({ anchorId: 'chair' }), {
		x: 0.9,
		z: 0.35,
		label: 'chair'
	});
	assert.deepEqual(resolveGotoTarget({ x: 0.5, z: -0.5 }), {
		x: 0.5,
		z: -0.5,
		label: '0.50,-0.50'
	});
	assert.equal(resolveGotoTarget({ anchorId: 'mars' }), null);
	assert.equal(resolveGotoTarget({}), null);
});

test('turns complete for routine steps and silently for ambient ones', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	assert.equal(motion.startTurn('sideways', true), false);
	assert.ok(motion.startTurn('left', true));
	let completions: MotionCompletion[] = [];
	for (let i = 0; i < 100 && completions.length === 0; i++) {
		completions = motion.update(0.05);
	}
	assert.deepEqual(completions, [{ type: 'turn', routine: true }]);
	assert.ok(Math.abs(env.root.yaw - Math.PI / 2) < 0.05);

	assert.ok(motion.startFaceCamera(false));
	assert.equal(motion.isBusy(false), false, 'ambient turns never hold busy');
	completions = [];
	for (let i = 0; i < 100 && completions.length === 0; i++) {
		completions = motion.update(0.05);
	}
	assert.deepEqual(completions, [{ type: 'turn', routine: false }]);
	// Facing the camera at +Z from the origin means yaw ~0.
	assert.ok(Math.abs(env.root.yaw) < 0.1);
});

test('jump arcs and lands exactly at zero', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	motion.startJump();
	let apex = 0;
	let completions: MotionCompletion[] = [];
	for (let i = 0; i < 60; i++) {
		completions = motion.update(1 / 60);
		apex = Math.max(apex, env.root.position.y);
		if (completions.length > 0) break;
	}
	assert.deepEqual(completions, [{ type: 'jump', routine: true }]);
	assert.ok(apex > 0.2, `apex ${apex}`);
	assert.equal(env.root.position.y, 0);
});

test('procedural actions run, sit persists until stand', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	assert.equal(motion.startProcedural('moonwalk'), false);
	assert.ok(motion.startProcedural('nod'));
	motion.update(0.5);
	assert.ok(env.nudges > 0, 'bone program applied');
	let completions = motion.update(0.5);
	assert.deepEqual(completions, [{ type: 'procedural', routine: true }]);

	assert.ok(motion.startProcedural('sit', 'chair'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	assert.equal(motion.sitting, true);
	assert.ok(env.root.position.y < 0, 'root dropped to seat height');
	// Held posture keeps applying without completions.
	assert.deepEqual(motion.update(0.05), []);
	assert.ok(motion.startProcedural('stand'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	assert.equal(motion.sitting, false);
	assert.equal(env.root.position.y, 0);
});

test('locomotion stands first, then moves — never teleports upright', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	assert.ok(motion.startProcedural('sit'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	assert.equal(motion.sitting, true);
	const seatY = env.root.position.y;
	assert.ok(seatY < 0);

	motion.startWalk('forward', 300);
	// Still seated the moment the walk is requested: the stand plays first.
	assert.equal(motion.sitting, true);
	assert.equal(env.root.position.y, seatY);
	assert.equal(env.root.position.z, 0);

	let stoodFrame = -1;
	let movedFrame = -1;
	const seen: string[] = [];
	for (let i = 0; i < 300 && motion.isBusy(false); i++) {
		for (const c of motion.update(0.05)) seen.push(c.type);
		if (stoodFrame < 0 && !motion.sitting) stoodFrame = i;
		if (movedFrame < 0 && env.root.position.z > 0) movedFrame = i;
	}
	assert.ok(stoodFrame >= 0, 'stood up during the sequence');
	assert.ok(movedFrame >= 0, 'walked after standing');
	assert.ok(movedFrame >= stoodFrame, 'no translation before the stand finished');
	assert.deepEqual(seen, ['procedural', 'locomotion']);
	assert.equal(env.root.position.y, 0);
});

test('sit twice is idempotent; stand while standing is a no-op', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	assert.ok(motion.startProcedural('sit'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	const seatY = env.root.position.y;

	// Second sit completes at once without re-measuring or lifting.
	assert.ok(motion.startProcedural('sit'));
	assert.deepEqual(motion.update(0.05), [{ type: 'procedural', routine: true }]);
	assert.equal(motion.sitting, true);
	assert.equal(env.root.position.y, seatY);

	assert.ok(motion.startProcedural('stand'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	assert.equal(env.root.position.y, 0);

	// Standing while standing completes at once with no crouch dip.
	assert.ok(motion.startProcedural('stand'));
	assert.deepEqual(motion.update(0.05), [{ type: 'procedural', routine: true }]);
	assert.equal(env.root.position.y, 0);
	assert.deepEqual(motion.update(0.5), []);
	assert.equal(env.root.position.y, 0);
});

test('cancel mid-sit restores standing; cancel mid-stand restores seated', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	assert.ok(motion.startProcedural('sit'));
	motion.update(0.5);
	assert.ok(env.root.position.y < 0, 'sit transition lowered the root');
	motion.cancel();
	assert.equal(env.root.position.y, 0);
	assert.equal(motion.sitting, false);
	assert.equal(motion.isBusy(false), false);
	assert.deepEqual(motion.update(0.5), []);
	assert.equal(env.root.position.y, 0, 'no partial posture survives cancel');

	// stopKind('procedural') restores the same way.
	assert.ok(motion.startProcedural('sit'));
	motion.update(0.5);
	assert.ok(env.root.position.y < 0);
	motion.stopKind('procedural');
	assert.equal(env.root.position.y, 0);
	assert.equal(motion.sitting, false);

	// A cancelled stand reverts to the seated baseline.
	assert.ok(motion.startProcedural('sit'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	const seatY = env.root.position.y;
	assert.ok(motion.startProcedural('stand'));
	motion.update(0.4);
	assert.ok(env.root.position.y > seatY, 'stand transition raised the root');
	motion.cancel();
	assert.equal(env.root.position.y, seatY);
	assert.equal(motion.sitting, true);
	// Held posture keeps the seat afterward.
	motion.update(0.05);
	assert.equal(env.root.position.y, seatY);
});

test('sit/stand reversals glide from mid-pose without snapping', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	assert.ok(motion.startProcedural('sit'));
	motion.update(0.5);
	const midY = env.root.position.y;
	assert.ok(midY < 0 && midY > -0.45, `mid-transition root ${midY}`);

	// Reverse into stand: root rises continuously from midY, never snaps.
	assert.ok(motion.startProcedural('stand'));
	let prev = midY;
	let maxStep = 0;
	for (let i = 0; i < 100 && motion.isBusy(false); i++) {
		motion.update(0.05);
		maxStep = Math.max(maxStep, Math.abs(env.root.position.y - prev));
		prev = env.root.position.y;
	}
	assert.equal(motion.sitting, false);
	assert.equal(env.root.position.y, 0);
	assert.ok(maxStep < 0.1, `reversal glides (max step ${maxStep})`);

	// And back the other way: stand halfway, re-sit to full depth.
	assert.ok(motion.startProcedural('sit'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	const seatY = env.root.position.y;
	assert.ok(motion.startProcedural('stand'));
	motion.update(0.4);
	assert.ok(motion.startProcedural('sit'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	assert.equal(motion.sitting, true);
	assert.ok(
		Math.abs(env.root.position.y - seatY) < 1e-9,
		're-sit after reversal reaches full seat depth'
	);
});

test('jump while seated stands first, then arcs', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	assert.ok(motion.startProcedural('sit'));
	for (let i = 0; i < 100 && motion.isBusy(false); i++) motion.update(0.05);
	motion.startJump();
	assert.equal(motion.sitting, true, 'still seated when the jump is requested');
	const seen: string[] = [];
	let apex = 0;
	for (let i = 0; i < 300 && motion.isBusy(false); i++) {
		for (const c of motion.update(1 / 60)) seen.push(c.type);
		apex = Math.max(apex, env.root.position.y);
	}
	assert.deepEqual(seen, ['procedural', 'jump']);
	assert.ok(apex > 0.2, `jump arced after standing (apex ${apex})`);
	assert.equal(env.root.position.y, 0);
});

test('controller poses through its motion basis', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	motion.setMotionBasis(VRM0_MOTION_BASIS);
	assert.ok(motion.startProcedural('sit'));
	motion.update(0.6);
	const thigh = env.humanoidImpl.bones.get('leftUpperLeg');
	assert.ok(thigh, 'sit nudged the thigh');
	// VRM0 scene space mirrors pitch: the thigh angle is positive where
	// VRM1 would be negative for the identical anatomical swing.
	assert.ok(thigh.rotation.x > 0, `vrm0 thigh mirrors (x=${thigh.rotation.x})`);
});

test('cancel halts motion and stopKind halts one kind', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	motion.startWalk('forward', 3000);
	motion.cancel();
	assert.equal(motion.isBusy(false), false);
	assert.deepEqual(motion.update(0.5), []);

	motion.startJump();
	motion.update(0.1);
	assert.ok(env.root.position.y > 0);
	motion.stopKind('jump');
	assert.equal(env.root.position.y, 0);
	assert.deepEqual(motion.update(0.5), []);
});

test('resetPose clears posture and root transform', () => {
	const env = makeEnv();
	const motion = new MotionController(env);
	env.root.position.x = 1;
	env.root.yaw = 2;
	assert.ok(motion.startProcedural('sit'));
	motion.resetPose();
	assert.deepEqual(
		{ ...env.root.position, yaw: env.root.yaw },
		{ x: 0, y: 0, z: 0, yaw: 0 }
	);
	assert.equal(motion.sitting, false);
	assert.equal(motion.isBusy(false), false);
});
