// Procedural pose programs: small additive bone routines (nod, shake,
// bow, shrug, sit, stand) plus the shared walk leg/arm swing. Runs on a
// structural bone interface — no Three.js import — so it stays testable
// under node; the renderer injects the live humanoid and the nudge sink
// (which unwinds every offset next frame).
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

// Seated lower body at blend k (0 standing, 1 fully seated): thighs swing
// forward, shins hang down, spine stays upright. The root-Y drop (applied
// by the motion controller) puts the hips at seat height so the feet stay
// near the floor instead of dangling.
export function applySittingPose(humanoid: PoseHumanoid | null, nudge: PoseNudge, k: number): void {
	if (!humanoid || k <= 0) return;
	nudge(humanoid.getNormalizedBoneNode('leftUpperLeg'), -1.25 * k, 0);
	nudge(humanoid.getNormalizedBoneNode('rightUpperLeg'), -1.25 * k, 0);
	nudge(humanoid.getNormalizedBoneNode('leftLowerLeg'), 1.35 * k, 0);
	nudge(humanoid.getNormalizedBoneNode('rightLowerLeg'), 1.35 * k, 0);
	nudge(humanoid.getNormalizedBoneNode('spine'), -0.06 * k, 0);
}

// Shared step swing for walk, run, and steered goto arrivals: opposing
// leg/arm swing at the caller's phase.
export function applyWalkSwing(humanoid: PoseHumanoid | null, nudge: PoseNudge, swing: number): void {
	if (!humanoid) return;
	nudge(humanoid.getNormalizedBoneNode('leftUpperLeg'), -0.38 * swing, 0);
	nudge(humanoid.getNormalizedBoneNode('rightUpperLeg'), 0.38 * swing, 0);
	nudge(humanoid.getNormalizedBoneNode('leftLowerLeg'), 0.45 * Math.max(0, -swing), 0);
	nudge(humanoid.getNormalizedBoneNode('rightLowerLeg'), 0.45 * Math.max(0, swing), 0);
	nudge(humanoid.getNormalizedBoneNode('leftUpperArm'), 0.2 * swing, 0);
	nudge(humanoid.getNormalizedBoneNode('rightUpperArm'), -0.2 * swing, 0);
}

// One procedural frame at progress p (0..1). Small additive programs that
// read correctly without mocap; all offsets unwind via the nudge sink.
export function applyProceduralAction(
	humanoid: PoseHumanoid | null,
	nudge: PoseNudge,
	name: string,
	p: number
): void {
	if (!humanoid) return;
	if (name === 'nod') {
		const a = Math.sin(p * Math.PI * 2);
		nudge(humanoid.getNormalizedBoneNode('head'), -0.3 * a, 0);
		nudge(humanoid.getNormalizedBoneNode('neck'), -0.12 * a, 0);
	} else if (name === 'shake_head') {
		nudge(humanoid.getNormalizedBoneNode('head'), 0, 0, 0.45 * Math.sin(p * Math.PI * 2));
	} else if (name === 'bow') {
		const k = smoothstep(0, 0.35, p) * (1 - smoothstep(0.65, 1, p));
		nudge(humanoid.getNormalizedBoneNode('spine'), 0.38 * k, 0);
		nudge(humanoid.getNormalizedBoneNode('chest'), 0.12 * k, 0);
		nudge(humanoid.getNormalizedBoneNode('head'), 0.24 * k, 0);
	} else if (name === 'shrug') {
		const k = Math.sin(p * Math.PI);
		for (const side of ['leftUpperArm', 'rightUpperArm'] as const) {
			const bone = humanoid.getNormalizedBoneNode(side);
			// Outward follows the rig's own rest sign, so v0 and v1 agree.
			if (bone) nudge(bone, 0, (Math.sign(bone.rotation.z) || 1) * 0.28 * k);
		}
		nudge(humanoid.getNormalizedBoneNode('head'), -0.06 * k, 0.1 * k);
	} else if (name === 'sit') {
		applySittingPose(humanoid, nudge, smoothstep(0, 0.8, p));
	} else if (name === 'stand') {
		applySittingPose(humanoid, nudge, 1 - smoothstep(0, 0.8, p));
	}
}
