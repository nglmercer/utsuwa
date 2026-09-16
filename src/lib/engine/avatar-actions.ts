// Semantic avatar actions: the single source of truth mapping AI-facing
// action names to playback metadata. Pure and dependency-free (no Svelte, no
// Three.js) so the prompt builder, response parser, renderer, and tests share
// it without drift. The model NEVER addresses animation files directly.
import type { TouchZone } from './photo-reactions.ts';

// One step of an avatar routine: a procedural bone program, a root-motion
// jump, a bounded walk/run, a return-home, a turn, a face-camera, a goto, or
// a VRMA emote clip. Shared by the command parser, the VRM store, and the
// renderer so all three agree on the shape.
export interface AvatarRoutineStep {
	kind:
		| 'procedural'
		| 'jump'
		| 'walk'
		| 'emote'
		| 'return_home'
		| 'turn'
		| 'face_camera'
		| 'goto';
	action: string;
	direction?: LocomotionDirection | TurnDirection;
	durationMs?: number;
	url?: string;
	// Goto target: a scene anchor id (preferred, AI-facing) or an explicit
	// plane coordinate (programmatic/dev only; the model never sends raw x/z).
	anchorId?: string;
	x?: number;
	z?: number;
}

// A one-shot body direction from the model ("wave while saying this").
// Semantic actions first; legacy numbered emotes stay parseable for backward
// compatibility but are never advertised to the model.
export type GestureCue =
	| {
			type: 'animation';
			action: AvatarActionName;
			// Turn direction for `turn`; anchor id for `goto` and seated `sit`.
			direction?: TurnDirection;
			anchorId?: string;
	  }
	| { type: 'locomotion'; action: LocomotionActionName; direction: LocomotionDirection; durationMs: number }
	| { type: 'reaction'; zone: TouchZone }
	| { type: 'emote'; id: string };
//
// Asset grounding (motion analysis of static/animations/*.vrma, 2026-09-15):
// all 7 emote clips are 7-12s full-body performances. Nothing on disk shows a
// head-nod oscillation, head-shake, held bow, shrug, jump spike, or walk
// cycle, so nod/shake_head/bow/shrug are procedural bone pulses (small,
// correct-looking without mocap) and jump/walk are world-motion on AvatarRoot
// (+ procedural leg swing for walk). VRMA mappings carry a confidence note;
// confirm or remap them visually in Developer Tools.

export type AvatarActionName =
	| 'wave'
	| 'nod'
	| 'shake_head'
	| 'bow'
	| 'shrug'
	| 'celebrate'
	| 'dance'
	| 'jump'
	| 'return_home'
	| 'face_camera'
	| 'turn'
	| 'goto'
	| 'sit'
	| 'stand';

// Walk and run share the locomotion pipeline (kind 'walk', action selects
// the speed); routine receipts keep them distinct as walk:walk:* / walk:run:*.
export type LocomotionActionName = 'walk' | 'run';

export const LOCOMOTION_ACTIONS: readonly LocomotionActionName[] = ['walk', 'run'];

export type LocomotionDirection = 'left' | 'right' | 'forward' | 'back';

export const LOCOMOTION_DIRECTIONS: readonly LocomotionDirection[] = [
	'left',
	'right',
	'forward',
	'back'
];

// In-place yaw: left +90°, right −90°, back 180°. No forward (a no-op turn).
export type TurnDirection = 'left' | 'right' | 'back';

export const TURN_DIRECTIONS: readonly TurnDirection[] = ['left', 'right', 'back'];

// Locomotion tuning, single-sourced here so the renderer, prompt, and tests
// agree. Speeds are m/s on the avatar plane; the radius bounds every walk,
// run, return-home, and goto.
export const WALK_SPEED_MPS = 0.45;
export const RUN_SPEED_MPS = 1.15;
export const GOTO_SPEED_MPS = 0.6;
export const WALK_STEP_HZ = 1.7;
export const RUN_STEP_HZ = 2.7;
export const WALK_MAX_RADIUS = 2.0;
export const TURN_SPEED_RPS = 2.6;
export const GOTO_ARRIVE_DIST = 0.06;
export const TURN_ARRIVE_RAD = 0.03;
// Hip height for sittable anchors without an explicit seat height.
export const DEFAULT_SEAT_HEIGHT = 0.45;

// AI locomotion is bounded: brisk enough to read, short enough to stay
// framed. Authoritative here so the command parser, response parser, prompt
// contract, renderer, and tests agree on the same window.
export const WALK_DURATION_MIN_MS = 300;
export const WALK_DURATION_MAX_MS = 3000;
export const WALK_DURATION_DEFAULT_MS = 1200;

// Clamp a walk/run duration to the authoritative window. NaN/Infinity fall
// back to the default instead of freezing a step (a NaN remainingMs never
// reaches zero AND the watchdog comparison never fires).
export function clampWalkDuration(durationMs: number): number {
	if (!Number.isFinite(durationMs)) return WALK_DURATION_DEFAULT_MS;
	return Math.min(WALK_DURATION_MAX_MS, Math.max(WALK_DURATION_MIN_MS, Math.round(durationMs)));
}

// Authoritative execution semantics: exactly how the runtime performs this
// action. Consumers must branch on this (or use the shared conversion
// helpers in avatar-action-runtime.ts), never infer behavior from the
// action name.
export type AvatarExecution =
	| { kind: 'vrma'; url: string }
	| { kind: 'procedural' }
	| { kind: 'jump' }
	| { kind: 'walk' }
	| { kind: 'return_home' }
	| { kind: 'face_camera' }
	| { kind: 'turn' }
	| { kind: 'goto' };

export interface AvatarActionDefinition {
	id: AvatarActionName | LocomotionActionName;
	label: string;
	// One line for the prompt catalog: when this action is appropriate.
	description: string;
	execution: AvatarExecution;
	mode: 'oneshot' | 'loop';
	category:
		| 'greeting'
		| 'agreement'
		| 'disagreement'
		| 'celebration'
		| 'movement'
		| 'social'
		| 'expressive';
	cooldownMs: number;
	// False hides the action from the model (dev/manual only).
	aiAllowed: boolean;
	// If true, the runtime gate only runs it on an explicit user request.
	explicitRequestOnly?: boolean;
	// Face to wear during the action. Absent = keep the current mood face;
	// body action and facial emotion stay separately controllable.
	expression?: { expression: string; intensity: number };
}

export const AVATAR_ACTIONS: Record<AvatarActionName | LocomotionActionName, AvatarActionDefinition> = {
	wave: {
		id: 'wave',
		label: 'Wave',
		description: 'greeting or goodbye',
		execution: { kind: 'vrma', url: '/animations/VRMA_04.vrma' },
		// Medium confidence: right-arm raise arc with finger articulation and
		// return-to-rest. Alternate candidate VRMA_03 (held right-forearm-up).
		mode: 'oneshot',
		category: 'greeting',
		cooldownMs: 15000,
		aiAllowed: true,
		expression: { expression: 'happy', intensity: 0.6 }
	},
	nod: {
		id: 'nod',
		label: 'Nod',
		description: 'clear agreement or acknowledgement',
		execution: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'agreement',
		cooldownMs: 8000,
		aiAllowed: true
	},
	shake_head: {
		id: 'shake_head',
		label: 'Shake head',
		description: 'clear disagreement',
		execution: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'disagreement',
		cooldownMs: 8000,
		aiAllowed: true
	},
	bow: {
		id: 'bow',
		label: 'Bow',
		description: 'greeting, thanks, or apology',
		execution: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'social',
		cooldownMs: 12000,
		aiAllowed: true,
		expression: { expression: 'relaxed', intensity: 0.5 }
	},
	shrug: {
		id: 'shrug',
		label: 'Shrug',
		description: 'uncertainty',
		execution: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'expressive',
		cooldownMs: 10000,
		aiAllowed: true
	},
	celebrate: {
		id: 'celebrate',
		label: 'Celebrate',
		description: 'strong success or excitement',
		execution: { kind: 'vrma', url: '/animations/VRMA_07.vrma' },
		// Medium confidence: symmetric arm pumps with knee dips, twice.
		mode: 'oneshot',
		category: 'celebration',
		cooldownMs: 20000,
		aiAllowed: true,
		expression: { expression: 'happy', intensity: 0.8 }
	},
	dance: {
		id: 'dance',
		label: 'Dance',
		description: 'only when explicitly requested',
		execution: { kind: 'vrma', url: '/animations/VRMA_05.vrma' },
		// Medium-low confidence: full-body symmetric arm arcs with a turn.
		// Alternate candidate VRMA_01 (sway with a turn).
		mode: 'oneshot',
		category: 'expressive',
		cooldownMs: 30000,
		aiAllowed: true,
		explicitRequestOnly: true,
		expression: { expression: 'happy', intensity: 0.7 }
	},
	jump: {
		id: 'jump',
		label: 'Jump',
		description: 'only when explicitly requested or clearly appropriate',
		execution: { kind: 'jump' },
		mode: 'oneshot',
		category: 'movement',
		cooldownMs: 15000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	walk: {
		id: 'walk',
		label: 'Walk',
		description: 'only when the user asks you to move',
		execution: { kind: 'walk' },
		mode: 'loop',
		category: 'movement',
		cooldownMs: 10000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	run: {
		id: 'run',
		label: 'Run',
		description: 'only when the user asks you to run or hurry',
		execution: { kind: 'walk' },
		mode: 'loop',
		category: 'movement',
		cooldownMs: 10000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	return_home: {
		id: 'return_home',
		label: 'Return home',
		description: 'go back to the center of the room',
		execution: { kind: 'return_home' },
		mode: 'oneshot',
		category: 'movement',
		cooldownMs: 8000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	face_camera: {
		id: 'face_camera',
		label: 'Face viewer',
		description: 'turn to face the viewer',
		execution: { kind: 'face_camera' },
		mode: 'oneshot',
		category: 'movement',
		cooldownMs: 8000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	turn: {
		id: 'turn',
		label: 'Turn',
		description: 'turn in place',
		execution: { kind: 'turn' },
		mode: 'oneshot',
		category: 'movement',
		cooldownMs: 8000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	goto: {
		id: 'goto',
		label: 'Go to',
		description: 'go to a named place',
		execution: { kind: 'goto' },
		mode: 'oneshot',
		category: 'movement',
		cooldownMs: 10000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	sit: {
		id: 'sit',
		label: 'Sit down',
		description: 'sit down in place until you stand',
		execution: { kind: 'procedural' },
		mode: 'loop',
		category: 'movement',
		cooldownMs: 8000,
		aiAllowed: true,
		explicitRequestOnly: true
	},
	stand: {
		id: 'stand',
		label: 'Stand up',
		description: 'stand up from sitting',
		execution: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'movement',
		cooldownMs: 5000,
		aiAllowed: true,
		explicitRequestOnly: true
	}
};

export const AVATAR_ACTION_NAMES = Object.keys(AVATAR_ACTIONS) as Array<
	AvatarActionName | LocomotionActionName
>;

export function isAvatarActionName(value: unknown): value is AvatarActionName | LocomotionActionName {
	return typeof value === 'string' && value in AVATAR_ACTIONS;
}

export function isLocomotionDirection(value: unknown): value is LocomotionDirection {
	return (
		typeof value === 'string' && (LOCOMOTION_DIRECTIONS as readonly string[]).includes(value)
	);
}

export function isTurnDirection(value: unknown): value is TurnDirection {
	return typeof value === 'string' && (TURN_DIRECTIONS as readonly string[]).includes(value);
}

export function isLocomotionActionName(value: unknown): value is LocomotionActionName {
	return value === 'walk' || value === 'run';
}

// Actions the prompt may offer the model, in catalog order.
export function aiAllowedActions(): AvatarActionDefinition[] {
	return AVATAR_ACTION_NAMES.map((name) => AVATAR_ACTIONS[name]).filter((def) => def.aiAllowed);
}

// Legacy numbered emotes (Developer Tools + backward-compatible cues).
// Kept out of the prompt; the model addresses semantic actions instead.
export const LEGACY_EMOTE_URLS: Record<string, string> = {
	vrma_01: '/animations/VRMA_01.vrma',
	vrma_02: '/animations/VRMA_02.vrma',
	vrma_03: '/animations/VRMA_03.vrma',
	vrma_04: '/animations/VRMA_04.vrma',
	vrma_05: '/animations/VRMA_05.vrma',
	vrma_06: '/animations/VRMA_06.vrma',
	vrma_07: '/animations/VRMA_07.vrma'
};

export function resolveLegacyEmote(id: string): string | null {
	return LEGACY_EMOTE_URLS[id] ?? null;
}

// Reverse lookup: which semantic action (if any) plays this clip. Used for
// the per-action face on manually triggered emotes.
export function actionForAnimationUrl(url: string): AvatarActionDefinition | null {
	for (const name of AVATAR_ACTION_NAMES) {
		const def = AVATAR_ACTIONS[name];
		if (def.execution.kind === 'vrma' && def.execution.url === url) return def;
	}
	return null;
}

export function expressionForAnimationUrl(url: string): { expression: string; intensity: number } | null {
	return actionForAnimationUrl(url)?.expression ?? null;
}

// Jump arc: normalized parabola over root height. Exactly 0 at takeoff and
// landing, `height` at the apex. The renderer sets the root Y absolutely from
// this every frame, so landing is exact and drift is impossible.
export function jumpArcHeight(progress: number, height: number): number {
	const p = Math.min(1, Math.max(0, progress));
	return 4 * height * p * (1 - p);
}

// Keep the avatar inside a radius of its origin so AI walks stay framed.
// Points inside pass through untouched; points outside project onto the rim.
export function clampWalkOffset(x: number, z: number, maxRadius: number): { x: number; z: number } {
	const dist = Math.hypot(x, z);
	if (!(dist > maxRadius)) return { x, z };
	const scale = maxRadius / dist;
	return { x: x * scale, z: z * scale };
}

// Normalize a yaw delta to (−π, π]: the shortest signed turn. Shared by the
// renderer (turn easing, face-camera) and the prompt (facing error).
export function shortAngleDelta(delta: number): number {
	while (delta > Math.PI) delta -= Math.PI * 2;
	while (delta < -Math.PI) delta += Math.PI * 2;
	return delta;
}

// Yaw that faces `target` from `from`, in Three.js convention (0 faces +Z,
// matching the walker's atan2(dirX, dirZ)). The renderer and prompt agree.
export function yawToFacePoint(fromX: number, fromZ: number, targetX: number, targetZ: number): number {
	return Math.atan2(targetX - fromX, targetZ - fromZ);
}

const TURN_YAW_DELTA: Record<TurnDirection, number> = {
	left: Math.PI / 2,
	right: -Math.PI / 2,
	back: Math.PI
};

// Absolute yaw after an in-place turn from the current yaw.
export function turnTargetYaw(currentYaw: number, direction: TurnDirection): number {
	return currentYaw + (TURN_YAW_DELTA[direction] ?? Math.PI);
}

export interface AvatarSpatialSnapshot {
	// Meters from the room center.
	distFromHome: number;
	// True when clamped at (or past 95% of) the walk radius.
	atRim: boolean;
	// Absolute angle between where she faces and the viewer, 0..180.
	facingErrorDeg: number;
}

// Pure spatial summary for the prompt: where she is, whether she is stuck
// at the rim, and how far her facing is from the viewer. NaN-safe: garbage
// in reads as centered-and-facing rather than poisoning the prompt.
export function computeAvatarSpatial(
	avatarX: number,
	avatarZ: number,
	avatarYaw: number,
	cameraX: number,
	cameraZ: number,
	maxRadius: number = WALK_MAX_RADIUS
): AvatarSpatialSnapshot {
	const x = Number.isFinite(avatarX) ? avatarX : 0;
	const z = Number.isFinite(avatarZ) ? avatarZ : 0;
	const yaw = Number.isFinite(avatarYaw) ? avatarYaw : 0;
	const cx = Number.isFinite(cameraX) ? cameraX : 0;
	const cz = Number.isFinite(cameraZ) ? cameraZ : 1;
	const distFromHome = Math.hypot(x, z);
	const atRim = Number.isFinite(maxRadius) && maxRadius > 0 && distFromHome >= maxRadius * 0.95;
	const facingErrorDeg =
		Math.abs(shortAngleDelta(yawToFacePoint(x, z, cx, cz) - yaw)) * (180 / Math.PI);
	return { distFromHome, atRim, facingErrorDeg };
}

// Root-Y offset that puts the hips at seat height: negative (the whole rig
// drops) and clamped so a bad measurement can't bury or launch the avatar.
export function computeSitOffsetY(hipsWorldY: number, seatHeight: number): number {
	const hips = Number.isFinite(hipsWorldY) ? hipsWorldY : 0.9;
	const seat = Number.isFinite(seatHeight) ? seatHeight : DEFAULT_SEAT_HEIGHT;
	return Math.min(0, Math.max(-0.8, seat - hips));
}
