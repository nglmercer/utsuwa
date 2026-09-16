// Kinematic pose tests: the sit assertion that matters is not "nudges > 0"
// but "knees point the right way". A planar forward-kinematics rig replays
// the recorded scene-space bone angles for each motion basis, maps them to
// world space (VRM0 inherits the loader's Y-flip; VRM1 is identity), and
// asserts anatomy in the world frame where both models face +Z:
//
// knee in front of hip, ankle below knee, foot level-ish and never above
// the knee, left/right mirror-symmetric — for VRM0 AND VRM1 alike.
import test from 'node:test';
import assert from 'node:assert/strict';

import {
	motionBasisFor,
	VRM0_MOTION_BASIS,
	VRM1_MOTION_BASIS,
	thighForward,
	type HumanoidMotionBasis
} from './humanoid-motion-basis.ts';
import {
	applyProceduralAction,
	applySittingPose,
	applyWalkSwing,
	type PoseHumanoid,
	type PoseNudge
} from './procedural-pose-controller.ts';

type Vec3 = [number, number, number];

// Segment lengths (meters, roughly adult proportions).
const THIGH = 0.42;
const SHIN = 0.42;
const FOOT = 0.22;
const TORSO = 0.5;
const HEAD = 0.24;
const ARM = 0.6;
const HIP_X = 0.09;
const HIP_Y = 0.9;
const SHOULDER_X = 0.2;
const SHOULDER_Y = 1.45;
const NECK_Y = 1.5;

function rotateX(v: Vec3, a: number): Vec3 {
	const [x, y, z] = v;
	return [x, y * Math.cos(a) - z * Math.sin(a), y * Math.sin(a) + z * Math.cos(a)];
}

function rotateZ(v: Vec3, a: number): Vec3 {
	const [x, y, z] = v;
	return [x * Math.cos(a) - y * Math.sin(a), x * Math.sin(a) + y * Math.cos(a), z];
}

function add(a: Vec3, b: Vec3): Vec3 {
	return [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
}

function scale(v: Vec3, s: number): Vec3 {
	return [v[0] * s, v[1] * s, v[2] * s];
}

// Model-space rest directions per convention. VRM1 faces +Z; VRM0 faces
// -Z (character-left is -X there — the exact mirror of VRM1).
function conventionFrame(basis: HumanoidMotionBasis) {
	const vrm0 = basis.facing === -1;
	return {
		forwardZ: (vrm0 ? -1 : 1) as 1 | -1,
		leftX: (vrm0 ? -1 : 1) as 1 | -1
	};
}

// Scene space to world space. The loader Y-flips VRM0 scenes by PI so
// both face the camera; VRM1 scenes are identity.
function toWorld(v: Vec3, basis: HumanoidMotionBasis): Vec3 {
	return basis.facing === -1 ? [-v[0], v[1], -v[2]] : v;
}

interface FakeRig {
	humanoid: PoseHumanoid;
	angle: (bone: string) => { x: number; y: number; z: number };
}

function makeRig(): FakeRig {
	const bones = new Map<string, { x: number; y: number; z: number }>();
	const humanoid: PoseHumanoid = {
		getNormalizedBoneNode: (name: string) => {
			let bone = bones.get(name);
			if (!bone) {
				bone = { x: 0, y: 0, z: 0 };
				bones.set(name, bone);
			}
			return { rotation: bone };
		}
	};
	return {
		humanoid,
		angle: (bone: string) => ({ ...(bones.get(bone) ?? { x: 0, y: 0, z: 0 }) })
	};
}

function recordNudge(): PoseNudge {
	return (bone, x, z, y = 0) => {
		if (!bone) return;
		bone.rotation.x += x;
		bone.rotation.z += z;
		bone.rotation.y += y;
	};
}

interface LegChain {
	hip: Vec3;
	knee: Vec3;
	ankle: Vec3;
	toe: Vec3;
}

// Forward kinematics for one leg in WORLD space, from recorded angles.
function legChain(
	rig: FakeRig,
	basis: HumanoidMotionBasis,
	side: 'left' | 'right'
): LegChain {
	const { leftX, forwardZ } = conventionFrame(basis);
	const sideSign = side === 'left' ? leftX : -leftX;
	const hip: Vec3 = [sideSign * HIP_X, HIP_Y, 0];
	const thighX = rig.angle(`${side}UpperLeg`).x;
	const kneeX = rig.angle(`${side}LowerLeg`).x;
	const footX = rig.angle(`${side}Foot`).x;
	const knee = add(hip, scale(rotateX([0, -1, 0], thighX), THIGH));
	const ankle = add(knee, scale(rotateX([0, -1, 0], thighX + kneeX), SHIN));
	const toe = add(
		ankle,
		scale(rotateX([0, 0, forwardZ], thighX + kneeX + footX), FOOT)
	);
	return {
		hip: toWorld(hip, basis),
		knee: toWorld(knee, basis),
		ankle: toWorld(ankle, basis),
		toe: toWorld(toe, basis)
	};
}

function interiorKneeAngle(chain: LegChain): number {
	// Angle between thigh-reversed (knee->hip) and shin (knee->ankle).
	const t: Vec3 = [
		chain.hip[0] - chain.knee[0],
		chain.hip[1] - chain.knee[1],
		chain.hip[2] - chain.knee[2]
	];
	const s: Vec3 = [
		chain.ankle[0] - chain.knee[0],
		chain.ankle[1] - chain.knee[1],
		chain.ankle[2] - chain.knee[2]
	];
	const dot = t[0] * s[0] + t[1] * s[1] + t[2] * s[2];
	const mag = Math.hypot(...t) * Math.hypot(...s);
	return (Math.acos(Math.min(1, Math.max(-1, dot / mag))) * 180) / Math.PI;
}

const BASES: Array<{ name: string; basis: HumanoidMotionBasis }> = [
	{ name: 'VRM1', basis: VRM1_MOTION_BASIS },
	{ name: 'VRM0', basis: VRM0_MOTION_BASIS }
];

test('motionBasisFor resolves the VRM version to a facing', () => {
	assert.equal(motionBasisFor('0').facing, -1);
	assert.equal(motionBasisFor('1').facing, 1);
	assert.equal(motionBasisFor(undefined).facing, 1);
	assert.equal(motionBasisFor('2').facing, 1);
});

test('semantic ops mirror exactly between conventions', () => {
	for (const side of ['left', 'right'] as const) {
		const a = makeRig();
		const b = makeRig();
		thighForward(a.humanoid, recordNudge(), VRM1_MOTION_BASIS, side, 0.5);
		thighForward(b.humanoid, recordNudge(), VRM0_MOTION_BASIS, side, 0.5);
		assert.equal(a.angle(`${side}UpperLeg`).x, 0.5 * -1);
		assert.equal(b.angle(`${side}UpperLeg`).x, 0.5);
	}
});

for (const { name, basis } of BASES) {
	test(`sit is anatomically seated (${name}): knees forward, shins down, feet level`, () => {
		const rig = makeRig();
		applySittingPose(rig.humanoid, recordNudge(), basis, 1);
		for (const side of ['left', 'right'] as const) {
			const chain = legChain(rig, basis, side);
			assert.ok(
				chain.knee[2] > chain.hip[2] + 0.2,
				`${name} ${side}: knee z=${chain.knee[2].toFixed(2)} should be well in front of hip z=${chain.hip[2].toFixed(2)}`
			);
			assert.ok(
				chain.ankle[1] < chain.knee[1] - 0.2,
				`${name} ${side}: ankle y=${chain.ankle[1].toFixed(2)} should be well below knee y=${chain.knee[1].toFixed(2)}`
			);
			assert.ok(
				chain.toe[1] < chain.knee[1],
				`${name} ${side}: foot must never end above the knee`
			);
			assert.ok(
				Math.abs(chain.toe[1] - chain.ankle[1]) < 0.06,
				`${name} ${side}: foot should be approximately level (toe-ankle dy=${Math.abs(chain.toe[1] - chain.ankle[1]).toFixed(3)})`
			);
			const interior = interiorKneeAngle(chain);
			assert.ok(
				interior >= 80 && interior <= 100,
				`${name} ${side}: knee interior ${interior.toFixed(1)}° should be 80-100°`
			);
		}
		// Left/right mirror symmetry in world space.
		const left = legChain(rig, basis, 'left');
		const right = legChain(rig, basis, 'right');
		for (const joint of ['knee', 'ankle', 'toe'] as const) {
			assert.ok(Math.abs(left[joint][0] + right[joint][0]) < 1e-9, `${name}: ${joint} x mirrors`);
			assert.ok(Math.abs(left[joint][1] - right[joint][1]) < 1e-9, `${name}: ${joint} y matches`);
			assert.ok(Math.abs(left[joint][2] - right[joint][2]) < 1e-9, `${name}: ${joint} z matches`);
		}
	});

	test(`half sit is a mid pose, not a snap (${name})`, () => {
		const rig = makeRig();
		applySittingPose(rig.humanoid, recordNudge(), basis, 0.5);
		const chain = legChain(rig, basis, 'left');
		assert.ok(chain.knee[2] > chain.hip[2], `${name}: half-sit knee still forward`);
		assert.ok(chain.knee[2] < 0.42, `${name}: half-sit knee short of full extension`);
		assert.ok(chain.ankle[1] < chain.knee[1], `${name}: half-sit ankle below knee`);
	});

	test(`walk swing drives opposing legs (${name})`, () => {
		const rig = makeRig();
		applyWalkSwing(rig.humanoid, recordNudge(), basis, 1);
		const left = legChain(rig, basis, 'left');
		const right = legChain(rig, basis, 'right');
		assert.ok(left.knee[2] > left.hip[2], `${name}: left knee forward on swing=1`);
		assert.ok(right.knee[2] < right.hip[2], `${name}: right knee back on swing=1`);

		const rig2 = makeRig();
		applyWalkSwing(rig2.humanoid, recordNudge(), basis, -1);
		const back = legChain(rig2, basis, 'left');
		// Trailing leg flexes: ankle folds behind the knee line.
		assert.ok(
			back.ankle[2] < back.knee[2],
			`${name}: trailing ankle behind knee (ankle z=${back.ankle[2].toFixed(2)}, knee z=${back.knee[2].toFixed(2)})`
		);
	});

	test(`bow tips the torso forward (${name})`, () => {
		const rig = makeRig();
		applyProceduralAction(rig.humanoid, recordNudge(), basis, 'bow', 0.5);
		const spineX = rig.angle('spine').x;
		const chestX = rig.angle('chest').x;
		const headX = rig.angle('head').x;
		const chest = add([0, HIP_Y, 0], scale(rotateX([0, 1, 0], spineX), TORSO));
		const head = add(chest, scale(rotateX([0, 1, 0], spineX + chestX + headX), HEAD));
		const chestW = toWorld(chest, basis);
		const headW = toWorld(head, basis);
		assert.ok(chestW[2] > 0.05, `${name}: chest forward of hips (z=${chestW[2].toFixed(2)})`);
		assert.ok(headW[2] > chestW[2], `${name}: head forward of chest`);
	});

	test(`nod rocks the head forward and back (${name})`, () => {
		const fwd = makeRig();
		applyProceduralAction(fwd.humanoid, recordNudge(), basis, 'nod', 0.125);
		const topFwd = add([0, NECK_Y, 0], scale(rotateX([0, 1, 0], fwd.angle('head').x), HEAD));
		assert.ok(toWorld(topFwd, basis)[2] > 0, `${name}: nod tips forward at p=0.125`);
		const back = makeRig();
		applyProceduralAction(back.humanoid, recordNudge(), basis, 'nod', 0.625);
		const topBack = add([0, NECK_Y, 0], scale(rotateX([0, 1, 0], back.angle('head').x), HEAD));
		assert.ok(toWorld(topBack, basis)[2] < 0, `${name}: nod tips back at p=0.625`);
	});

	test(`shrug lifts both arms symmetrically (${name})`, () => {
		const rig = makeRig();
		applyProceduralAction(rig.humanoid, recordNudge(), basis, 'shrug', 0.5);
		const { leftX } = conventionFrame(basis);
		const hands: Record<string, Vec3> = {};
		for (const side of ['left', 'right'] as const) {
			const sideSign = side === 'left' ? leftX : -leftX;
			const shoulder: Vec3 = [sideSign * SHOULDER_X, SHOULDER_Y, 0];
			// T-pose rest: arm points sideways away from the torso.
			const rest: Vec3 = [sideSign, 0, 0];
			const z = rig.angle(`${side}UpperArm`).z;
			const hand = add(shoulder, scale(rotateZ(rest, z), ARM));
			hands[side] = toWorld(hand, basis);
			const shoulderW = toWorld(shoulder, basis);
			assert.ok(hands[side][1] > shoulderW[1], `${name} ${side}: shrug raises the arm`);
			assert.ok(
				Math.abs(hands[side][0]) > Math.abs(shoulderW[0]),
				`${name} ${side}: shrug moves the hand outward`
			);
		}
		assert.ok(
			Math.abs(hands.left[0] + hands.right[0]) < 1e-9,
			`${name}: shrug is left/right symmetric`
		);
	});
}
