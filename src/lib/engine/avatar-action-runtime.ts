// Avatar runtime conversions: the single place that turns registry actions
// and model cues into executable routine steps and renderer requests.
// Pure and node-safe. Branches on the registry's `execution` field — never
// on action names — so adding an action is a registry edit, not a hunt
// through if/else chains in the parser, turn staging, and dev tools.
import {
	AVATAR_ACTIONS,
	clampWalkDuration,
	WALK_DURATION_DEFAULT_MS,
	type AvatarActionName,
	type AvatarRoutineStep,
	type GestureCue,
	type LocomotionActionName,
	type LocomotionDirection,
	type TurnDirection
} from './avatar-actions.ts';

// Renderer invocation without the store's sequence number: structurally
// identical to `AvatarActionRequest` minus `seq`, which the store assigns.
// (Defined here instead of importing the Svelte store, which is not
// node-safe.)
export interface AvatarActionInvocation {
	kind: 'procedural' | 'jump' | 'walk' | 'return_home' | 'turn' | 'face_camera' | 'goto';
	action: string;
	direction?: LocomotionDirection | TurnDirection;
	durationMs?: number;
	anchorId?: string;
	x?: number;
	z?: number;
}

export interface RoutineStepParams {
	direction?: LocomotionDirection | TurnDirection;
	durationMs?: number;
	anchorId?: string;
	x?: number;
	z?: number;
}

// One registry action → one executable routine step. Parameterized actions
// take their parameter from `params` (walk/run default to stepping forward
// for the default duration; turn defaults to 'back'); goto without an
// anchor or coordinate is not executable and returns null.
export function actionToRoutineStep(
	action: AvatarActionName | LocomotionActionName,
	params: RoutineStepParams = {}
): AvatarRoutineStep | null {
	const def = AVATAR_ACTIONS[action];
	const execution = def.execution;
	switch (execution.kind) {
		case 'vrma':
			return { kind: 'emote', action, url: execution.url };
		case 'procedural': {
			const step: AvatarRoutineStep = { kind: 'procedural', action };
			if (params.anchorId !== undefined) step.anchorId = params.anchorId;
			return step;
		}
		case 'jump':
			return { kind: 'jump', action };
		case 'walk':
			return {
				kind: 'walk',
				action,
				direction: (params.direction as LocomotionDirection | undefined) ?? 'forward',
				durationMs: clampWalkDuration(params.durationMs ?? WALK_DURATION_DEFAULT_MS)
			};
		case 'return_home':
			return { kind: 'return_home', action };
		case 'face_camera':
			return { kind: 'face_camera', action };
		case 'turn':
			return {
				kind: 'turn',
				action,
				direction: (params.direction as TurnDirection | undefined) ?? 'back'
			};
		case 'goto': {
			if (params.anchorId !== undefined) {
				return { kind: 'goto', action, anchorId: params.anchorId };
			}
			if (typeof params.x === 'number' && typeof params.z === 'number') {
				return { kind: 'goto', action, x: params.x, z: params.z };
			}
			return null;
		}
	}
}

// One model cue → one executable routine step, or null when the cue is not
// a routine step at all (reactions reuse the tap path; legacy emotes play
// their clip directly). A goto cue without an anchor is dropped for the
// same reason: stepping somewhere random would be the wrong motion.
export function cueToRoutineStep(cue: GestureCue): AvatarRoutineStep | null {
	switch (cue.type) {
		case 'animation':
			return actionToRoutineStep(cue.action, {
				direction: cue.direction,
				anchorId: cue.anchorId
			});
		case 'locomotion':
			return actionToRoutineStep(cue.action, {
				direction: cue.direction,
				durationMs: cue.durationMs
			});
		case 'reaction':
		case 'emote':
			return null;
	}
}

// One routine step → one renderer invocation. Emote steps have no action
// request (their clip plays through the animation effect), so they map to
// null and the caller plays `step.url` instead.
export function routineStepToAvatarRequest(step: AvatarRoutineStep): AvatarActionInvocation | null {
	switch (step.kind) {
		case 'procedural':
			return step.anchorId !== undefined
				? { kind: 'procedural', action: step.action, anchorId: step.anchorId }
				: { kind: 'procedural', action: step.action };
		case 'jump':
			return { kind: 'jump', action: step.action };
		case 'walk':
			return {
				kind: 'walk',
				action: step.action,
				direction: (step.direction as LocomotionDirection | undefined) ?? 'forward',
				durationMs: clampWalkDuration(step.durationMs ?? WALK_DURATION_DEFAULT_MS)
			};
		case 'return_home':
			return { kind: 'return_home', action: step.action };
		case 'turn':
			return {
				kind: 'turn',
				action: step.action,
				direction: (step.direction as TurnDirection | undefined) ?? 'back'
			};
		case 'face_camera':
			return { kind: 'face_camera', action: step.action };
		case 'goto': {
			if (step.anchorId !== undefined) {
				return { kind: 'goto', action: step.action, anchorId: step.anchorId };
			}
			if (typeof step.x === 'number' && typeof step.z === 'number') {
				return { kind: 'goto', action: step.action, x: step.x, z: step.z };
			}
			return null;
		}
		case 'emote':
			return null;
	}
}
