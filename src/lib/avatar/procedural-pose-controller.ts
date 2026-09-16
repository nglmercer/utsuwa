// Procedural pose programs: small additive bone routines (nod, shake,
// bow, shrug, sit, stand) plus the shared walk leg/arm swing. Runs on a
// structural bone interface — no Three.js import — so it stays testable
// under node; the renderer injects the live humanoid and the nudge sink
// (which unwinds every offset next frame).
//
// All rotations go through the HumanoidMotionBasis as semantic operations
// (thighForward, kneeFlex, ...): raw Euler signs mirror anatomically
// between VRM0 and VRM1, so no program writes a bare `rotation.x += ±n`.
import {
	armSwing,
	chestPitch,
	footPitch,
	headPitch,
	kneeFlex,
	neckPitch,
	shoulderAbduct,
	thighForward,
	torsoForward,
	type HumanoidMotionBasis
} from './humanoid-motion-basis.ts';

export interface PoseBone {
	rotation: { x: number; y: number; z: number };
}

export interface PoseHumanoid {
	getNormalizedBoneNode(name: string): PoseBone | null;
}

export type PoseNudge = (bone: PoseBone | null, x: number, z: number, y?: number) => void;

export const PROCEDURAL_DURATIONS: Record<string, number> = {
	nod: 0.9,
	shake_head: 0.9,
	bow: 1.3,
	shrug: 0.8,
	sit: 1.1,
	stand: 0.9
};

export function smoothstep(edge0: number, edge1: number, x: number): number {
	const t = Math.min(1, Math.max(0, (x - edge0) / (edge1 - edge0)));
	return t * t * (3 - 2 * t);
}

// Full seated pose, in VRM1-sense radians (the basis mirrors for VRM0):
// thighs forward ~77°, knees flexed ~83° (interior ~97°), shins hanging
// near vertical, feet leveled against the residual shin tilt, spine
// nearly upright. The root-Y drop (applied by the motion controller)
// puts the hips at seat height so the feet stay near the floor.
export const SIT_THIGH_FORWARD = 1.35;
export const SIT_KNEE_FLEX = 1.45;
export const SIT_FOOT_LEVEL = -0.1;
export const SIT_SPINE_BACK = -0.06;

// Seated lower body at blend k (0 standing, 1 fully seated).
export function applySittingPose(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	k: number
): void {
	if (!humanoid || k <= 0) return;
	for (const side of ['left', 'right'] as const) {
		thighForward(humanoid, nudge, basis, side, SIT_THIGH_FORWARD * k);
		kneeFlex(humanoid, nudge, basis, side, SIT_KNEE_FLEX * k);
		footPitch(humanoid, nudge, basis, side, SIT_FOOT_LEVEL * k);
	}
	torsoForward(humanoid, nudge, basis, SIT_SPINE_BACK * k);
}

// Shared step swing for walk, run, and steered goto arrivals: opposing
// leg/arm swing at the caller's phase. Positive swing drives the left
// thigh forward while the left arm counter-swings back.
export function applyWalkSwing(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	swing: number
): void {
	if (!humanoid) return;
	thighForward(humanoid, nudge, basis, 'left', 0.38 * swing);
	thighForward(humanoid, nudge, basis, 'right', -0.38 * swing);
	kneeFlex(humanoid, nudge, basis, 'left', 0.45 * Math.max(0, -swing));
	kneeFlex(humanoid, nudge, basis, 'right', 0.45 * Math.max(0, swing));
	armSwing(humanoid, nudge, basis, 'left', -0.2 * swing);
	armSwing(humanoid, nudge, basis, 'right', 0.2 * swing);
}

// One procedural frame at progress p (0..1). Small additive programs that
// read correctly without mocap; all offsets unwind via the nudge sink.
// Sit/stand accept an explicit seated weight so the motion controller can
// interpolate from a captured baseline (mid-transition reversals); without
// one the weight derives from progress assuming a standing/seated start.
export function applyProceduralAction(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	name: string,
	p: number,
	seatedWeight?: number
): void {
	if (!humanoid) return;
	if (name === 'nod') {
		const a = Math.sin(p * Math.PI * 2);
		headPitch(humanoid, nudge, basis, 0.3 * a);
		neckPitch(humanoid, nudge, basis, 0.12 * a);
	} else if (name === 'shake_head') {
		// Pure yaw: the VRM0 scene flip is about Y, so no mirroring applies.
		nudge(humanoid.getNormalizedBoneNode('head'), 0, 0, 0.45 * Math.sin(p * Math.PI * 2));
	} else if (name === 'bow') {
		const k = smoothstep(0, 0.35, p) * (1 - smoothstep(0.65, 1, p));
		torsoForward(humanoid, nudge, basis, 0.38 * k);
		chestPitch(humanoid, nudge, basis, 0.12 * k);
		headPitch(humanoid, nudge, basis, 0.24 * k);
	} else if (name === 'shrug') {
		const k = Math.sin(p * Math.PI);
		shoulderAbduct(humanoid, nudge, basis, 'left', 0.28 * k);
		shoulderAbduct(humanoid, nudge, basis, 'right', 0.28 * k);
		headPitch(humanoid, nudge, basis, -0.06 * k);
		nudge(humanoid.getNormalizedBoneNode('head'), 0, 0, 0.1 * k);
	} else if (name === 'sit') {
		applySittingPose(humanoid, nudge, basis, seatedWeight ?? smoothstep(0, 0.8, p));
	} else if (name === 'stand') {
		applySittingPose(humanoid, nudge, basis, seatedWeight ?? (1 - smoothstep(0, 0.8, p)));
	}
}
