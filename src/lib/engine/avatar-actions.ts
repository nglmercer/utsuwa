// Semantic avatar actions: the single source of truth mapping AI-facing
// action names to playback metadata. Pure and dependency-free (no Svelte, no
// Three.js) so the prompt builder, response parser, renderer, and tests share
// it without drift. The model NEVER addresses animation files directly.
import type { TouchZone } from './photo-reactions.ts';

// One step of an avatar routine: a procedural bone program, a root-motion
// jump, a bounded walk, or a VRMA emote clip. Shared by the command parser,
// the VRM store, and the renderer so all three agree on the shape.
export interface AvatarRoutineStep {
	kind: 'procedural' | 'jump' | 'walk' | 'emote';
	action: string;
	direction?: LocomotionDirection;
	durationMs?: number;
	url?: string;
}

// A one-shot body direction from the model ("wave while saying this").
// Semantic actions first; legacy numbered emotes stay parseable for backward
// compatibility but are never advertised to the model.
export type GestureCue =
	| { type: 'animation'; action: AvatarActionName }
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
	| 'jump';

export type LocomotionActionName = 'walk';

export type LocomotionDirection = 'left' | 'right' | 'forward' | 'back';

export const LOCOMOTION_DIRECTIONS: readonly LocomotionDirection[] = [
	'left',
	'right',
	'forward',
	'back'
];

export type AvatarActionSource =
	// A shipped .vrma clip played as a one-shot through the mixer.
	| { kind: 'vrma'; url: string }
	// A scripted bone-pulse routine in the renderer (no asset needed).
	| { kind: 'procedural' }
	// AvatarRoot world motion (jump arc / walk translation), optionally with
	// a procedural leg swing underneath. No skeleton asset required.
	| { kind: 'world-motion' };

export interface AvatarActionDefinition {
	id: AvatarActionName | LocomotionActionName;
	label: string;
	// One line for the prompt catalog: when this action is appropriate.
	description: string;
	source: AvatarActionSource;
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
		// Medium confidence: right-arm raise arc with finger articulation and
		// return-to-rest. Alternate candidate VRMA_03 (held right-forearm-up).
		source: { kind: 'vrma', url: '/animations/VRMA_04.vrma' },
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
		source: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'agreement',
		cooldownMs: 8000,
		aiAllowed: true
	},
	shake_head: {
		id: 'shake_head',
		label: 'Shake head',
		description: 'clear disagreement',
		source: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'disagreement',
		cooldownMs: 8000,
		aiAllowed: true
	},
	bow: {
		id: 'bow',
		label: 'Bow',
		description: 'greeting, thanks, or apology',
		source: { kind: 'procedural' },
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
		source: { kind: 'procedural' },
		mode: 'oneshot',
		category: 'expressive',
		cooldownMs: 10000,
		aiAllowed: true
	},
	celebrate: {
		id: 'celebrate',
		label: 'Celebrate',
		description: 'strong success or excitement',
		// Medium confidence: symmetric arm pumps with knee dips, twice.
		source: { kind: 'vrma', url: '/animations/VRMA_07.vrma' },
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
		// Medium-low confidence: full-body symmetric arm arcs with a turn.
		// Alternate candidate VRMA_01 (sway with a turn).
		source: { kind: 'vrma', url: '/animations/VRMA_05.vrma' },
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
		source: { kind: 'world-motion' },
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
		source: { kind: 'world-motion' },
		mode: 'loop',
		category: 'movement',
		cooldownMs: 10000,
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
		if (def.source.kind === 'vrma' && def.source.url === url) return def;
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
