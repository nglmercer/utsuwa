import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
	approachWeight,
	blendTemporaryFace,
	clamp01,
	directTemporaryTarget,
	emptyWeights,
	isEmotionalExpression,
	isProtectedChannel,
	moodToExpressionWeights,
	reactionEnvelope,
	resolveExpressionName,
	resolveTargets,
	EMOTIONAL_EXPRESSIONS,
	EXPRESSION_CANDIDATES,
	type ExpressionWeights
} from './facial-expressions.ts';
import type { Emotion } from '$lib/types/character';

const ALL_MOODS: Emotion[] = [
	'happy',
	'playful',
	'affectionate',
	'excited',
	'sad',
	'melancholy',
	'frustrated',
	'content',
	'flustered',
	'curious',
	'anxious',
	'neutral'
];

function total(weights: ExpressionWeights): number {
	return Object.values(weights).reduce((sum, w) => sum + w, 0);
}

describe('moodToExpressionWeights', () => {
	it('maps every Utsuwa mood to the specified blend', () => {
		const cases: Array<{ mood: Emotion; expect: Partial<ExpressionWeights>; absent: (keyof ExpressionWeights)[] }> = [
			{ mood: 'happy', expect: { happy: 1 }, absent: ['angry', 'sad'] },
			{ mood: 'playful', expect: { happy: 0.8, surprised: 0.25 }, absent: ['angry', 'sad'] },
			{ mood: 'affectionate', expect: { happy: 0.7, relaxed: 0.5 }, absent: ['angry'] },
			{ mood: 'excited', expect: { happy: 1, surprised: 0.35 }, absent: ['sad'] },
			{ mood: 'sad', expect: { sad: 1 }, absent: ['happy'] },
			{ mood: 'melancholy', expect: { sad: 0.6 }, absent: ['happy', 'angry'] },
			{ mood: 'frustrated', expect: { angry: 1 }, absent: ['happy', 'sad'] },
			{ mood: 'content', expect: { relaxed: 1 }, absent: ['angry'] },
			{ mood: 'flustered', expect: { surprised: 0.8, happy: 0.3 }, absent: ['angry', 'sad'] },
			{ mood: 'curious', expect: { surprised: 0.5 }, absent: ['angry', 'sad'] },
			{ mood: 'anxious', expect: { surprised: 0.7, sad: 0.4 }, absent: ['happy'] },
			{ mood: 'neutral', expect: { neutral: 0.6 }, absent: ['angry', 'sad'] }
		];
		assert.equal(cases.length, ALL_MOODS.length);
		for (const { mood, expect, absent } of cases) {
			const weights = moodToExpressionWeights({ primary: mood, intensity: 100 });
			for (const [expression, weight] of Object.entries(expect)) {
				assert.equal(weights[expression as keyof ExpressionWeights], weight, `${mood}.${expression}`);
			}
			for (const expression of absent) {
				assert.equal(weights[expression], 0, `${mood}.${expression} should be absent`);
			}
		}
	});

	it('keeps every weight inside 0..1 for all moods and intensities', () => {
		for (const primary of ALL_MOODS) {
			for (const intensity of [-50, 0, 1, 25, 50, 75, 99, 100, 150, Number.NaN]) {
				const weights = moodToExpressionWeights({ primary, intensity });
				for (const expression of EMOTIONAL_EXPRESSIONS) {
					const w = weights[expression];
					assert.ok(w >= 0 && w <= 1, `${primary}@${intensity}.${expression}=${w}`);
				}
			}
		}
	});

	it('scales strength with intensity', () => {
		const faint = moodToExpressionWeights({ primary: 'happy', intensity: 25 });
		const half = moodToExpressionWeights({ primary: 'happy', intensity: 50 });
		const full = moodToExpressionWeights({ primary: 'happy', intensity: 100 });
		assert.equal(faint.happy, 0.25);
		assert.equal(half.happy, 0.5);
		assert.equal(full.happy, 1);
		assert.ok(total(faint) < total(half) && total(half) < total(full));
	});

	it('produces a blank face at zero intensity', () => {
		assert.equal(total(moodToExpressionWeights({ primary: 'excited', intensity: 0 })), 0);
		assert.deepEqual(emptyWeights(), {
			happy: 0,
			angry: 0,
			sad: 0,
			relaxed: 0,
			surprised: 0,
			neutral: 0
		});
	});
});

describe('expression resolution', () => {
	const FULL_RIG = ['happy', 'angry', 'sad', 'relaxed', 'surprised', 'neutral', 'blink'];

	it('resolves every channel on a full rig', () => {
		const targets = resolveTargets(moodToExpressionWeights({ primary: 'anxious', intensity: 100 }), FULL_RIG);
		const byName = new Map(targets.map((t) => [t.name, t.weight]));
		assert.equal(byName.get('surprised'), 0.7);
		assert.equal(byName.get('sad'), 0.4);
	});

	it('falls back to candidate presets on minimal rigs', () => {
		assert.equal(resolveExpressionName('happy', ['joy', 'blink']), 'joy');
		assert.equal(resolveExpressionName('relaxed', ['neutral']), 'neutral');
		assert.equal(resolveExpressionName('neutral', ['relaxed']), 'relaxed');
		assert.equal(resolveExpressionName('happy', ['HAPPY']), 'HAPPY');
	});

	it('skips channels with no available preset', () => {
		assert.equal(resolveExpressionName('surprised', ['happy', 'sad']), null);
		const targets = resolveTargets(moodToExpressionWeights({ primary: 'curious', intensity: 100 }), ['happy']);
		assert.deepEqual(targets, []);
	});

	it('merges channels that resolve to the same preset by max weight', () => {
		// relaxed → neutral and neutral → neutral collide on this rig.
		const weights: ExpressionWeights = {
			happy: 0,
			angry: 0,
			sad: 0,
			relaxed: 0.4,
			surprised: 0,
			neutral: 0.7
		};
		const targets = resolveTargets(weights, ['neutral']);
		assert.deepEqual(targets, [{ name: 'neutral', weight: 0.7 }]);
	});

	it('never resolves to protected channels', () => {
		assert.ok(isProtectedChannel('blink'));
		assert.ok(isProtectedChannel('Blink'));
		assert.ok(isProtectedChannel('eyeBlinkLeft'));
		assert.ok(isProtectedChannel('aa'));
		assert.ok(isProtectedChannel('jawOpen'));
		assert.ok(!isProtectedChannel('happy'));
		assert.ok(!isProtectedChannel('shy'));
		assert.ok(isEmotionalExpression('happy'));
		assert.ok(isEmotionalExpression('Surprised'));
		assert.ok(!isEmotionalExpression('shy'));
		assert.ok(!isEmotionalExpression('blink'));
		const targets = resolveTargets(
			{ happy: 1, angry: 0, sad: 0, relaxed: 0, surprised: 0, neutral: 0 },
			['happy', 'blink']
		);
		assert.deepEqual(targets, [{ name: 'happy', weight: 1 }]);
	});

	it('covers every emotional channel with at least one candidate', () => {
		for (const expression of EMOTIONAL_EXPRESSIONS) {
			assert.ok(EXPRESSION_CANDIDATES[expression].length > 0);
		}
	});

	it('resolves every mood to at least one expression on a stock VRoid rig', () => {
		const VROID_PRESETS = ['happy', 'angry', 'sad', 'relaxed', 'surprised', 'neutral'];
		for (const primary of ALL_MOODS) {
			for (const intensity of [50, 100]) {
				const targets = resolveTargets(moodToExpressionWeights({ primary, intensity }), VROID_PRESETS);
				assert.ok(targets.length > 0, `${primary}@${intensity} resolved to nothing`);
				for (const target of targets) {
					assert.ok(VROID_PRESETS.includes(target.name), `${primary} resolved to ${target.name}`);
					assert.ok(target.weight > 0 && target.weight <= 1, `${primary}.${target.name}=${target.weight}`);
				}
			}
			assert.deepEqual(
				resolveTargets(moodToExpressionWeights({ primary, intensity: 0 }), VROID_PRESETS),
				[],
				`${primary}@0 should stay silent`
			);
		}
	});

	it('degrades VRM 0.x blendshapes to their closest preset', () => {
		const VRM0X_PRESETS = ['joy', 'sorrow', 'anger', 'fun'];
		assert.equal(resolveExpressionName('happy', VRM0X_PRESETS), 'joy');
		assert.equal(resolveExpressionName('sad', VRM0X_PRESETS), 'sorrow');
		assert.equal(resolveExpressionName('angry', VRM0X_PRESETS), 'anger');
		// No 0.x counterpart exists for these: they stay silent instead of failing.
		for (const channel of ['relaxed', 'surprised', 'neutral'] as const) {
			assert.equal(resolveExpressionName(channel, VRM0X_PRESETS), null, channel);
		}
		assert.deepEqual(
			resolveTargets(moodToExpressionWeights({ primary: 'sad', intensity: 100 }), VRM0X_PRESETS),
			[{ name: 'sorrow', weight: 1 }]
		);
		assert.deepEqual(
			resolveTargets(moodToExpressionWeights({ primary: 'frustrated', intensity: 100 }), VRM0X_PRESETS),
			[{ name: 'anger', weight: 1 }]
		);
	});

	it('degrades unknown model presets to silence without throwing', () => {
		const weird = ['shy_custom', 'Blink', '', 'HAPPY_CUSTOM'];
		assert.equal(resolveExpressionName('surprised', weird), null);
		assert.deepEqual(
			resolveTargets(moodToExpressionWeights({ primary: 'excited', intensity: 100 }), weird),
			[]
		);
		assert.deepEqual(
			resolveTargets(moodToExpressionWeights({ primary: 'happy', intensity: 100 }), []),
			[]
		);
		assert.equal(directTemporaryTarget({ name: 'shy', weight: 1 }, weird), null);
		assert.equal(directTemporaryTarget({ name: 'shy', weight: 1 }, []), null);
	});

	it('skips non-string entries in the available list instead of throwing', () => {
		const dirty = ['joy', 42, null, undefined, { name: 'happy' }] as unknown as string[];
		assert.equal(resolveExpressionName('happy', dirty), 'joy');
		assert.equal(resolveExpressionName('sad', dirty), null);
		assert.deepEqual(
			resolveTargets(moodToExpressionWeights({ primary: 'happy', intensity: 100 }), dirty),
			[{ name: 'joy', weight: 1 }]
		);
		assert.equal(
			directTemporaryTarget({ name: 'shy', weight: 0.5 }, [...dirty, 'SHY'])?.name,
			'SHY'
		);
		assert.ok(!isProtectedChannel(42 as unknown as string));
	});
});

describe('protected channels', () => {
	const PROTECTED = [
		'blink',
		'blinkLeft',
		'blinkRight',
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
	];

	it('recognizes every protected channel in any case', () => {
		for (const name of PROTECTED) {
			assert.ok(isProtectedChannel(name), name);
			assert.ok(isProtectedChannel(name.toUpperCase()), name.toUpperCase());
			assert.ok(isProtectedChannel(name[0].toUpperCase() + name.slice(1)), name);
		}
		for (const name of [...EMOTIONAL_EXPRESSIONS, 'shy']) {
			assert.ok(!isProtectedChannel(name), `${name} must not be protected`);
		}
	});

	it('never emits a protected channel from the mood path', () => {
		const rig = [...PROTECTED, 'Blink', 'JAWOPEN', 'happy', 'sad'];
		for (const primary of ALL_MOODS) {
			const targets = resolveTargets(moodToExpressionWeights({ primary, intensity: 100 }), rig);
			for (const target of targets) {
				assert.ok(!isProtectedChannel(target.name), `${primary} wrote ${target.name}`);
			}
		}
	});

	it('drops temporaries that name a protected channel, in any case', () => {
		const baseline = moodToExpressionWeights({ primary: 'sad', intensity: 80 });
		for (const name of [...PROTECTED, 'Blink', 'JAWOPEN', 'AA', 'EyeBlinkRight']) {
			assert.deepEqual(blendTemporaryFace(baseline, { name, weight: 1 }), baseline, name);
			assert.equal(directTemporaryTarget({ name, weight: 1 }, [name, 'happy', 'shy']), null, name);
		}
	});
});

describe('weight hygiene', () => {
	it('drives at most two channels per mood', () => {
		for (const primary of ALL_MOODS) {
			for (const intensity of [50, 100]) {
				const weights = moodToExpressionWeights({ primary, intensity });
				const active = EMOTIONAL_EXPRESSIONS.filter((expression) => weights[expression] > 0);
				assert.ok(active.length <= 2, `${primary}@${intensity} drives ${active.join(',')}`);
			}
		}
	});

	it('keeps neutral exclusive: no emotional channel stacked under it', () => {
		const weights = moodToExpressionWeights({ primary: 'neutral', intensity: 100 });
		for (const expression of ['happy', 'angry', 'sad', 'relaxed', 'surprised'] as const) {
			assert.equal(weights[expression], 0, `neutral.${expression}`);
		}
		assert.ok(weights.neutral > 0);
	});
});

describe('approachWeight', () => {
	it('converges toward the target without overshooting', () => {
		let current = 0;
		for (let i = 0; i < 120; i++) {
			current = approachWeight(current, 1, 1 / 60, 6);
			assert.ok(current >= 0 && current <= 1);
		}
		assert.ok(current > 0.99, `expected convergence, got ${current}`);
	});

	it('is roughly frame-rate independent', () => {
		let fast = 0;
		for (let i = 0; i < 120; i++) fast = approachWeight(fast, 1, 1 / 120, 4);
		let slow = 0;
		for (let i = 0; i < 30; i++) slow = approachWeight(slow, 1, 1 / 30, 4);
		assert.ok(Math.abs(fast - slow) < 0.05, `120Hz=${fast} 30Hz=${slow}`);
	});

	it('holds still on degenerate input', () => {
		assert.equal(approachWeight(0.5, 1, 0, 6), 0.5);
		assert.equal(approachWeight(0.5, 1, -1, 6), 0.5);
		assert.equal(approachWeight(0.5, 1, 1 / 60, 0), 0.5);
	});
});

describe('temporary faces', () => {
	const sadMood = moodToExpressionWeights({ primary: 'sad', intensity: 80 });

	it('overlays a reaction and fades back to the mood baseline', () => {
		const peak = blendTemporaryFace(sadMood, { name: 'happy', weight: 1 });
		assert.equal(peak.happy, 1);
		assert.equal(peak.sad, 0);
		const mid = blendTemporaryFace(sadMood, { name: 'happy', weight: 0.5 });
		assert.equal(mid.happy, 0.5);
		assert.ok(Math.abs(mid.sad - 0.4) < 1e-9);
		const gone = blendTemporaryFace(sadMood, { name: 'happy', weight: 0 });
		assert.deepEqual(gone, sadMood);
		const none = blendTemporaryFace(sadMood, null);
		assert.deepEqual(none, sadMood);
	});

	it('dims the mood under custom presets without touching semantic weights', () => {
		const blended = blendTemporaryFace(sadMood, { name: 'shy', weight: 0.5 });
		assert.ok(Math.abs(blended.sad - 0.4) < 1e-9);
		assert.equal(blended.happy, 0);
		const direct = directTemporaryTarget({ name: 'shy', weight: 0.5 }, ['shy', 'happy']);
		assert.deepEqual(direct, { name: 'shy', weight: 0.5 });
	});

	it('routes emotional temporaries through the blend, not the direct path', () => {
		assert.equal(directTemporaryTarget({ name: 'happy', weight: 1 }, ['happy']), null);
		assert.equal(directTemporaryTarget({ name: 'shy', weight: 1 }, ['happy']), null);
	});

	it('ignores protected channels in temporaries', () => {
		assert.deepEqual(blendTemporaryFace(sadMood, { name: 'blink', weight: 1 }), sadMood);
		assert.equal(directTemporaryTarget({ name: 'jawOpen', weight: 1 }, ['jawOpen']), null);
	});
});

describe('reactionEnvelope', () => {
	it('attacks fast, holds, then releases to zero', () => {
		assert.equal(reactionEnvelope(0, 2), 0);
		assert.ok(reactionEnvelope(0.1, 2) > 0 && reactionEnvelope(0.1, 2) < 1);
		assert.equal(reactionEnvelope(0.5, 2), 1);
		const release = reactionEnvelope(1.5, 2);
		assert.ok(release > 0 && release < 1);
		assert.equal(reactionEnvelope(2, 2), 0);
		assert.equal(reactionEnvelope(3, 2), 0);
		assert.equal(reactionEnvelope(1, 0), 0);
	});
});

describe('clamp01', () => {
	it('clamps and sanitizes', () => {
		assert.equal(clamp01(-1), 0);
		assert.equal(clamp01(2), 1);
		assert.equal(clamp01(0.5), 0.5);
		assert.equal(clamp01(Number.NaN), 0);
		assert.equal(clamp01(Infinity), 0);
	});
});
