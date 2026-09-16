// Avatar action identity: the single place that turns a gesture cue or a
// routine step into a stable, comparable identity — and that identity into
// the string keys used for deduplication, correlation, receipts, and logs.
// Pure and node-safe. Nobody else may hand-build `animation:turn:left`-style
// strings; every key below flows through these serializers.
import type { GestureCue, LocomotionDirection, TurnDirection } from './avatar-actions.ts';

// Structural step shape: both engine routine steps and host routine inputs
// satisfy this without importing Svelte stores.
export interface RoutineStepLike {
	kind: string;
	action: string;
	direction?: string;
	anchorId?: string;
	x?: number;
	z?: number;
}

export interface AvatarActionIdentity {
	// Semantic action name (animation/locomotion), e.g. 'turn', 'walk'.
	action: string;
	direction?: string;
	anchorId?: string;
	legacyId?: string;
	reactionZone?: string;
	// Cue channel: locomotion cues and walk steps share one namespace,
	// everything else shares the animation namespace.
	channel?: 'animation' | 'locomotion';
	// Routine-step kind, for the receipt namespace only
	// (`kind:action[:direction][:anchor]`).
	kind?: string;
	// Explicit plane coordinate for coordinate gotos (receipts only).
	x?: number;
	z?: number;
}

function cueChannel(cue: GestureCue): 'animation' | 'locomotion' {
	return cue.type === 'locomotion' ? 'locomotion' : 'animation';
}

export function identityFromGestureCue(cue: GestureCue): AvatarActionIdentity {
	switch (cue.type) {
		case 'animation': {
			const identity: AvatarActionIdentity = { action: cue.action, channel: 'animation' };
			if (cue.direction !== undefined) identity.direction = cue.direction;
			if (cue.anchorId !== undefined) identity.anchorId = cue.anchorId;
			return identity;
		}
		case 'locomotion':
			return {
				action: cue.action,
				direction: cue.direction,
				channel: 'locomotion'
			};
		case 'reaction':
			return { action: 'reaction', reactionZone: cue.zone, channel: 'animation' };
		case 'emote':
			return { action: 'emote', legacyId: cue.id, channel: 'animation' };
	}
}

export function identityFromRoutineStep(step: RoutineStepLike): AvatarActionIdentity {
	const identity: AvatarActionIdentity = {
		action: step.action,
		channel: step.kind === 'walk' ? 'locomotion' : 'animation',
		kind: step.kind
	};
	if (step.direction !== undefined) identity.direction = step.direction;
	if (step.anchorId !== undefined) identity.anchorId = step.anchorId;
	if (typeof step.x === 'number' && typeof step.z === 'number') {
		identity.x = step.x;
		identity.z = step.z;
	}
	return identity;
}

// Gesture namespace: `animation:<action>[:direction][:anchor]`,
// `locomotion:<action>:<direction>`, `reaction:<zone>`, `emote:<id>`.
// Used for gesture dedup, direct-command/model-cue suppression, and logging.
export function serializeAvatarIdentity(identity: AvatarActionIdentity): string {
	if (identity.legacyId !== undefined) return `emote:${identity.legacyId}`;
	if (identity.reactionZone !== undefined) return `reaction:${identity.reactionZone}`;
	const prefix = identity.channel === 'locomotion' ? 'locomotion' : 'animation';
	let key = `${prefix}:${identity.action}`;
	if (identity.direction !== undefined) key += `:${identity.direction}`;
	if (identity.anchorId !== undefined) key += `:${identity.anchorId}`;
	return key;
}

// Receipt namespace: `kind:action[:direction][:anchorId | x,z]`, shared by
// the renderer (receipts) and task creators (verification specs). Changing
// this format breaks in-flight `avatar_routine` verification expectations,
// so it is pinned by tests.
export function serializeRoutineIdentity(identity: AvatarActionIdentity): string {
	const kind = identity.kind ?? (identity.channel === 'locomotion' ? 'walk' : 'procedural');
	let key = `${kind}:${identity.action}`;
	if (identity.direction !== undefined) key += `:${identity.direction}`;
	if (typeof identity.anchorId === 'string' && identity.anchorId) {
		key += `:${identity.anchorId}`;
	} else if (typeof identity.x === 'number' && typeof identity.z === 'number') {
		key += `:${identity.x.toFixed(2)},${identity.z.toFixed(2)}`;
	}
	return key;
}

// Convenience: one call from cue/step to correlation key.
export function gestureCueKey(cue: GestureCue): string {
	return serializeAvatarIdentity(identityFromGestureCue(cue));
}

export function routineStepGestureKey(step: RoutineStepLike): string {
	const identity = identityFromRoutineStep(step);
	// Historical correlation defaults: a bare walk steps forward, a bare
	// turn turns back, and a goto without an anchor keeps its trailing
	// colon (`animation:goto:`) — receipts never gain these defaults.
	if (identity.direction === undefined) {
		if (step.kind === 'walk') identity.direction = 'forward' satisfies LocomotionDirection;
		else if (step.kind === 'turn') identity.direction = 'back' satisfies TurnDirection;
	}
	if (step.kind === 'goto' && identity.anchorId === undefined) identity.anchorId = '';
	return serializeAvatarIdentity(identity);
}

export function routineStepReceiptKey(step: RoutineStepLike): string {
	return serializeRoutineIdentity(identityFromRoutineStep(step));
}
