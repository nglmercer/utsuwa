// Avatar command plans: parse explicit user requests ("walk left 3s then
// right 3s") into ordered routine steps. Multi-segment aware — every action
// segment becomes a step, never just the first match. Pure and node-safe.
// English plus compact Japanese; word-boundaried to avoid "jumper"/"dancer".
import {
	AVATAR_ACTIONS,
	type AvatarActionName,
	type AvatarRoutineStep,
	type LocomotionActionName,
	type LocomotionDirection,
	type TurnDirection
} from './avatar-actions.ts';
import { resolveSceneAnchor } from './scene-anchors.ts';

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
// Durations: "3 seconds", "3s", "3秒". Applies to walk/run steps.
const DURATION_RE = /(\d+)\s*(seconds?|secs?|s|秒)\b/i;
// Locomotion verbs. "Step" is deliberately absent: "next step" and "step by
// step" are instructional chat, not movement, and would false-positive.
const LOCOMOTION_VERB_RE = /\bwalk\b|\brun\b|\bmove\b|\bgo\b|歩|走/;

function actionStep(action: AvatarActionName): AvatarRoutineStep {
	const def = AVATAR_ACTIONS[action];
	if (def.source.kind === 'vrma') {
		return { kind: 'emote', action, url: def.source.url };
	}
	if (action === 'jump') return { kind: 'jump', action };
	if (action === 'return_home') return { kind: 'return_home', action };
	if (action === 'face_camera') return { kind: 'face_camera', action };
	if (action === 'sit' || action === 'stand') return { kind: 'procedural', action };
	return { kind: 'procedural', action };
}

function locomotionStep(
	direction: LocomotionDirection,
	action: LocomotionActionName = 'walk'
): (durationMs?: number) => AvatarRoutineStep {
	return (durationMs?: number) => ({
		kind: 'walk',
		action,
		direction,
		durationMs: clampWalkDuration(durationMs ?? WALK_DURATION_DEFAULT_MS)
	});
}

export function clampWalkDuration(durationMs: number): number {
	if (!Number.isFinite(durationMs)) return WALK_DURATION_DEFAULT_MS;
	return Math.min(WALK_DURATION_MAX_MS, Math.max(WALK_DURATION_MIN_MS, Math.round(durationMs)));
}

const WALK_DIRS: Array<{ re: RegExp; direction: LocomotionDirection; notIf?: RegExp }> = [
	{ re: /\bleft\b|左/, direction: 'left' },
	{ re: /\bright\b|右/, direction: 'right' },
	{ re: /\b(forward|forwards|forth|ahead|front|closer|nearer)\b|前/, direction: 'forward' },
	{ re: /\bback(wards?)?\b|\b(backside|behind|farther|further)\b|後ろ|バック/, direction: 'back' },
	// Screen-relative: in the orbit view world-away reads as up-screen and
	// world-toward as down-screen. "Up" never matches approach phrasing ("up
	// to X" has its own branch below); the notIf guards keep idioms ("give
	// up", "calm down") from steering her.
	{
		re: /\bup\b(?!\s+to\b)/,
		direction: 'back',
		notIf:
			/\b(what'?s|what is|give|grow|grew|wake|woke|get|got|look|pick|scroll|bring|brought|hold|hang|step)\s+up\b/
	},
	{
		re: /\bdown\b/,
		direction: 'forward',
		notIf: /\b(lie|lies|lay|lying|calm|calming|sit|sitting|sat|kneel|sleep|settle|bend|break|broke|broken|slow)\b/
	}
];

const TURN_DIRS: Array<{ re: RegExp; direction: TurnDirection }> = [
	{ re: /\bleft\b|左/, direction: 'left' },
	{ re: /\bright\b|右/, direction: 'right' },
	{ re: /\b(back|around)\b|後ろ|バック/, direction: 'back' }
];

function findWalkDirection(text: string): LocomotionDirection | null {
	for (const { re, direction, notIf } of WALK_DIRS) {
		if (notIf?.test(text)) continue;
		if (re.test(text)) return direction;
	}
	return null;
}

function findTurnDirection(text: string): TurnDirection | null {
	for (const { re, direction } of TURN_DIRS) {
		if (re.test(text)) return direction;
	}
	return null;
}

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

// "Come back" phrasing means the room center — never a walk-back step, which
// lands nowhere near the origin. "Go back" alone stays a walk (see below).
const RETURN_HOME_RE =
	/\bcome\s+back\b|\breturn\b|\bback\s+to\s+(the\s+)?(center|middle|home)\b|\bgo\s+home\b|\b(original\s+position|home|center)\b|戻って|元の位置/;
// Facing the viewer is its own step: walks leave her in profile. The verb
// needs its target ("face me", "face the camera") so "my face hurts" stays chat.
const FACE_CAMERA_RE =
	/\bface\s+(me|the|toward|towards|forward)\b|\bface\b[^.?!]*\bcamera\b|\blook\s+at\s+me\b|こっちを見/;

function parseDurationMs(segment: string): number | undefined {
	const match = DURATION_RE.exec(segment);
	if (!match) return undefined;
	return clampWalkDuration(Number(match[1]) * 1000);
}

// Text after "go to" / "sit on": the anchor label ("the chair" -> chair).
function anchorLabelAfter(text: string, marker: RegExp): string | null {
	const match = marker.exec(text);
	if (!match || match.index === undefined) return null;
	return text.slice(match.index + match[0].length).trim() || null;
}

function parseSegment(
	segment: string,
	inheritLocomotion: LocomotionActionName | null
): AvatarRoutineStep | AvatarRoutineStep[] | null {
	const text = segment.toLowerCase();

	// "Go to the chair" resolves against the scene registry. A "go to"
	// phrasing with an unresolvable place is NOT a walk: stepping forward
	// would be the wrong motion, so the segment stays conversational — unless
	// the remainder is itself a direction ("go to the left" walks left).
	const gotoMarker = /\b(?:go|move|walk|run)\s+to\b/.exec(text);
	if (gotoMarker && gotoMarker.index !== undefined) {
		const label = text.slice(gotoMarker.index + gotoMarker[0].length).trim();
		const anchor = label ? resolveSceneAnchor(label) : null;
		if (anchor) return { kind: 'goto', action: 'goto', anchorId: anchor.id };
		// "Go to the left" walks left; "go to mars" is not a walk at all.
		const bare = label.replace(/^(the|a)\s+/, '').trim();
		if (
			!/^(left|right|forward|forwards|forth|ahead|front|closer|nearer|back|backwards?|backside|behind|farther|further|up|down|左|右|前|後ろ|バック)$/.test(
				bare
			)
		) {
			return null;
		}
	}

	// Approach phrasing ("walk up to the chair", "go up to me"): a named
	// anchor becomes a goto, the viewer becomes a forward walk, and an
	// unknown target stays conversational — stepping somewhere random would
	// be the wrong motion. Requires a locomotion verb so "it's up to me"
	// never moves her.
	if (LOCOMOTION_VERB_RE.test(text)) {
		const upTo = /\bup\s+to\b/.exec(text);
		if (upTo && upTo.index !== undefined) {
			const label = text.slice(upTo.index + upTo[0].length).trim();
			if (/\b(me|you|here|camera)\b/.test(label)) {
				return locomotionStep('forward')(parseDurationMs(text));
			}
			const anchor = label ? resolveSceneAnchor(label) : null;
			if (anchor) return { kind: 'goto', action: 'goto', anchorId: anchor.id };
			return null;
		}
	}

	// "Sit on the chair" is two steps: travel, then sit with that seat height.
	// An unknown seat degrades to sitting in place.
	if (/\bsit\b|座/.test(text)) {
		const label = anchorLabelAfter(text, /\bsit\b[^a-z]*\b(?:on|in)\b/);
		if (label) {
			const anchor = resolveSceneAnchor(label);
			if (anchor) {
				return [
					{ kind: 'goto', action: 'goto', anchorId: anchor.id },
					{ kind: 'procedural', action: 'sit', anchorId: anchor.id }
				];
			}
		}
		return { kind: 'procedural', action: 'sit' };
	}
	if (/\bstand\b|立/.test(text)) return { kind: 'procedural', action: 'stand' };

	if (RETURN_HOME_RE.test(text)) return { kind: 'return_home', action: 'return_home' };
	if (FACE_CAMERA_RE.test(text)) return { kind: 'face_camera', action: 'face_camera' };
	if (/\bturn\b|\bspin\b|回って/.test(text)) {
		return { kind: 'turn', action: 'turn', direction: findTurnDirection(text) ?? 'back' };
	}

	// Walk/run accept a bare verb ("walk" steps forward); move/go are too
	// chatty for that ("go on", "move along") and need a direction.
	if (/\bwalk\b|\brun\b|歩|走/.test(text)) {
		const action: LocomotionActionName = /\brun\b|走/.test(text) ? 'run' : 'walk';
		return locomotionStep(findWalkDirection(text) ?? 'forward', action)(parseDurationMs(text));
	}
	if (/\bmove\b|\bgo\b/.test(text)) {
		const direction = findWalkDirection(text);
		if (direction) return locomotionStep(direction)(parseDurationMs(text));
	}
	for (const { re, action } of VERBS) {
		if (re.test(text)) return actionStep(action);
	}
	// "walk left then right": a bare direction continues the previous
	// locomotion, preserving walk vs run.
	if (inheritLocomotion) {
		const direction = findWalkDirection(text);
		if (direction) return locomotionStep(direction, inheritLocomotion)(parseDurationMs(text));
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
		const last = steps.length > 0 ? steps[steps.length - 1] : null;
		const inheritLocomotion: LocomotionActionName | null =
			last && last.kind === 'walk' && (last.action === 'walk' || last.action === 'run')
				? last.action
				: null;
		const parsed = parseSegment(cleaned, inheritLocomotion);
		if (parsed) {
			const list = Array.isArray(parsed) ? parsed : [parsed];
			for (const step of list) {
				if (steps.length < MAX_PLAN_STEPS) steps.push(step);
			}
		} else {
			leftovers.push(raw.trim());
		}
	}
	if (steps.length === 0) return null;
	const plan: AvatarCommandPlan = { steps, pureAvatarCommand: leftovers.length === 0 };
	if (leftovers.length > 0) plan.remainingText = leftovers.join(' ').trim();
	return plan;
}

// True when the message explicitly asks the avatar to move: a locomotion
// verb plus a direction word (or approach phrasing) in the same segment.
// The turn uses it to let a matching model locomotion cue through the
// gesture gate — without it every model walk is rejected as not-explicit
// while the model's own text claims it moved.
export function isExplicitLocomotionAsk(message: string): boolean {
	if (typeof message !== 'string' || message.length === 0) return false;
	for (const raw of message.split(SEGMENT_SPLIT_RE)) {
		const text = raw.toLowerCase();
		if (!LOCOMOTION_VERB_RE.test(text)) continue;
		// Bare "walk"/"run" steps forward (parser default), so it counts
		// without a direction; "move"/"go" stay direction-gated ("go on").
		if (/\bwalk\b|\brun\b|歩|走/.test(text)) return true;
		if (findWalkDirection(text)) return true;
		if (/\bup\s+to\b/.test(text)) return true;
	}
	return false;
}
