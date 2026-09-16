// Runtime gesture gate: decides whether a parsed gesture actually executes.
// Pure and dependency-free. The prompt tells the model that gestures are
// exceptional; this module enforces it independently of model obedience:
// cooldowns, duplicate suppression, rate limits, busy/photo guards, and the
// explicit-request requirement for large gestures (jump, dance, walk).
import { AVATAR_ACTIONS, type GestureCue } from './avatar-actions.ts';

export type AvatarMotionState =
	| 'idle'
	| 'talking'
	| 'thinking'
	| 'tool_running'
	| 'emote'
	| 'locomotion'
	| 'photo_mode';

// Any intentional action at all must wait this long after the previous one.
export const GLOBAL_GESTURE_COOLDOWN_MS = 6000;
// Same action twice needs at least this gap (registry cooldowns can extend it).
export const SAME_GESTURE_COOLDOWN_MS = 15000;
// Automatic gestures allowed per rolling minute (explicit user commands exempt).
export const MAX_AUTOMATIC_GESTURES_PER_MINUTE = 4;
const RATE_WINDOW_MS = 60000;

export interface GestureGateState {
	// Key + timestamp of the last EXECUTED gesture (staging records it).
	lastKey: string | null;
	lastAt: number;
	// Timestamps of recent executed gestures, for the rate limit.
	recentAt: number[];
}

export function createGestureGateState(): GestureGateState {
	return { lastKey: null, lastAt: 0, recentAt: [] };
}

export interface GestureGateContext {
	cue: GestureCue;
	// Millisecond clock, injected so tests control time.
	now: number;
	state: GestureGateState;
	motion: AvatarMotionState;
	// An intentional action is currently playing on the avatar.
	busy: boolean;
	// The user explicitly asked for this action ("jump!", "walk left").
	explicitRequest: boolean;
}

export type GestureGateRejection =
	| 'cooldown'
	| 'duplicate'
	| 'busy'
	| 'photo-mode'
	| 'not-explicit'
	| 'rate-limit';

export type GestureGateResult = { allowed: true } | { allowed: false; reason: GestureGateRejection };

// Stable identity for deduplication. Legacy numbered emotes keep their own
// namespace so they never collide with semantic actions. Parameterized
// animation cues (turn direction, goto anchor) include their parameter so
// "turn left" never suppresses "turn right".
export function gestureKey(cue: GestureCue): string {
	switch (cue.type) {
		case 'animation': {
			let key = `animation:${cue.action}`;
			if (cue.direction) key += `:${cue.direction}`;
			if (cue.anchorId) key += `:${cue.anchorId}`;
			return key;
		}
		case 'locomotion':
			return `locomotion:${cue.action}:${cue.direction}`;
		case 'reaction':
			return `reaction:${cue.zone}`;
		case 'emote':
			return `emote:${cue.id}`;
	}
}

function duplicateWindowMs(cue: GestureCue): number {
	if (cue.type === 'animation' || cue.type === 'locomotion') {
		return Math.max(SAME_GESTURE_COOLDOWN_MS, AVATAR_ACTIONS[cue.action].cooldownMs);
	}
	return SAME_GESTURE_COOLDOWN_MS;
}

function requiresExplicit(cue: GestureCue): boolean {
	if (cue.type === 'animation' || cue.type === 'locomotion') {
		return AVATAR_ACTIONS[cue.action].explicitRequestOnly === true;
	}
	return false;
}

export function evaluateGestureGate(ctx: GestureGateContext): GestureGateResult {
	const { cue, now, state, motion, busy, explicitRequest } = ctx;
	// Photo mode owns the body unconditionally, even for explicit commands.
	if (motion === 'photo_mode') return { allowed: false, reason: 'photo-mode' };
	if (requiresExplicit(cue) && !explicitRequest) return { allowed: false, reason: 'not-explicit' };
	// Explicit user commands bypass the autonomous anti-spam policy
	// (cooldowns, duplicates, rate limit, busy): only hard safety states
	// reject them. The gate stays strict for model-generated gestures.
	if (explicitRequest) return { allowed: true };
	if (busy) return { allowed: false, reason: 'busy' };
	const key = gestureKey(cue);
	if (state.lastKey === key && now - state.lastAt < duplicateWindowMs(cue)) {
		return { allowed: false, reason: 'duplicate' };
	}
	if (state.lastKey !== null && state.lastKey !== key && now - state.lastAt < GLOBAL_GESTURE_COOLDOWN_MS) {
		return { allowed: false, reason: 'cooldown' };
	}
	const recent = state.recentAt.filter((t) => now - t < RATE_WINDOW_MS);
	if (recent.length >= MAX_AUTOMATIC_GESTURES_PER_MINUTE) {
		return { allowed: false, reason: 'rate-limit' };
	}
	return { allowed: true };
}

// Record an executed gesture. Called by staging AFTER the gate allows it
// (never for rejected cues), so rejections don't extend cooldowns.
export function recordGestureExecution(state: GestureGateState, cue: GestureCue, now: number): void {
	state.lastKey = gestureKey(cue);
	state.lastAt = now;
	state.recentAt = [...state.recentAt.filter((t) => now - t < RATE_WINDOW_MS), now];
}
