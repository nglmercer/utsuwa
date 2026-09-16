// Humanoid motion basis: the ONE place that knows which way is anatomically
// forward for procedural bone rotations.
//
// three-vrm normalized bones are identity-rest helper objects parented under
// the model scene, so a local Euler offset is expressed in scene space. VRM
// 1.0 models face +Z with no scene rotation; VRM 0.x models face -Z and the
// loader Y-flips the scene by PI to face the camera. That flip mirrors local
// X (pitch) and Z (roll) rotations anatomically: the same `rotation.x`
// swings a VRM1 thigh forward and a VRM0 thigh backward. (Local Y / yaw is
// unaffected — the flip axis is Y.)
//
// Every procedural body program therefore speaks in semantic operations
// (`thighForward`, `kneeFlex`, ...) through a basis resolved once at load,
// instead of writing raw `rotation.x += ±number` signs. Angles below are
// authored in the VRM1 sense; the basis mirrors them for VRM0.
//
// Framework-free and THREE-free: pure sign/angle math, node-testable.
import type { PoseHumanoid, PoseNudge } from './procedural-pose-controller.ts';

export type LegSide = 'left' | 'right';
export type ArmSide = 'left' | 'right';

export interface HumanoidMotionBasis {
	// +1 when model space faces +Z (VRM1, no scene flip), -1 when model
	// space faces -Z (VRM0, scene Y-flipped by PI at load).
	readonly facing: 1 | -1;
}

export const VRM1_MOTION_BASIS: HumanoidMotionBasis = { facing: 1 };
export const VRM0_MOTION_BASIS: HumanoidMotionBasis = { facing: -1 };

export function motionBasisFor(metaVersion: string | undefined): HumanoidMotionBasis {
	return metaVersion === '0' ? VRM0_MOTION_BASIS : VRM1_MOTION_BASIS;
}

function bone(humanoid: PoseHumanoid | null, name: string) {
	return humanoid?.getNormalizedBoneNode(name) ?? null;
}

// --- Semantic leg operations (amount > 0 is the named direction) ---

// Swing the whole thigh forward from the hip (sitting, stepping).
export function thighForward(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	side: LegSide,
	amount: number
): void {
	nudge(bone(humanoid, `${side}UpperLeg`), -amount * basis.facing, 0);
}

// Flex the knee: heel toward the buttock, shin folding backward.
export function kneeFlex(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	side: LegSide,
	amount: number
): void {
	nudge(bone(humanoid, `${side}LowerLeg`), amount * basis.facing, 0);
}

// Pitch the foot at the ankle. Positive toes-down (plantarflex), negative
// toes-up (dorsiflex), relative to the shin.
export function footPitch(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	side: LegSide,
	amount: number
): void {
	nudge(bone(humanoid, `${side}Foot`), amount * basis.facing, 0);
}

// --- Semantic torso/head operations ---

// Pitch the torso forward (bowing). Negative arches slightly back.
export function torsoForward(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	amount: number
): void {
	nudge(bone(humanoid, 'spine'), amount * basis.facing, 0);
}

// Pitch the head forward/down (nodding). Positive tips the chin down.
export function headPitch(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	amount: number
): void {
	nudge(bone(humanoid, 'head'), amount * basis.facing, 0);
}

// Pitch the neck with the head (nodding assist).
export function neckPitch(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	amount: number
): void {
	nudge(bone(humanoid, 'neck'), amount * basis.facing, 0);
}

// Pitch the chest with the torso (bowing assist).
export function chestPitch(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	amount: number
): void {
	nudge(bone(humanoid, 'chest'), amount * basis.facing, 0);
}

// --- Semantic arm operations ---

// Swing the whole arm forward from the shoulder (walk counter-swing).
export function armSwing(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	side: ArmSide,
	amount: number
): void {
	nudge(bone(humanoid, `${side}UpperArm`), -amount * basis.facing, 0);
}

// Lift the arm sideways away from the body (shrugging). Outward is
// mirror-symmetric: +Z on the character-left arm, -Z on the right.
export function shoulderAbduct(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	basis: HumanoidMotionBasis,
	side: ArmSide,
	amount: number
): void {
	const outward = side === 'left' ? 1 : -1;
	nudge(bone(humanoid, `${side}UpperArm`), 0, outward * amount * basis.facing);
}
