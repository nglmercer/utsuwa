// Avatar command plans: parse explicit user requests ("walk left 3s then
// right 3s") into ordered routine steps. Multi-segment aware — every action
// segment becomes a step, never just the first match. Pure and node-safe.
// English plus compact Japanese; word-boundaried to avoid "jumper"/"dancer".
import {
	AVATAR_ACTIONS,
	type AvatarActionName,
	type AvatarRoutineStep,
	type LocomotionDirection
} from './avatar-actions.ts';

export interface AvatarCommandPlan {
	steps: AvatarRoutineStep[];
	// True when the whole message is avatar direction (no conversation left).
	pureAvatarCommand: boolean;
	// Non-avatar remainder for mixed requests ("walk left and tell me a joke").
	remainingText?: string;
}

export const WALK_DURATION_MIN_MS = 300;
export const WALK_DURATION_MAX_MS = 3000;
export const WALK_DURATION_DEFAULT_MS = 1200;
const MAX_PLAN_STEPS = 8;

// Split sequential commands: "walk left then right", "jump, wave", "nod. bow".
const SEGMENT_SPLIT_RE = /\b(?:and then|then|after that|and)\b|[;,.\n、。！？]|!|\?/i;
// Polite filler that does not make a segment conversational.
const FILLER_RE = /\b(?:please|thanks|thank you|can you|could you|would you)\b|おねがい|ください/gi;
// Durations: "3 seconds", "3s", "3秒". Applies to walk steps.
const DURATION_RE = /(\d+)\s*(seconds?|secs?|s|秒)\b/i;

function actionStep(action: AvatarActionName): AvatarRoutineStep {
	const def = AVATAR_ACTIONS[action];
	if (def.source.kind === 'vrma') {
		return { kind: 'emote', action, url: def.source.url };
	}
	if (action === 'jump') return { kind: 'jump', action };
	return { kind: 'procedural', action };
}

function walkStep(direction: LocomotionDirection): (durationMs?: number) => AvatarRoutineStep {
	return (durationMs?: number) => ({
		kind: 'walk',
		action: 'walk',
		direction,
		durationMs: clampWalkDuration(durationMs ?? WALK_DURATION_DEFAULT_MS)
	});
}

export function clampWalkDuration(durationMs: number): number {
	if (!Number.isFinite(durationMs)) return WALK_DURATION_DEFAULT_MS;
	return Math.min(WALK_DURATION_MAX_MS, Math.max(WALK_DURATION_MIN_MS, Math.round(durationMs)));
}

const WALK_DIRS: Array<{ re: RegExp; direction: LocomotionDirection }> = [
	{ re: /\bleft\b|左/, direction: 'left' },
	{ re: /\bright\b|右/, direction: 'right' },
	{ re: /\b(forward|forwards|forth|ahead)\b|前/, direction: 'forward' },
	{ re: /\bback(wards?)?\b|後ろ|バック/, direction: 'back' }
];

const VERBS: Array<{ re: RegExp; action: AvatarActionName }> = [
	{ re: /\bjump(ing|ed)?\b|跳んで|ジャンプ/, action: 'jump' },
	{ re: /\bwav(e|ing)\b|手を振/, action: 'wave' },
	{ re: /\bnod(ing|ded)?\b|うなず|頷/, action: 'nod' },
	{ re: /shake\s+(your\s+)?head|首を(横に)?振/, action: 'shake_head' },
	{ re: /\bbow(ing|ed)?\b|お辞儀/, action: 'bow' },
	{ re: /\bdance[sd]?\b|\bdancing\b|踊/, action: 'dance' },
	{ re: /\bcelebrate[sd]?\b|お祝い/, action: 'celebrate' },
	{ re: /\bshrug(ged|s)?\b|肩をすくめ/, action: 'shrug' }
];

function parseDurationMs(segment: string): number | undefined {
	const match = DURATION_RE.exec(segment);
	if (!match) return undefined;
	return clampWalkDuration(Number(match[1]) * 1000);
}

function parseSegment(segment: string, inheritWalk: boolean): AvatarRoutineStep | null {
	const text = segment.toLowerCase();
	if (/\bwalk\b|歩/.test(text)) {
		for (const { re, direction } of WALK_DIRS) {
			if (re.test(text)) return walkStep(direction)(parseDurationMs(text));
		}
		return walkStep('forward')(parseDurationMs(text));
	}
	for (const { re, action } of VERBS) {
		if (re.test(text)) return actionStep(action);
	}
	// "walk left then right": a bare direction continues the previous walk.
	if (inheritWalk) {
		for (const { re, direction } of WALK_DIRS) {
			if (re.test(text)) return walkStep(direction)(parseDurationMs(text));
		}
	}
	return null;
}

// Parse a user message into an ordered avatar plan, or null when it carries
// no avatar command. Segments that are empty after filler-stripping are
// ignored; segments with other content make the plan mixed, never pure.
export function parseAvatarCommand(message: string): AvatarCommandPlan | null {
	if (typeof message !== 'string' || message.length === 0) return null;
	const steps: AvatarRoutineStep[] = [];
	const leftovers: string[] = [];
	for (const raw of message.split(SEGMENT_SPLIT_RE)) {
		const cleaned = raw.replace(FILLER_RE, ' ').replace(/\s+/g, ' ').trim();
		if (!cleaned) continue;
		const inheritWalk = steps.length > 0 && steps[steps.length - 1].action === 'walk';
		const step = parseSegment(cleaned, inheritWalk);
		if (step) {
			if (steps.length < MAX_PLAN_STEPS) steps.push(step);
		} else {
			leftovers.push(raw.trim());
		}
	}
	if (steps.length === 0) return null;
	const plan: AvatarCommandPlan = { steps, pureAvatarCommand: leftovers.length === 0 };
	if (leftovers.length > 0) plan.remainingText = leftovers.join(' ').trim();
	return plan;
}
