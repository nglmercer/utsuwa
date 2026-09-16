// Animation controller: owns the mixer lifecycle and every clip-level
// state machine — idle cycling (+timer), the talking loop, one-shot
// emotes, and held photo poses with crossfades. The Svelte component owns
// the THREE scene, the stores, and the effects that call in; this
// controller owns the animation state and the transitions, behind an
// injected environment (clip catalogs, store readers/writers, routine
// callbacks) so it stays testable under node with a headless mixer.
//
// Nothing here is Svelte-reactive: every field used to be component-local
// `$state` read only inside `untrack()` or plain functions, so plain
// fields keep identical synchronous semantics.
import * as THREE from 'three';
import { createVRMAnimationClip } from '@pixiv/three-vrm-animation';
import type { VRM } from '@pixiv/three-vrm';
import { loadVrmAnimation } from '../services/vrm-animations.ts';
import { loadPoseAnimation, loadPoseManifest } from '../services/poses.ts';
import { actionForAnimationUrl, expressionForAnimationUrl } from '../engine/avatar-actions.ts';

export interface EmoteSelection {
	url: string;
	id: string;
}

export interface AnimationEnvironment {
	// Clip catalogs (store readers).
	idleUrls: () => string[];
	talkingUrl: () => string | null;
	findAnimation: (id: string) => EmoteSelection | undefined;
	// Emote selection (store read/write).
	readEmoteSelection: () => string | null;
	clearEmoteSelection: () => void;
	// Per-action emote face.
	requestFace: (expression: string, intensity: number, durationMs: number) => void;
	// Mode guards.
	photoActive: () => boolean;
	shouldTalk: () => boolean;
	// Routine coupling (the renderer owns the ledger; this only reports).
	hasActiveRoutine: () => boolean;
	isRoutineAwaitingEmote: (url: string) => boolean;
	isAnyEmoteAwaited: () => boolean;
	onEmoteClipStarted: (url: string, deadlineMs: number) => void;
	onEmoteFinished: (url: string) => void;
	onEmoteLoadFailed: () => void;
	onManualEmoteWhileRoutine: () => void;
	// Busy flag sync after every transition.
	onBusyChanged: () => void;
	log?: (message: string, detail?: string) => void;
}

export interface AnimationLoaders {
	loadClip: (url: string, vrm: VRM) => Promise<THREE.AnimationClip>;
	loadPose: (poseId: string, vrm: VRM) => Promise<{ clip: THREE.AnimationClip; hold: number } | null>;
}

export const defaultAnimationLoaders: AnimationLoaders = {
	loadClip: async (url, vrm) => createVRMAnimationClip(await loadVrmAnimation(url), vrm),
	loadPose: async (poseId, vrm) => {
		const manifest = await loadPoseManifest();
		const entry = manifest.find((pose) => pose.id === poseId);
		if (!entry) return null;
		const animation = await loadPoseAnimation(entry.file);
		return { clip: createVRMAnimationClip(animation, vrm), hold: entry.hold ?? 0 };
	}
};

export type PhotoTransition = 'entered' | 'exited' | 'none';

export class AnimationController {
	private readonly env: AnimationEnvironment;
	private readonly loaders: AnimationLoaders;
	private vrm: VRM | null = null;
	private mixer: THREE.AnimationMixer | null = null;
	private idleAction: THREE.AnimationAction | null = null;
	private talkingAction: THREE.AnimationAction | null = null;
	private talkingClip: THREE.AnimationClip | null = null;
	private emoteAction: THREE.AnimationAction | null = null;
	private emotePlaying = false;
	private lastIdleIndex = -1;
	private idleCycleTimeout: ReturnType<typeof setTimeout> | null = null;
	private poseAction: THREE.AnimationAction | null = null;
	// Rapid pose taps race their async loads; only the latest wins.
	private poseToken = 0;
	// Clips are per-model; cleared on detach so a model switch starts clean.
	private readonly poseClipCache = new Map<string, THREE.AnimationClip>();
	private photoActiveFlag = false;

	constructor(env: AnimationEnvironment, loaders: AnimationLoaders = defaultAnimationLoaders) {
		this.env = env;
		this.loaders = loaders;
	}

	private log(message: string, detail?: string): void {
		if (this.env.log) this.env.log(message, detail);
		else console.debug(message, detail);
	}

	get isEmotePlaying(): boolean {
		return this.emotePlaying;
	}

	get isAttached(): boolean {
		return this.vrm !== null && this.mixer !== null;
	}

	// A new model is live: start idling and preload the talking loop.
	attach(vrm: VRM, mixer: THREE.AnimationMixer): void {
		this.vrm = vrm;
		this.mixer = mixer;
		this.startIdleAnimation(vrm, mixer);
		this.loadTalkingAnimation(vrm, mixer);
	}

	// Model swap or unmount: stop timers and actions, drop clips, and clear
	// a mid-play emote selection whose 'finished' handler will never run.
	detach(): void {
		if (this.idleCycleTimeout) {
			clearTimeout(this.idleCycleTimeout);
			this.idleCycleTimeout = null;
		}
		if (this.mixer) {
			this.mixer.stopAllAction();
		}
		this.vrm = null;
		this.mixer = null;
		this.idleAction = null;
		this.talkingAction = null;
		this.talkingClip = null;
		this.emoteAction = null;
		if (this.emotePlaying) {
			this.emotePlaying = false;
			this.env.clearEmoteSelection();
		}
		this.poseAction = null;
		this.poseClipCache.clear();
	}

	update(delta: number): void {
		this.mixer?.update(delta);
	}

	// Pick a random idle animation index, excluding the last played one.
	private pickRandomIdleIndex(): number {
		const urls = this.env.idleUrls();
		if (urls.length <= 1) return 0;
		let newIndex: number;
		do {
			newIndex = Math.floor(Math.random() * urls.length);
		} while (newIndex === this.lastIdleIndex);
		return newIndex;
	}

	// Load and start the looping idle animation.
	private startIdleAnimation(targetVrm: VRM, targetMixer: THREE.AnimationMixer): void {
		const urls = this.env.idleUrls();
		if (!urls || urls.length === 0) return;
		const index = this.pickRandomIdleIndex();
		this.lastIdleIndex = index;
		const idleUrl = urls[index];
		this.loaders
			.loadClip(idleUrl, targetVrm)
			.then((clip) => {
				// Model was swapped or unmounted while this animation loaded.
				if (this.mixer !== targetMixer) return;
				const action = targetMixer.clipAction(clip);
				action.setLoop(THREE.LoopRepeat, Infinity);
				action.play();
				// A model that finishes loading while photo mode is already
				// open holds its stance instead of idling through the shot.
				if (this.env.photoActive()) action.paused = true;
				this.idleAction = action;
				this.scheduleIdleCycle(targetVrm, targetMixer, clip.duration);
			})
			.catch((error) => {
				console.error('Error loading idle animation:', error);
			});
	}

	// Schedule the next idle animation switch.
	private scheduleIdleCycle(
		targetVrm: VRM,
		targetMixer: THREE.AnimationMixer,
		duration: number
	): void {
		if (this.idleCycleTimeout) {
			clearTimeout(this.idleCycleTimeout);
		}
		// Switch after 1-2 full loops of the current animation.
		const loops = 1 + Math.random();
		const delay = duration * loops * 1000;
		this.idleCycleTimeout = setTimeout(() => {
			if (!this.env.shouldTalk() && !this.emotePlaying && !this.env.photoActive()) {
				this.playNextIdleAnimation(targetVrm, targetMixer);
			} else {
				// Retry later if we're busy (talking, emoting, or posing).
				this.scheduleIdleCycle(targetVrm, targetMixer, duration);
			}
		}, delay);
	}

	// Play the next random idle animation with smooth crossfade.
	private playNextIdleAnimation(targetVrm: VRM, targetMixer: THREE.AnimationMixer): void {
		const urls = this.env.idleUrls();
		if (!urls || urls.length === 0) return;
		const index = this.pickRandomIdleIndex();
		this.lastIdleIndex = index;
		const idleUrl = urls[index];
		this.loaders
			.loadClip(idleUrl, targetVrm)
			.then((clip) => {
				// Model was swapped or unmounted while this animation loaded.
				if (this.mixer !== targetMixer) return;
				if (this.idleAction) {
					this.idleAction.fadeOut(1.2);
				}
				const action = targetMixer.clipAction(clip);
				action.setLoop(THREE.LoopRepeat, Infinity);
				action.reset().fadeIn(1.2).play();
				this.idleAction = action;
				this.scheduleIdleCycle(targetVrm, targetMixer, clip.duration);
			})
			.catch((error) => {
				console.error('Error loading idle animation:', error);
			});
	}

	// Load the talking animation clip (called once after model loads).
	private loadTalkingAnimation(targetVrm: VRM, targetMixer: THREE.AnimationMixer): void {
		const talkingUrl = this.env.talkingUrl();
		if (!talkingUrl) return;
		this.loaders
			.loadClip(talkingUrl, targetVrm)
			.then((clip) => {
				// Model was swapped or unmounted while this animation loaded.
				if (this.mixer !== targetMixer) return;
				this.talkingClip = clip;
			})
			.catch((error) => {
				console.error('Error loading talking animation:', error);
			});
	}

	// Switch between idle and talking animations based on speaking state.
	// A held photo pose must not be stomped by TTS either; exit restores.
	// Photo activity arrives as a parameter (not an env read) because the
	// caller tracks it reactively: photo exit while speaking re-runs the
	// switch so the talking loop fades back in.
	setTalking(speaking: boolean, photoOpen: boolean): void {
		const currentMixer = this.mixer;
		const currentIdleAction = this.idleAction;
		const currentTalkingClip = this.talkingClip;
		if (!currentMixer || this.emotePlaying || photoOpen) return;
		if (speaking && currentTalkingClip) {
			if (currentIdleAction) {
				currentIdleAction.fadeOut(0.3);
			}
			if (!this.talkingAction) {
				this.talkingAction = currentMixer.clipAction(currentTalkingClip);
				this.talkingAction.setLoop(THREE.LoopRepeat, Infinity);
			}
			this.talkingAction.reset().fadeIn(0.3).play();
		} else if (!speaking) {
			if (this.talkingAction) {
				this.talkingAction.fadeOut(0.3);
			}
			if (currentIdleAction) {
				currentIdleAction.reset().fadeIn(0.3).play();
			}
		}
	}

	// Play emote animations when the selection changes; a null selection
	// stops the emote and ensures idle is playing.
	playEmoteSelection(animId: string | null): void {
		const currentVrm = this.vrm;
		const currentMixer = this.mixer;
		const currentIdleAction = this.idleAction;
		if (!currentVrm || !currentMixer) return;
		if (this.emoteAction) {
			this.emoteAction.fadeOut(0.3);
		}
		if (!animId) {
			this.emotePlaying = false;
			this.emoteAction = null;
			this.env.onBusyChanged();
			if (currentIdleAction && !currentIdleAction.isRunning()) {
				currentIdleAction.reset().fadeIn(0.3).play();
			}
			return;
		}
		const animationData = this.env.findAnimation(animId);
		if (!animationData?.url) return;
		this.loaders
			.loadClip(animationData.url, currentVrm)
			.then((clip) => {
				if (!this.vrm || !this.mixer) return;
				// Stale load (selection moved on while fetching): discard
				// instead of playing a clip nobody asked for anymore.
				if (this.env.readEmoteSelection() !== animId) return;
				if (this.idleAction) {
					this.idleAction.fadeOut(0.2);
				}
				// Loop mode comes from the action registry (all VRMA entries
				// are one-shots today).
				const action = this.mixer.clipAction(clip);
				const loopForever = actionForAnimationUrl(animationData.url)?.mode === 'loop';
				if (loopForever) action.setLoop(THREE.LoopRepeat, Infinity);
				else action.setLoop(THREE.LoopOnce, 1);
				action.clampWhenFinished = true;
				action.timeScale = 1.5;
				action.reset().fadeIn(0.2).play();
				this.emoteAction = action;
				this.emotePlaying = true;
				// Routine emote steps get a clip-derived watchdog budget
				// from the moment the clip actually starts playing.
				if (this.env.isRoutineAwaitingEmote(animationData.url)) {
					this.env.onEmoteClipStarted(
						animationData.url,
						(clip.duration / action.timeScale) * 1000 + 3000
					);
				}
				this.env.onBusyChanged();
				this.log('[AvatarCue] action started', `emote:${animationData.id}`);
				// A manually triggered emote supersedes any routine; the
				// routine's own emote step arrives with its URL awaited.
				if (
					this.env.hasActiveRoutine() &&
					!this.env.isRoutineAwaitingEmote(animationData.url)
				) {
					this.env.onManualEmoteWhileRoutine();
				}
				// Face comes from the action's metadata, not a generic grin:
				// unmapped clips leave the mood face alone.
				const face = expressionForAnimationUrl(animationData.url);
				if (face) {
					const clipMs = Math.round((clip.duration / action.timeScale) * 1000);
					this.env.requestFace(face.expression, face.intensity, clipMs);
				}
				const capturedMixer = this.mixer;
				const capturedIdleAction = this.idleAction;
				const onFinished = (e: { action: THREE.AnimationAction }) => {
					if (e.action === action) {
						capturedMixer.removeEventListener('finished', onFinished);
						this.emotePlaying = false;
						this.emoteAction = null;
						if (this.env.isRoutineAwaitingEmote(animationData.url)) {
							this.env.onEmoteFinished(animationData.url);
						}
						this.env.onBusyChanged();
						this.log('[AvatarCue] action finished', `emote:${animationData.id}`);
						// The action face fades out with its own envelope.
						if (capturedIdleAction) {
							capturedIdleAction.reset().fadeIn(0.3).play();
						}
						this.env.clearEmoteSelection();
					}
				};
				capturedMixer.addEventListener('finished', onFinished);
			})
			.catch((error) => {
				console.error('Error loading emote animation:', error);
				// Clear the stale selection so the gesture gate's busy flag
				// (currentAnimation !== null) can't stick forever.
				this.env.clearEmoteSelection();
				if (this.env.hasActiveRoutine() && this.env.isAnyEmoteAwaited()) {
					this.env.onEmoteLoadFailed();
				}
			});
	}

	// Enter/exit lifecycle: freeze the current stance on the way in, and
	// re-run the normal idle start path on the way out so cycling resumes
	// cleanly. Returns the transition so the caller can cancel the routine
	// on entry (a posed avatar must not keep dancing through the shot).
	setPhotoActive(active: boolean): PhotoTransition {
		const was = this.photoActiveFlag;
		if (!this.vrm || !this.mixer) {
			this.photoActiveFlag = active;
			return 'none';
		}
		if (active && !was) {
			if (this.talkingAction) this.talkingAction.fadeOut(0.2);
			if (this.idleAction) {
				// Ensure the idle actually holds weight (entering mid-talk
				// left it faded out), then freeze it as the held stance.
				this.idleAction.play();
				this.idleAction.fadeIn(0.2);
				this.idleAction.paused = true;
			}
			this.photoActiveFlag = active;
			return 'entered';
		}
		if (!active && was) {
			if (this.poseAction) {
				this.poseAction.fadeOut(0.6);
				this.poseAction = null;
			}
			// Resume the frozen idle so the crossfade has live motion to
			// blend from, then hand back to the cycler, which fades it out
			// against a fresh idle clip and reschedules cycling. Starting a
			// second idle at full weight here (the old path) blended two
			// idles at once and made the resumed animation drift strangely.
			// When TTS is still speaking, the talking switch fades the
			// talking action back in instead; starting an idle at the same
			// time would blend both at half weight.
			if (this.idleAction) this.idleAction.paused = false;
			if (!this.env.shouldTalk() && this.vrm && this.mixer) {
				this.playNextIdleAnimation(this.vrm, this.mixer);
			}
			this.photoActiveFlag = active;
			return 'exited';
		}
		this.photoActiveFlag = active;
		return 'none';
	}

	// Apply pose selections while photo mode is active. A held pose is a
	// single-frame clip: play, pause at the hold point, and let the weight
	// crossfade do the transition. Entering starts on Natural, which the
	// enter lifecycle already froze, but the token still bumps so an
	// in-flight pose load can't land stale.
	async applyPose(poseId: string | null): Promise<void> {
		const targetVrm = this.vrm;
		const targetMixer = this.mixer;
		if (!targetVrm || !targetMixer) return;
		const token = ++this.poseToken;
		// A slower fade reads as easing into the pose rather than a cut.
		const POSE_FADE = 0.6;
		if (poseId === null) {
			if (!this.poseAction) return;
			this.poseAction.fadeOut(POSE_FADE);
			this.poseAction = null;
			if (this.idleAction) {
				this.idleAction.reset().fadeIn(POSE_FADE).play();
				this.idleAction.paused = true;
			}
			return;
		}
		try {
			const loaded = await this.loaders.loadPose(poseId, targetVrm);
			// Model swapped or a newer pose was requested while loading.
			if (this.mixer !== targetMixer || token !== this.poseToken) return;
			if (!this.env.photoActive()) return;
			if (!loaded) return;
			let clip = this.poseClipCache.get(poseId);
			if (!clip) {
				clip = loaded.clip;
				this.poseClipCache.set(poseId, clip);
			}
			const previous = this.poseAction ?? this.idleAction;
			if (previous) previous.fadeOut(POSE_FADE);
			const action = targetMixer.clipAction(clip);
			action.reset();
			action.setLoop(THREE.LoopOnce, 1);
			action.clampWhenFinished = true;
			action.fadeIn(POSE_FADE).play();
			// Freeze at the clip's expressive moment (manifest hold,
			// fraction of duration). Frame zero is a neutral stance on most
			// motion clips, which made every placeholder pose identical.
			action.paused = true;
			action.time = clip.duration * Math.min(Math.max(loaded.hold, 0), 0.99);
			this.poseAction = action;
		} catch (e) {
			console.error('[PhotoMode] Failed to apply pose:', e);
		}
	}
}
