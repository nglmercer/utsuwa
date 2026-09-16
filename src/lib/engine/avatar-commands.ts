// Direct avatar commands: recognize explicit user requests ("jump!", "walk
// left") locally so physical responsiveness never depends on whether the chat
// model remembered to emit the right gesture_cue. Pure and dependency-free.
// English plus compact Japanese; word-boundaried to avoid "jumper"/"dancer".
import type { GestureCue } from './avatar-actions.ts';

interface CommandPattern {
	re: RegExp;
	cue: () => GestureCue;
}

const WALK_DURATION_MS = 1200;

const PATTERNS: CommandPattern[] = [
	// Walk variants first: "walk left" must win before any bare verb below.
	{
		re: /walk\b[^.!?]*\bleft\b|left[^.!?]*\bwalk\b|左\s*(に|へ)\s*(歩|すすむ|進む)|左に来て/,
		cue: () => ({ type: 'locomotion', action: 'walk', direction: 'left', durationMs: WALK_DURATION_MS })
	},
	{
		re: /walk\b[^.!?]*\bright\b|right[^.!?]*\bwalk\b|右\s*(に|へ)\s*(歩|すすむ|進む)|右に来て/,
		cue: () => ({ type: 'locomotion', action: 'walk', direction: 'right', durationMs: WALK_DURATION_MS })
	},
	{
		re: /walk\b[^.!?]*\b(forward|forwards|ahead)\b|(come|step)\s+(here|closer|forward)|前に?(進め|歩いて|来て)/,
		cue: () => ({ type: 'locomotion', action: 'walk', direction: 'forward', durationMs: WALK_DURATION_MS })
	},
	{
		re: /walk\b[^.!?]*\bback(wards?)?\b|step\s+back|後ろ\s*(に|へ)?\s*(下がって|歩いて)/,
		cue: () => ({ type: 'locomotion', action: 'walk', direction: 'back', durationMs: WALK_DURATION_MS })
	},
	{ re: /\bjump(ing|ed)?\b|跳んで|ジャンプ/, cue: () => ({ type: 'animation', action: 'jump' }) },
	{ re: /\bwav(e|ing)\b|手を振/, cue: () => ({ type: 'animation', action: 'wave' }) },
	{ re: /\bnod(ing|ded)?\b|うなず|頷/, cue: () => ({ type: 'animation', action: 'nod' }) },
	{
		re: /shake\s+your\s+head|首を(横に)?振/,
		cue: () => ({ type: 'animation', action: 'shake_head' })
	},
	{ re: /\bbow(ing|ed)?\b|お辞儀/, cue: () => ({ type: 'animation', action: 'bow' }) },
	{ re: /\bdance[sd]?\b|\bdancing\b|踊/, cue: () => ({ type: 'animation', action: 'dance' }) },
	{ re: /\bcelebrate[sd]?\b|お祝い/, cue: () => ({ type: 'animation', action: 'celebrate' }) },
	{ re: /\bshrug(ged|s)?\b|肩をすくめ/, cue: () => ({ type: 'animation', action: 'shrug' }) },
	// Bare "walk" with no direction reads as a step forward.
	{ re: /\bwalk\b|歩いて/, cue: () => ({ type: 'locomotion', action: 'walk', direction: 'forward', durationMs: WALK_DURATION_MS }) }
];

// A direct command if any pattern matches. Returns the cue to stage (the
// caller treats it as an explicit request); null when the message carries no
// avatar command.
export function detectExplicitAvatarCommand(message: string): GestureCue | null {
	if (typeof message !== 'string' || message.length === 0) return null;
	const text = message.toLowerCase();
	for (const pattern of PATTERNS) {
		if (pattern.re.test(text)) return pattern.cue();
	}
	return null;
}
