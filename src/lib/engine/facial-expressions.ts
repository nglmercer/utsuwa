// Mood → VRM facial-expression weights. Pure and dependency-free (no Svelte,
// no Three.js): given a mood it produces semantic expression weights, and
// given the model's available expressions it resolves those to concrete VRM
// expression names with fallbacks. The VRM component owns the per-frame
// smoothing and the actual expressionManager writes.
import type { Emotion } from '$lib/types/character';

// The only emotional channels the mood system drives. Blink, lip-sync
// visemes, jawOpen, and any other mouth animation channels are never touched.
export type EmotionalExpression = 'happy' | 'angry' | 'sad' | 'relaxed' | 'surprised' | 'neutral';

export const EMOTIONAL_EXPRESSIONS: EmotionalExpression[] = [
	'happy',
	'angry',
	'sad',
	'relaxed',
	'surprised',
	'neutral'
];

export function isEmotionalExpression(value: unknown): value is EmotionalExpression {
	return (
		typeof value === 'string' &&
		(EMOTIONAL_EXPRESSIONS as readonly string[]).includes(value.toLowerCase())
	);
}

// Candidate VRM expression names per semantic channel, in preference order.
// Models vary (VRoid, VRM 0.x samples, custom rigs), so each channel degrades
// to the closest available preset instead of failing.
export const EXPRESSION_CANDIDATES: Record<EmotionalExpression, string[]> = {
	happy: ['happy', 'joy', 'smile'],
	angry: ['angry'],
	sad: ['sad'],
	relaxed: ['relaxed', 'neutral'],
	surprised: ['surprised'],
	neutral: ['neutral', 'relaxed']
};

// Channels the mood system must never write, even if a temporary request
// names one: blinking, lip-sync visemes, and jaw motion belong to their own
// frame-loop systems.
const PROTECTED_CHANNELS = new Set([
	'blink',
	'eyeBlinkLeft',
	'eyeBlinkRight',
	'aa',
	'ee',
	'ih',
	'oh',
	'ou',
	'a',
	'i',
	'u',
	'e',
	'o',
	'jawOpen'
]);

export function isProtectedChannel(name: string): boolean {
	if (PROTECTED_CHANNELS.has(name)) return true;
	const lower = name.toLowerCase();
	for (const channel of PROTECTED_CHANNELS) {
		if (channel.toLowerCase() === lower) return true;
	}
	return false;
}

// Base weights at full mood intensity (0-100 scale in, see moodToExpressionWeights).
// Each mood is a blend of at most two channels so the face stays readable.
const MOOD_EXPRESSION_MAP: Record<Emotion, Partial<Record<EmotionalExpression, number>>> = {
	happy: { happy: 1 },
	playful: { happy: 0.8, surprised: 0.25 },
	affectionate: { happy: 0.7, relaxed: 0.5 },
	excited: { happy: 1, surprised: 0.35 },
	sad: { sad: 1 },
	melancholy: { sad: 0.6 },
	frustrated: { angry: 1 },
	content: { relaxed: 1 },
	flustered: { surprised: 0.8, happy: 0.3 },
	curious: { surprised: 0.5 },
	anxious: { surprised: 0.7, sad: 0.4 },
	neutral: { neutral: 0.6 }
};

export type ExpressionWeights = Record<EmotionalExpression, number>;

function zeroWeights(): ExpressionWeights {
	return { happy: 0, angry: 0, sad: 0, relaxed: 0, surprised: 0, neutral: 0 };
}

// A blank face: every channel at rest. Used when a photo pose owns the face
// exclusively and the mood must contribute nothing.
export function emptyWeights(): ExpressionWeights {
	return zeroWeights();
}

export function clamp01(value: number): number {
	if (!Number.isFinite(value)) return 0;
	return Math.min(1, Math.max(0, value));
}

// Map a mood to semantic expression weights. Intensity (0-100) scales the
// blend linearly: a faint mood is a faint face, full intensity is the full
// blend. Out-of-range intensities are clamped, never wrapped.
export function moodToExpressionWeights(mood: { primary: Emotion; intensity: number }): ExpressionWeights {
	const weights = zeroWeights();
	const strength = clamp01(mood.intensity / 100);
	if (strength <= 0) return weights;
	const blend = MOOD_EXPRESSION_MAP[mood.primary];
	if (!blend) return weights;
	for (const [expression, base] of Object.entries(blend) as Array<[EmotionalExpression, number]>) {
		weights[expression] = clamp01(base * strength);
	}
	return weights;
}

// Resolve one semantic channel to a concrete VRM expression name available on
// the model. Exact match first, then case-insensitive; nothing matches → null
// so the caller skips the channel gracefully.
export function resolveExpressionName(
	expression: EmotionalExpression,
	available: readonly string[]
): string | null {
	const candidates = EXPRESSION_CANDIDATES[expression] ?? [];
	for (const candidate of candidates) {
		if (available.includes(candidate)) return candidate;
	}
	const lowered = available.map((name) => name.toLowerCase());
	for (const candidate of candidates) {
		const index = lowered.indexOf(candidate.toLowerCase());
		if (index !== -1) return available[index];
	}
	return null;
}

export interface ResolvedTarget {
	// Concrete VRM expression name to write.
	name: string;
	weight: number;
}

// Resolve semantic weights to concrete per-model targets. Channels with no
// available preset are dropped; when two channels resolve to the same preset
// (e.g. relaxed → neutral on a minimal rig) the stronger weight wins.
export function resolveTargets(
	weights: ExpressionWeights,
	available: readonly string[]
): ResolvedTarget[] {
	const merged = new Map<string, number>();
	for (const expression of EMOTIONAL_EXPRESSIONS) {
		const weight = weights[expression];
		if (!(weight > 0)) continue;
		const name = resolveExpressionName(expression, available);
		if (!name || isProtectedChannel(name)) continue;
		const prev = merged.get(name);
		if (prev === undefined || weight > prev) merged.set(name, clamp01(weight));
	}
	return [...merged.entries()].map(([name, weight]) => ({ name, weight }));
}

// Frame-rate-independent exponential approach toward a target weight:
// current += (target - current) * (1 - exp(-speed * delta)). Higher speed
// snaps faster; speed ~6 reaches ~99% in under a second.
export function approachWeight(current: number, target: number, delta: number, speed: number): number {
	if (!(delta > 0) || !(speed > 0)) return clamp01(current);
	const alpha = 1 - Math.exp(-speed * delta);
	return clamp01(current + (clamp01(target) - clamp01(current)) * alpha);
}

export interface TemporaryFace {
	// Raw VRM expression name (usually one of the emotional six, but custom
	// presets like 'shy' from tap reactions are allowed).
	name: string;
	// Current envelope weight 0..1 (attack/decay handled by the caller).
	weight: number;
}

// Blend a temporary reaction over the mood baseline: the mood fades out in
// proportion to the reaction's envelope and the reaction takes over, so a
// smile flashes on top of sadness and melts back into it as weight → 0.
// Returns semantic weights; the caller still resolves + smooths them.
export function blendTemporaryFace(
	mood: ExpressionWeights,
	temporary: TemporaryFace | null
): ExpressionWeights {
	if (!temporary || !(temporary.weight > 0) || isProtectedChannel(temporary.name)) {
		return { ...mood };
	}
	const envelope = clamp01(temporary.weight);
	const blended = zeroWeights();
	const lower = temporary.name.toLowerCase();
	for (const expression of EMOTIONAL_EXPRESSIONS) {
		let weight = mood[expression] * (1 - envelope);
		if (expression === lower) weight = Math.max(weight, envelope);
		blended[expression] = clamp01(weight);
	}
	return blended;
}

// Non-emotional temporary names (e.g. 'shy') bypass the semantic blend and
// are written directly with the envelope weight. Returns null when the name
// is emotional (handled by blendTemporaryFace) or protected (never written).
export function directTemporaryTarget(
	temporary: TemporaryFace | null,
	available: readonly string[]
): ResolvedTarget | null {
	if (!temporary || !(temporary.weight > 0)) return null;
	if (isEmotionalExpression(temporary.name) || isProtectedChannel(temporary.name)) return null;
	const match = available.find((name) => name === temporary.name);
	const name = match ?? available.find((name) => name.toLowerCase() === temporary.name.toLowerCase());
	if (!name) return null;
	return { name, weight: clamp01(temporary.weight) };
}

// Envelope for a transient reaction: quick attack, hold, long release.
// t and duration are in the same unit (seconds in the frame loop).
export function reactionEnvelope(t: number, duration: number): number {
	if (!(duration > 0)) return 0;
	const progress = t / duration;
	if (progress <= 0) return 0;
	if (progress >= 1) return 0;
	if (progress < 0.15) return progress / 0.15;
	if (progress < 0.5) return 1;
	return 1 - (progress - 0.5) / 0.5;
}
