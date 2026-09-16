// Expression controller: owns the face state machines — tap-reaction
// pulses, transient-face staging and arbitration, the mood face, photo
// held expressions, blinking, and viseme application. The Svelte
// component owns the THREE scene, the stores, and the frame loop; this
// controller owns the face state and the per-phase integration, behind an
// injected environment (humanoid, nudge sink, store readers) so it stays
// testable under node.
//
// Frame order is part of the contract (the loop calls each phase at the
// same position the inline code ran): pulses land right after the mixer
// update and before motion; the mood face resolves before `vrm.update()`;
// blink and visemes apply after the expression manager update. Lipsync
// analysis itself stays outside: the caller samples the analyzer and
// passes visemes in.
import {
	approachWeight,
	blendTemporaryFace,
	directTemporaryTarget,
	emptyWeights,
	moodToExpressionWeights,
	reactionEnvelope,
	resolveTargets
} from '../engine/facial-expressions.ts';
import { pickReaction, stageTier, type TouchZone } from '../engine/photo-reactions.ts';
import type { PoseBone, PoseNudge } from './procedural-pose-controller.ts';
import type { Emotion } from '$lib/types/character';
import type { RelationshipStage } from '$lib/types/character';

// Smoothing speed for mood-face weights (exponential approach; ~6 reaches
// ~99% of a new target in under a second, frame-rate independent).
export const MOOD_FACE_SPEED = 6;

export const REACTION_REPEAT_WINDOW_MS = 4000;

export interface ExpressionHumanoid {
	getNormalizedBoneNode(name: string): PoseBone | null;
}

export interface FaceExpressionManager {
	setValue: (name: string, value: number) => void;
	update: () => void;
}

export interface MoodState {
	primary: Emotion;
	intensity: number;
	causes: string[];
}

export interface ExpressionEnvironment {
	getHumanoid: () => ExpressionHumanoid | null;
	/** Additive bone-offset sink (unwound by the renderer each frame). */
	nudge: PoseNudge;
	getRelationshipStage: () => RelationshipStage;
	shouldTalk: () => boolean;
	/** Face flash through the shared arbitration (never a direct write). */
	requestFace: (expression: string, intensity: number, durationMs: number) => void;
	getExpressionManager: () => FaceExpressionManager | null;
	getAvailableExpressionNames: () => string[];
}

export interface StagedExpression {
	expression: string;
	intensity: number;
	durationMs: number;
	seq: number;
}

export interface FaceFrame {
	expressionManager: FaceExpressionManager;
	availableExpressions: string[];
	mood: MoodState;
	photoActive: boolean;
}

export interface VoiceFrame {
	expressionManager: FaceExpressionManager;
	emotePlaying: boolean;
	visemes: { aa: number; ee: number; ih: number; oh: number; ou: number };
}

interface ReactionPulse {
	bone: PoseBone;
	t: number;
	duration: number;
	magnitude: number;
	direction: number;
}

interface TransientFace {
	name: string;
	weight: number;
	t: number;
	duration: number;
	seq: number;
}

// Best-effort expression write: presets were resolved against the model's
// snapshot, so a throw only happens on a mid-frame model swap.
function safeSetFace(
	manager: FaceExpressionManager | null | undefined,
	name: string,
	value: number
): void {
	if (!manager) return;
	try {
		manager.setValue(name, value);
	} catch {
		// Unknown preset on this model; resolution already skipped it.
	}
}

export class ExpressionController {
	private readonly env: ExpressionEnvironment;
	private activePulses: ReactionPulse[] = [];
	// In-flight transient face (tap flash, emote grin, AI cue), arbitrated
	// against the mood face in updateFace.
	private transientFace: TransientFace | null = null;
	// Smoothed weights the mood system owns (emotional presets + custom
	// transient names like 'shy'). Never includes blink/viseme/jaw.
	private readonly moodWeights = new Map<string, number>();
	private heldExpression: string | null = null;
	private readonly recentTaps = { zone: null as TouchZone | null, at: 0, count: 0 };
	private blinkTimer = 0;
	private nextBlinkTime = Math.random() * 4 + 2; // 2-6 seconds
	private isBlinking = false;
	private blinkProgress = 0;

	constructor(env: ExpressionEnvironment) {
		this.env = env;
	}

	// Stage a transient face (or null to release the in-flight one into
	// its fade instead of snapping back to mood).
	stageExpression(request: StagedExpression | null): void {
		if (!request) {
			if (this.transientFace) {
				this.transientFace.t = Math.max(
					this.transientFace.t,
					this.transientFace.duration * 0.7
				);
			}
			return;
		}
		if (this.transientFace && this.transientFace.seq === request.seq) return;
		this.transientFace = {
			name: request.expression,
			weight: request.intensity,
			t: 0,
			duration: Math.max(0.1, request.durationMs / 1000),
			seq: request.seq
		};
	}

	// Tap reactions: an expression flash plus a decaying rotation nudge
	// whose motion the spring bones inherit. Repeat taps inside the window
	// escalate. Pulses overlap instead of replacing each other
	// (replacement snapped the active nudge to zero, which read as a jump
	// on rapid taps).
	stageReaction(zone: TouchZone): void {
		const humanoid = this.env.getHumanoid();
		if (!humanoid) return;
		const now = performance.now();
		if (
			this.recentTaps.zone === zone &&
			now - this.recentTaps.at < REACTION_REPEAT_WINDOW_MS
		) {
			this.recentTaps.count += 1;
		} else {
			this.recentTaps.count = 0;
		}
		this.recentTaps.zone = zone;
		this.recentTaps.at = now;
		const tier = stageTier(this.env.getRelationshipStage());
		const spec = pickReaction(zone, tier, this.recentTaps.count);
		// The face flash goes through the shared arbitration (no direct
		// expression writes here): it overlays the mood face, then melts.
		const available = this.env.getAvailableExpressionNames();
		const name = spec.expressions.find((candidate) => available.includes(candidate));
		if (name) {
			this.env.requestFace(name, spec.weight, 1800);
		}
		const bone =
			zone === 'head' || zone === 'face'
				? humanoid.getNormalizedBoneNode('head')
				: zone === 'shoulder'
					? (humanoid.getNormalizedBoneNode('upperChest') ??
						humanoid.getNormalizedBoneNode('chest'))
					: zone === 'torso'
						? humanoid.getNormalizedBoneNode('spine')
						: humanoid.getNormalizedBoneNode('hips');
		const fallback = humanoid.getNormalizedBoneNode('spine');
		const target = bone ?? fallback;
		if (target && this.activePulses.length < 4) {
			// Half strength while she is talking: the head is already
			// moving, and a full kick layered on that read as a jump.
			const talkScale = this.env.shouldTalk() ? 0.5 : 1;
			this.activePulses.push({
				bone: target,
				t: 0,
				duration: 0.9,
				magnitude: spec.impulse * talkScale,
				direction: Math.random() > 0.5 ? 1 : -1
			});
		}
	}

	// Held photo expression: applied exclusively, cleared on change. Tap
	// reactions layer a transient expression on top and restore this one.
	setPhotoExpression(active: boolean, name: string | null): void {
		const em = this.env.getExpressionManager();
		if (!em) return;
		if (this.heldExpression && this.heldExpression !== name) {
			em.setValue(this.heldExpression, 0);
			this.heldExpression = null;
		}
		if (active && name) {
			em.setValue(name, 1);
			this.heldExpression = name;
		}
	}

	// Tap reactions: decaying additive nudges layered over whatever the
	// mixer wrote, rendered this frame (so the body sways with the physics
	// instead of the solver and the render disagreeing, which read as
	// jitter during talking). Overlapping pulses sum.
	updatePulses(delta: number): void {
		if (this.activePulses.length === 0) return;
		const remaining: ReactionPulse[] = [];
		for (const pulse of this.activePulses) {
			pulse.t += delta;
			const progress = pulse.t / pulse.duration;
			if (progress >= 1) continue;
			// sin^2 has zero slope at both ends: eases in and out.
			const wave = Math.sin(progress * Math.PI);
			const envelope = wave * wave * Math.exp(-1.6 * progress);
			const angle = pulse.magnitude * 0.07 * envelope;
			this.env.nudge(pulse.bone, -angle * 0.4, angle * pulse.direction);
			remaining.push(pulse);
		}
		this.activePulses = remaining;
	}

	// Mood face + transient arbitration. Priority: held photo pose
	// (exclusive) > transient reaction (tap flash, emote grin, AI cue) >
	// persistent mood > resting face. Only emotional presets are written
	// here; blink, visemes, and jawOpen are owned by their own systems.
	updateFace(delta: number, frame: FaceFrame): void {
		const em = frame.expressionManager;
		if (this.transientFace) {
			this.transientFace.t += delta;
			if (this.transientFace.t >= this.transientFace.duration) {
				const done = this.transientFace;
				this.transientFace = null;
				if (done.name === this.heldExpression) {
					// The reaction borrowed the held expression; hand it
					// back whole.
					this.moodWeights.delete(done.name);
					safeSetFace(em, done.name, 1);
				}
				// Other names melt back into the mood through moodWeights.
			}
		}
		const temp =
			this.transientFace && this.transientFace.weight > 0
				? {
						name: this.transientFace.name,
						weight:
							this.transientFace.weight *
							reactionEnvelope(this.transientFace.t, this.transientFace.duration)
					}
				: null;
		const held = frame.photoActive ? this.heldExpression : null;
		const base = held ? emptyWeights() : moodToExpressionWeights(frame.mood);
		const blended = blendTemporaryFace(base, temp);
		const targets = new Map<string, number>();
		for (const target of resolveTargets(blended, frame.availableExpressions)) {
			targets.set(target.name, target.weight);
		}
		if (temp) {
			const direct = directTemporaryTarget(temp, frame.availableExpressions);
			if (direct) targets.set(direct.name, Math.max(targets.get(direct.name) ?? 0, direct.weight));
		}
		// Drive owned weights toward their targets.
		for (const [name, weight] of targets) {
			const next = approachWeight(this.moodWeights.get(name) ?? 0, weight, delta, MOOD_FACE_SPEED);
			this.moodWeights.set(name, next);
			safeSetFace(em, name, next);
		}
		// Decay anything without a target back to rest. A stale entry for
		// the held preset decays silently (the pose owns the actual value)
		// so exiting the pose ramps back up instead of snapping.
		for (const [name, current] of [...this.moodWeights]) {
			if (targets.has(name)) continue;
			const next = approachWeight(current, 0, delta, MOOD_FACE_SPEED);
			if (next <= 0.001) {
				this.moodWeights.delete(name);
				if (name !== held) safeSetFace(em, name, 0);
			} else {
				this.moodWeights.set(name, next);
				if (name !== held) safeSetFace(em, name, next);
			}
		}
	}

	// Blinking (suppressed while an emote plays), then the expression
	// manager update, then lip-sync visemes across VRM 1.0, 0.x, and ARKit
	// naming conventions.
	updateBlinkAndVoice(delta: number, frame: VoiceFrame): void {
		const em = frame.expressionManager;
		const setExpression = (name: string, value: number) => {
			try {
				em.setValue(name, value);
			} catch {
				// Expression doesn't exist on this model.
			}
		};
		if (!frame.emotePlaying) {
			this.blinkTimer += delta;
			if (!this.isBlinking && this.blinkTimer >= this.nextBlinkTime) {
				this.isBlinking = true;
				this.blinkProgress = 0;
			}
			if (this.isBlinking) {
				this.blinkProgress += delta * 8; // Blink duration ~0.125s
				// Asymmetric blink curve: quick close (30%), slow open (70%).
				let blinkValue: number;
				if (this.blinkProgress < 0.3) {
					blinkValue = this.blinkProgress / 0.3;
				} else {
					blinkValue = 1 - (this.blinkProgress - 0.3) / 0.7;
				}
				const finalBlinkValue = Math.max(0, blinkValue);
				if (this.blinkProgress >= 1) {
					this.isBlinking = false;
					this.blinkTimer = 0;
					this.nextBlinkTime = Math.random() * 4 + 2; // Random 2-6s
					setExpression('blink', 0);
					setExpression('Blink', 0);
					setExpression('eyeBlinkLeft', 0);
					setExpression('eyeBlinkRight', 0);
				} else {
					setExpression('blink', finalBlinkValue);
					setExpression('Blink', finalBlinkValue);
					setExpression('eyeBlinkLeft', finalBlinkValue);
					setExpression('eyeBlinkRight', finalBlinkValue);
				}
			}
		}
		em.update();
		const visemes = frame.visemes;
		setExpression('aa', visemes.aa);
		setExpression('ee', visemes.ee);
		setExpression('ih', visemes.ih);
		setExpression('oh', visemes.oh);
		setExpression('ou', visemes.ou);
		setExpression('a', visemes.aa);
		setExpression('i', visemes.ih);
		setExpression('u', visemes.ou);
		setExpression('e', visemes.ee);
		setExpression('o', visemes.oh);
		setExpression('jawOpen', visemes.aa * 0.7);
	}

	// Model swap or unmount: drop pulses, the transient face, mood
	// weights, and the held expression.
	reset(): void {
		this.activePulses = [];
		this.transientFace = null;
		this.moodWeights.clear();
		this.heldExpression = null;
	}
}
