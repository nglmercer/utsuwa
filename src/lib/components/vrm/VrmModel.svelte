<script lang="ts">
	import { T, useThrelte, useTask } from '@threlte/core';
	import { GLTFLoader } from 'three/addons/loaders/GLTFLoader.js';
	import { VRMLoaderPlugin, VRMUtils, type VRM } from '@pixiv/three-vrm';
	import { createVRMAnimationClip } from '@pixiv/three-vrm-animation';
	import { loadVrmAnimation } from '$lib/services/vrm-animations';
	import { vrmStore } from '$lib/stores/vrm.svelte';
	import { ttsStore } from '$lib/stores/tts.svelte';
	import { displayStore } from '$lib/stores/display.svelte';
	import { photomodeStore } from '$lib/stores/photomode.svelte';
	import { loadPoseAnimation, loadPoseManifest } from '$lib/services/poses';
	import { pickReaction, stageTier, type TouchZone } from '$lib/engine/photo-reactions';
	import { characterStore } from '$lib/stores/character.svelte';
	import {
		approachWeight,
		blendTemporaryFace,
		directTemporaryTarget,
		emptyWeights,
		moodToExpressionWeights,
		reactionEnvelope,
		resolveTargets
	} from '$lib/engine/facial-expressions';
	import {
		actionForAnimationUrl,
		clampWalkOffset,
		expressionForAnimationUrl,
		jumpArcHeight
	} from '$lib/engine/avatar-actions';
	import {
		computeSpringJointParams,
		clampFrameDelta,
		type SpringJointParams
	} from '$lib/engine/spring-physics';
	import {
		cameraAngles,
		angularVelocity,
		stepJiggle,
		createJiggleState,
		type CameraAngles
	} from '$lib/engine/camera-impulse';
	import { lipSyncAnalyzer } from '$lib/services/lipsync/analyzer';
	import { untrack } from 'svelte';
	import * as THREE from 'three';

	// Pose configurations for different VRM versions
	// VRM 0.x and 1.0 have different bone orientations and coordinate systems
	const VRM_POSE_CONFIG = {
		// VRM 0.x (older models like AvatarSample_A/B)
		'0': {
			sceneRotationY: Math.PI, // Rotate 180° to face camera
			leftUpperArm: { x: Math.PI * 0.05, y: 0, z: Math.PI * 0.4 },
			rightUpperArm: { x: Math.PI * 0.05, y: 0, z: -Math.PI * 0.4 },
			leftLowerArm: { x: 0, y: -Math.PI * 0.1, z: 0 },
			rightLowerArm: { x: 0, y: Math.PI * 0.1, z: 0 }
		},
		// VRM 1.0 (VRoid Studio models like Utsuwa)
		'1': {
			sceneRotationY: 0, // Already facing camera
			leftUpperArm: { x: Math.PI * 0.05, y: 0, z: -Math.PI * 0.4 },
			rightUpperArm: { x: Math.PI * 0.05, y: 0, z: Math.PI * 0.4 },
			leftLowerArm: { x: 0, y: -Math.PI * 0.1, z: 0 }, // Same Y values as 0.x
			rightLowerArm: { x: 0, y: Math.PI * 0.1, z: 0 }
		}
	} as const;

	// Smoothing speed for mood-face weights (exponential approach; ~6 reaches
	// ~99% of a new target in under a second, frame-rate independent).
	const MOOD_FACE_SPEED = 6;

	// Best-effort expression write: presets were resolved against the model's
	// snapshot, so a throw only happens on a mid-frame model swap.
	function safeSetFace(
		manager: { setValue: (name: string, value: number) => void } | null | undefined,
		name: string,
		value: number
	) {
		if (!manager) return;
		try {
			manager.setValue(name, value);
		} catch {
			// Unknown preset on this model; resolution already skipped it.
		}
	}

	interface Props {
		url: string;
	}

	let { url }: Props = $props();
	let vrm = $state<VRM | null>(null);
	// AvatarRoot owns application-level world motion (walk translation, jump
	// arc, facing); the VRM scene parented inside it owns the skeleton pose.
	// The mixer never moves the root, so world motion and bone animation stay
	// separate systems that cannot fight.
	const avatarRoot = new THREE.Group();

	// === Spring-bone physics ===
	// Authored per-joint values captured at load. The intensity setting always
	// multiplies these bases (never the current values), so re-applying is
	// idempotent and a model switch starts clean from its own rig tuning.
	let springBase: Array<{
		settings: { stiffness: number; gravityPower: number; dragForce: number };
		base: SpringJointParams;
	}> = [];

	function snapshotSpringBase(target: VRM) {
		springBase = [];
		const joints = target.springBoneManager?.joints;
		if (!joints) return;
		for (const joint of joints) {
			springBase.push({
				settings: joint.settings,
				base: {
					stiffness: joint.settings.stiffness,
					gravityPower: joint.settings.gravityPower,
					dragForce: joint.settings.dragForce
				}
			});
		}
	}

	// Applied live so slider tuning is immediate; re-runs on model switch since
	// the load path re-assigns `vrm` after rebuilding the snapshot.
	$effect(() => {
		const intensity = displayStore.physicsIntensity;
		if (!vrm) return;
		for (const { settings, base } of springBase) {
			const next = computeSpringJointParams(base, intensity);
			settings.stiffness = next.stiffness;
			settings.gravityPower = next.gravityPower;
			settings.dragForce = next.dragForce;
		}
	});

	// === Animation State ===
	let mixer = $state<THREE.AnimationMixer | null>(null);
	let idleAction = $state<THREE.AnimationAction | null>(null); // Current idle animation
	let talkingAction = $state<THREE.AnimationAction | null>(null); // Looping talking animation
	let talkingClip = $state<THREE.AnimationClip | null>(null); // Cached talking clip
	let emoteAction = $state<THREE.AnimationAction | null>(null); // One-shot emote animations
	let isEmotePlaying = $state(false); // True when an emote is playing (disables blinking)
	let lastIdleIndex = $state(-1); // Track last played idle to avoid repeats
	const currentAnimation = $derived(vrmStore.currentAnimation);
	// Talking animation plays when TTS is speaking OR when text-based talking is triggered
	const shouldTalk = $derived(ttsStore.isSpeaking || vrmStore.isTalking);

	// === Blinking State ===
	let blinkTimer = $state(0);
	let nextBlinkTime = $state(Math.random() * 4 + 2); // 2-6 seconds
	let isBlinking = $state(false);
	let blinkProgress = $state(0);

	// === Breathing State ===
	let breathTime = $state(0);
	const BREATH_SPEED = 0.8; // cycles per second
	const BREATH_INTENSITY = 0.015; // subtle movement

	// === Eye Saccade State ===
	let saccadeTime = $state(0);
	let nextSaccadeIn = $state(1 + Math.random() * 2);
	let eyeTarget = $state({ x: 0, y: 0 });
	let currentEyeTarget = $state({ x: 0, y: 0 });

	// === Idle Face Animation State ===
	let idleFaceTime = $state(0);
	let headTime = $state(0);

	const { renderer, camera } = useThrelte();

	// Generate thumbnail from the current 3D render
	function generateThumbnail(modelId: string | null) {
		if (!renderer) return;

		const canvas = renderer.domElement;
		if (!canvas) return;

		const size = 256;
		const thumbCanvas = document.createElement('canvas');
		thumbCanvas.width = size;
		thumbCanvas.height = size;
		const ctx = thumbCanvas.getContext('2d');

		if (ctx) {
			const srcSize = Math.min(canvas.width, canvas.height);
			const srcX = (canvas.width - srcSize) / 2;
			const srcY = (canvas.height - srcSize) / 2;

			ctx.drawImage(canvas, srcX, srcY, srcSize, srcSize, 0, 0, size, size);

			const thumbnailDataUrl = thumbCanvas.toDataURL('image/png');
			vrmStore.setModelPreview(modelId, thumbnailDataUrl);
		}
	}

	// Normalize model orientation and position
	function normalizeModel(loadedVrm: VRM) {
		const scene = loadedVrm.scene;
		const version = loadedVrm.meta?.metaVersion === '1' ? '1' : '0';
		const config = VRM_POSE_CONFIG[version];

		// Apply version-specific scene rotation
		scene.rotation.y = config.sceneRotationY;

		// Calculate bounding box
		const box = new THREE.Box3().setFromObject(scene);
		const center = box.getCenter(new THREE.Vector3());

		// Center model at origin (X and Z)
		scene.position.x = -center.x;
		scene.position.z = -center.z;

		// Ground the model (feet at y=0)
		scene.position.y = -box.min.y;
	}

	// Set a natural idle pose (arms relaxed at sides)
	function setIdlePose(loadedVrm: VRM) {
		const humanoid = loadedVrm.humanoid;
		const version = loadedVrm.meta?.metaVersion === '1' ? '1' : '0';
		const config = VRM_POSE_CONFIG[version];

		// Get arm bones
		const leftUpperArm = humanoid.getNormalizedBoneNode('leftUpperArm');
		const rightUpperArm = humanoid.getNormalizedBoneNode('rightUpperArm');
		const leftLowerArm = humanoid.getNormalizedBoneNode('leftLowerArm');
		const rightLowerArm = humanoid.getNormalizedBoneNode('rightLowerArm');

		// Apply version-specific arm rotations
		if (leftUpperArm) {
			leftUpperArm.rotation.set(config.leftUpperArm.x, config.leftUpperArm.y, config.leftUpperArm.z);
		}
		if (rightUpperArm) {
			rightUpperArm.rotation.set(config.rightUpperArm.x, config.rightUpperArm.y, config.rightUpperArm.z);
		}
		if (leftLowerArm) {
			leftLowerArm.rotation.set(config.leftLowerArm.x, config.leftLowerArm.y, config.leftLowerArm.z);
		}
		if (rightLowerArm) {
			rightLowerArm.rotation.set(config.rightLowerArm.x, config.rightLowerArm.y, config.rightLowerArm.z);
		}
	}

	// Pick a random idle animation index, excluding the last played one
	function pickRandomIdleIndex(): number {
		const urls = vrmStore.idleAnimationUrls;
		if (urls.length <= 1) return 0;

		let newIndex: number;
		do {
			newIndex = Math.floor(Math.random() * urls.length);
		} while (newIndex === lastIdleIndex);

		return newIndex;
	}

	// Idle animation cycling timer
	let idleCycleTimeout: ReturnType<typeof setTimeout> | null = null;

	// Load and start the looping idle animation
	function startIdleAnimation(targetVrm: VRM, targetMixer: THREE.AnimationMixer) {
		const urls = vrmStore.idleAnimationUrls;
		if (!urls || urls.length === 0) return;

		const index = pickRandomIdleIndex();
		lastIdleIndex = index;
		const idleUrl = urls[index];

		loadVrmAnimation(idleUrl)
			.then((vrmAnimation) => {
				// Model was swapped or unmounted while this animation loaded
				if (mixer !== targetMixer) return;

				const clip = createVRMAnimationClip(vrmAnimation, targetVrm);
				const action = targetMixer.clipAction(clip);
				action.setLoop(THREE.LoopRepeat, Infinity);
				action.play();
				// A model that finishes loading while photo mode is already open
				// holds its stance instead of idling through the shot
				if (photomodeStore.active) action.paused = true;
				idleAction = action;

				// Schedule next animation change
				scheduleIdleCycle(targetVrm, targetMixer, clip.duration);
			})
			.catch((error) => {
				console.error('Error loading idle animation:', error);
			});
	}

	// Schedule the next idle animation switch
	function scheduleIdleCycle(targetVrm: VRM, targetMixer: THREE.AnimationMixer, duration: number) {
		if (idleCycleTimeout) {
			clearTimeout(idleCycleTimeout);
		}
		// Switch after 1-2 full loops of the current animation
		const loops = 1 + Math.random();
		const delay = duration * loops * 1000;
		idleCycleTimeout = setTimeout(() => {
			if (!shouldTalk && !isEmotePlaying && !photomodeStore.active) {
				playNextIdleAnimation(targetVrm, targetMixer);
			} else {
				// Retry later if we're busy (talking, emoting, or posing for a photo)
				scheduleIdleCycle(targetVrm, targetMixer, duration);
			}
		}, delay);
	}

	// Play the next random idle animation with smooth crossfade
	function playNextIdleAnimation(targetVrm: VRM, targetMixer: THREE.AnimationMixer) {
		const urls = vrmStore.idleAnimationUrls;
		if (!urls || urls.length === 0) return;

		const index = pickRandomIdleIndex();
		lastIdleIndex = index;
		const idleUrl = urls[index];

		loadVrmAnimation(idleUrl)
			.then((vrmAnimation) => {
				// Model was swapped or unmounted while this animation loaded
				if (mixer !== targetMixer) return;

				// Fade out current idle
				if (idleAction) {
					idleAction.fadeOut(1.2);
				}

				const clip = createVRMAnimationClip(vrmAnimation, targetVrm);
				const action = targetMixer.clipAction(clip);
				action.setLoop(THREE.LoopRepeat, Infinity);
				action.reset().fadeIn(1.2).play();
				idleAction = action;

				// Schedule next change
				scheduleIdleCycle(targetVrm, targetMixer, clip.duration);
			})
			.catch((error) => {
				console.error('Error loading idle animation:', error);
			});
	}

	// Load the talking animation clip (called once after model loads)
	function loadTalkingAnimation(targetVrm: VRM, targetMixer: THREE.AnimationMixer) {
		const talkingUrl = vrmStore.talkingAnimationUrl;
		if (!talkingUrl) return;

		loadVrmAnimation(talkingUrl)
			.then((vrmAnimation) => {
				// Model was swapped or unmounted while this animation loaded
				if (mixer !== targetMixer) return;

				talkingClip = createVRMAnimationClip(vrmAnimation, targetVrm);
			})
			.catch((error) => {
				console.error('Error loading talking animation:', error);
			});
	}

	// === Photo mode ===
	// A held pose is a single-frame clip: play, pause at t=0, and let the weight
	// crossfade do the transition. vrm.update() keeps running in the render task,
	// so spring bones and blinking stay alive while posed.
	let poseAction: THREE.AnimationAction | null = null;
	// Rapid pose taps race their async loads; only the latest application wins.
	let poseToken = 0;
	// Clips are per-model; caching them means repeat selections reuse the same
	// mixer action instead of accumulating new clips. Cleared on model switch.
	const poseClipCache = new Map<string, THREE.AnimationClip>();

	async function applyPhotoPose(poseId: string | null) {
		const targetVrm = vrm;
		const targetMixer = mixer;
		if (!targetVrm || !targetMixer) return;
		const token = ++poseToken;

		// A slower fade reads as easing into the pose rather than a hard cut
		const POSE_FADE = 0.6;

		if (poseId === null) {
			// Natural: fade any pose out and hold the idle stance
			if (poseAction) {
				poseAction.fadeOut(POSE_FADE);
				poseAction = null;
			}
			if (idleAction) {
				idleAction.reset().fadeIn(POSE_FADE).play();
				idleAction.paused = true;
			}
			return;
		}

		const manifest = await loadPoseManifest();
		const entry = manifest.find((p) => p.id === poseId);
		if (!entry) return;

		try {
			const animation = await loadPoseAnimation(entry.file);
			// Model swapped or a newer pose was requested while this one loaded
			if (mixer !== targetMixer || token !== poseToken) return;
			if (!photomodeStore.active) return;

			let clip = poseClipCache.get(poseId);
			if (!clip) {
				clip = createVRMAnimationClip(animation, targetVrm);
				poseClipCache.set(poseId, clip);
			}
			const previous = poseAction ?? idleAction;
			if (previous) previous.fadeOut(POSE_FADE);

			const action = targetMixer.clipAction(clip);
			action.reset();
			action.setLoop(THREE.LoopOnce, 1);
			action.clampWhenFinished = true;
			action.fadeIn(POSE_FADE).play();
			// Freeze at the clip's expressive moment (manifest hold, fraction of
			// duration). Frame zero is a neutral stance on most motion clips, which
			// made every placeholder pose look identical.
			action.paused = true;
			action.time = clip.duration * Math.min(Math.max(entry.hold ?? 0, 0), 0.99);
			poseAction = action;
		} catch (e) {
			console.error('[PhotoMode] Failed to apply pose:', e);
		}
	}

	// Enter/exit lifecycle: freeze the current stance on the way in, and re-run
	// the normal idle start path on the way out so cycling resumes cleanly.
	let wasPhotoActive = false;
	$effect(() => {
		const active = photomodeStore.active;
		untrack(() => {
			const targetVrm = vrm;
			const targetMixer = mixer;
			if (!targetVrm || !targetMixer) {
				wasPhotoActive = active;
				return;
			}
			if (active && !wasPhotoActive) {
				cancelRoutine('photo-mode');
				if (talkingAction) talkingAction.fadeOut(0.2);
				if (idleAction) {
					// Ensure the idle actually holds weight (entering mid-talk left it
					// faded out), then freeze it as the held stance.
					idleAction.play();
					idleAction.fadeIn(0.2);
					idleAction.paused = true;
				}
			} else if (!active && wasPhotoActive) {
				if (poseAction) {
					poseAction.fadeOut(0.6);
					poseAction = null;
				}
				// Resume the frozen idle so the crossfade has live motion to blend
				// from, then hand back to the cycler, which fades it out against a
				// fresh idle clip and reschedules cycling. Starting a second idle at
				// full weight here (the old path) blended two idles at once and made
				// the resumed animation drift strangely. When TTS is still speaking,
				// the talking-switch effect fades the talking action back in instead;
				// starting an idle at the same time would blend both at half weight.
				if (idleAction) idleAction.paused = false;
				if (!shouldTalk) {
					playNextIdleAnimation(targetVrm, targetMixer);
				}
			}
			wasPhotoActive = active;
		});
	});

	// Apply pose selections while photo mode is active
	$effect(() => {
		const active = photomodeStore.active;
		const poseId = photomodeStore.selectedPoseId;
		if (!active) return;
		untrack(() => {
			// Entering starts on Natural, which the enter lifecycle already froze,
			// but the token still bumps so an in-flight pose load can't land stale
			if (poseId === null && !poseAction) {
				poseToken++;
				return;
			}
			applyPhotoPose(poseId);
		});
	});

	// Held photo expression: applied exclusively, cleared on change and exit.
	// Tap reactions layer a transient expression on top and restore this one.
	let heldExpression: string | null = null;
	$effect(() => {
		const active = photomodeStore.active;
		const name = photomodeStore.selectedExpression;
		untrack(() => {
			const em = vrm?.expressionManager;
			if (!em) return;
			if (heldExpression && heldExpression !== name) {
				em.setValue(heldExpression, 0);
				heldExpression = null;
			}
			if (active && name) {
				em.setValue(name, 1);
				heldExpression = name;
			}
		});
	});

	// Tap reactions: an expression flash plus a decaying rotation nudge whose
	// motion the spring bones inherit. Repeat taps inside the window escalate.
	// Pulses overlap instead of replacing each other (replacement snapped the
	// active nudge to zero, which read as a jump on rapid taps), and every
	// nudge applied to a bone is explicitly undone at the start of the next
	// frame, so nothing can accumulate no matter what the mixer weights are.
	interface ReactionPulse {
		bone: THREE.Object3D;
		t: number;
		duration: number;
		magnitude: number;
		direction: number;
	}
	let activePulses: ReactionPulse[] = [];
	let appliedNudges: Array<{ bone: THREE.Object3D; z: number; x: number; y?: number }> = [];
	// In-flight transient face (tap flash, emote grin, AI cue): staged from
	// vrmStore.expressionRequest and arbitrated against the mood face below.
	let transientFace: { name: string; weight: number; t: number; duration: number; seq: number } | null =
		null;
	// Smoothed weights the mood system owns (emotional presets + custom
	// transient names like 'shy'). Never includes blink/viseme/jaw channels.
	const moodWeights = new Map<string, number>();
	const recentTaps = { zone: null as TouchZone | null, at: 0, count: 0 };
	const REACTION_REPEAT_WINDOW_MS = 4000;

	$effect(() => {
		const request = vrmStore.expressionRequest;
		untrack(() => {
			if (!request) {
				// Cleared mid-flight: jump into the release so the face fades
				// back to mood instead of snapping.
				if (transientFace) {
					transientFace.t = Math.max(transientFace.t, transientFace.duration * 0.7);
				}
				return;
			}
			if (transientFace && transientFace.seq === request.seq) return;
			transientFace = {
				name: request.expression,
				weight: request.intensity,
				t: 0,
				duration: Math.max(0.1, request.durationMs / 1000),
				seq: request.seq
			};
		});
	});

	$effect(() => {
		const request = vrmStore.reactionRequest;
		if (!request) return;
		untrack(() => {
			const targetVrm = vrm;
			if (!targetVrm) return;

			const now = performance.now();
			if (recentTaps.zone === request.zone && now - recentTaps.at < REACTION_REPEAT_WINDOW_MS) {
				recentTaps.count += 1;
			} else {
				recentTaps.count = 0;
			}
			recentTaps.zone = request.zone;
			recentTaps.at = now;

			const tier = stageTier(characterStore.state.relationshipStage);
			const spec = pickReaction(request.zone, tier, recentTaps.count);

			// The face flash goes through the shared arbitration (no direct
			// expression writes here): it overlays the mood face, then melts back.
			const em = targetVrm.expressionManager;
			if (em) {
				const available = em.expressions.map((e) => e.expressionName);
				const name = spec.expressions.find((candidate) => available.includes(candidate));
				if (name) {
					vrmStore.requestExpression({ expression: name, intensity: spec.weight, durationMs: 1800 });
				}
			}

			const bone =
				request.zone === 'head' || request.zone === 'face'
					? targetVrm.humanoid.getNormalizedBoneNode('head')
					: request.zone === 'shoulder'
						? (targetVrm.humanoid.getNormalizedBoneNode('upperChest') ??
							targetVrm.humanoid.getNormalizedBoneNode('chest'))
						: request.zone === 'torso'
							? targetVrm.humanoid.getNormalizedBoneNode('spine')
							: targetVrm.humanoid.getNormalizedBoneNode('hips');
			const fallback = targetVrm.humanoid.getNormalizedBoneNode('spine');
			const target = bone ?? fallback;
			if (target && activePulses.length < 4) {
				// Half strength while she is talking: the head is already moving,
				// and a full kick layered on that read as a jump
				const talkScale = shouldTalk ? 0.5 : 1;
				activePulses.push({
					bone: target,
					t: 0,
					duration: 0.9,
					magnitude: spec.impulse * talkScale,
					direction: Math.random() > 0.5 ? 1 : -1
				});
			}
		});
	});

	// === Intentional avatar actions (procedural / jump / walk) ===
	// At most one runs at a time; staging a new one cancels the current.
	// Bone offsets reuse appliedNudges (auto-undone next frame); root motion
	// is set absolutely every frame, so interrupts can never strand a pose.
	const PROCEDURAL_DURATIONS: Record<string, number> = {
		nod: 0.9,
		shake_head: 0.9,
		bow: 1.3,
		shrug: 0.8
	};
	const WALK_SPEED = 0.45; // m/s: gentle, stays framed
	const WALK_STEP_HZ = 1.7;
	const WALK_MAX_RADIUS = 2.0; // m from origin; walks clamp onto this rim
	const JUMP_DURATION = 0.65; // s
	const JUMP_HEIGHT = 0.28; // m at the apex
	interface LocomotionState {
		dirX: number;
		dirZ: number;
		remainingMs: number;
		phase: number;
	}
	let locomotion: LocomotionState | null = null;
	let jumpState: { t: number } | null = null;
	let procedural: { name: string; t: number; duration: number } | null = null;
	let actionSeqSeen = 0;

	function syncActionBusy() {
		vrmStore.setActionBusy(
			isEmotePlaying || locomotion !== null || jumpState !== null || procedural !== null
		);
	}

	function cancelAvatarActions() {
		locomotion = null;
		procedural = null;
		if (jumpState) {
			jumpState = null;
			avatarRoot.position.y = 0;
		}
		syncActionBusy();
	}

	function smoothstep(edge0: number, edge1: number, x: number): number {
		const t = Math.min(1, Math.max(0, (x - edge0) / (edge1 - edge0)));
		return t * t * (3 - 2 * t);
	}

	function shortAngle(delta: number): number {
		while (delta > Math.PI) delta -= Math.PI * 2;
		while (delta < -Math.PI) delta += Math.PI * 2;
		return delta;
	}

	function nudge(bone: THREE.Object3D | null, x: number, z: number, y = 0) {
		if (!bone) return;
		bone.rotation.x += x;
		bone.rotation.z += z;
		if (y) bone.rotation.y += y;
		appliedNudges.push({ bone, z, x, y });
	}

	// One procedural frame at progress p (0..1). Small additive programs that
	// read correctly without mocap; all offsets unwind via appliedNudges.
	function applyProceduralAction(name: string, p: number) {
		const targetVrm = vrm;
		if (!targetVrm) return;
		const humanoid = targetVrm.humanoid;
		if (name === 'nod') {
			const a = Math.sin(p * Math.PI * 2);
			nudge(humanoid.getNormalizedBoneNode('head'), -0.3 * a, 0);
			nudge(humanoid.getNormalizedBoneNode('neck'), -0.12 * a, 0);
		} else if (name === 'shake_head') {
			nudge(humanoid.getNormalizedBoneNode('head'), 0, 0, 0.45 * Math.sin(p * Math.PI * 2));
		} else if (name === 'bow') {
			const k = smoothstep(0, 0.35, p) * (1 - smoothstep(0.65, 1, p));
			nudge(humanoid.getNormalizedBoneNode('spine'), 0.38 * k, 0);
			nudge(humanoid.getNormalizedBoneNode('chest'), 0.12 * k, 0);
			nudge(humanoid.getNormalizedBoneNode('head'), 0.24 * k, 0);
		} else if (name === 'shrug') {
			const k = Math.sin(p * Math.PI);
			for (const side of ['leftUpperArm', 'rightUpperArm'] as const) {
				const bone = humanoid.getNormalizedBoneNode(side);
				// Outward follows the rig's own rest sign, so v0 and v1 agree.
				if (bone) nudge(bone, 0, (Math.sign(bone.rotation.z) || 1) * 0.28 * k);
			}
			nudge(humanoid.getNormalizedBoneNode('head'), -0.06 * k, 0.1 * k);
		}
	}

	const scratchWalkDir = new THREE.Vector3();
	const scratchWalkToAvatar = new THREE.Vector3();

	// Walk directions are viewer-relative: "left" means screen-left from the
	// current camera, "forward" means toward the viewer.
	function walkDirectionVector(direction: string): { x: number; z: number } {
		if (camera.current) {
			camera.current.getWorldPosition(scratchWalkDir);
			scratchWalkToAvatar.subVectors(avatarRoot.position, scratchWalkDir);
			scratchWalkToAvatar.y = 0;
			if (scratchWalkToAvatar.lengthSq() > 1e-6) {
				scratchWalkToAvatar.normalize();
				// Screen-right = view direction × up = (-vz, 0, vx).
				const rx = -scratchWalkToAvatar.z;
				const rz = scratchWalkToAvatar.x;
				if (direction === 'left') return { x: -rx, z: -rz };
				if (direction === 'right') return { x: rx, z: rz };
				if (direction === 'back') return { x: scratchWalkToAvatar.x, z: scratchWalkToAvatar.z };
				return { x: -scratchWalkToAvatar.x, z: -scratchWalkToAvatar.z };
			}
		}
		if (direction === 'left') return { x: -1, z: 0 };
		if (direction === 'right') return { x: 1, z: 0 };
		if (direction === 'back') return { x: 0, z: -1 };
		return { x: 0, z: 1 };
	}

	function startProceduralAction(name: string): boolean {
		const duration = PROCEDURAL_DURATIONS[name];
		if (!duration) return false;
		procedural = { name, t: 0, duration };
		console.debug('[AvatarCue] action started', `procedural:${name}`);
		return true;
	}

	function startJumpAction(): void {
		jumpState = { t: 0 };
		console.debug('[AvatarCue] action started', 'jump');
	}

	function startWalkAction(direction: string, durationMs?: number): void {
		const dir = walkDirectionVector(direction);
		locomotion = {
			dirX: dir.x,
			dirZ: dir.z,
			remainingMs: Math.min(3000, Math.max(300, Math.round(durationMs ?? 1200))),
			phase: 0
		};
		console.debug('[AvatarCue] action started', `walk:${direction}`);
	}

	function handleAvatarActionRequest(req: {
		kind: string;
		action: string;
		direction?: string;
		durationMs?: number;
	}) {
		if (req.kind === 'stop') {
			cancelRoutine('stopped');
			vrmStore.setCurrentAnimation(null);
			return;
		}
		if (req.kind === 'reset') {
			cancelRoutine('reset');
			avatarRoot.position.set(0, 0, 0);
			avatarRoot.rotation.set(0, 0, 0);
			return;
		}
		// A new action interrupts the current one (and any routine); every
		// program below is absolute per-frame, so interruption strands nothing.
		cancelRoutine('superseded');
		if (req.kind === 'procedural') {
			if (!startProceduralAction(req.action)) return;
		} else if (req.kind === 'jump') {
			startJumpAction();
		} else if (req.kind === 'walk') {
			startWalkAction(req.direction ?? 'forward', req.durationMs);
		} else {
			return;
		}
		syncActionBusy();
	}

	// === Avatar routines: ordered step sequences with completion ===
	// Every step transitions pending -> running -> completed|failed|cancelled
	// in the ledger, and the routine resolves to completed ONLY when every
	// expected step completed with zero failures. A per-step watchdog (driven
	// by render time, so a hidden tab pauses deadlines together with motion)
	// fails stuck steps instead of hanging the routine forever.
	interface ActiveRoutine {
		id: string;
		steps: Array<{ kind: string; action: string; direction?: string; durationMs?: number; url?: string }>;
		index: number;
		completed: string[];
		failures: Array<{ stepIndex: number; key: string; reason: string }>;
		continueOnFailure: boolean;
		startedAt: number;
		stepStartedAt: number;
		stepElapsed: number;
		stepDeadlineMs: number;
	}
	let routine: ActiveRoutine | null = null;
	let routineSeqSeen = 0;
	let routineAwaitingEmote: string | null = null;

	function routineStepKey(step: { kind: string; action: string; direction?: string }): string {
		return `${step.kind}:${step.action}${step.direction ? `:${step.direction}` : ''}`;
	}

	function finishRoutine(status: 'completed' | 'partial' | 'failed' | 'cancelled' | 'timed_out') {
		if (!routine) return;
		vrmStore.recordRoutineResult({
			routineId: routine.id,
			status,
			expected: routine.steps.map(routineStepKey),
			completed: routine.completed,
			failures: routine.failures,
			startedAt: routine.startedAt,
			finishedAt: Date.now(),
			endPosition: { x: avatarRoot.position.x, y: avatarRoot.position.y, z: avatarRoot.position.z }
		});
		console.debug('[AvatarCue] routine finished', `${routine.id} (${status})`);
		routine = null;
		routineAwaitingEmote = null;
		syncActionBusy();
	}

	function cancelRoutine(reason: string) {
		if (routine) {
			const step = routine.steps[routine.index];
			if (step) {
				vrmStore.recordRoutineStep({
					routineId: routine.id,
					stepIndex: routine.index,
					key: routineStepKey(step),
					status: 'cancelled',
					reason,
					startedAt: routine.stepStartedAt,
					finishedAt: Date.now()
				});
			}
			// Drop a routine-owned emote selection so a pending load cannot
			// play after the cancel; a manual selection is left untouched.
			if (routineAwaitingEmote && vrmStore.currentAnimation === routineAwaitingEmote) {
				vrmStore.setCurrentAnimation(null);
			}
			console.debug('[AvatarCue] routine cancelled', `${routine.id} (${reason})`);
		}
		cancelAvatarActions();
		if (!routine) return;
		finishRoutine('cancelled');
	}

	function advanceRoutine() {
		if (!routine) return;
		if (routine.index >= routine.steps.length) {
			if (routine.failures.length === 0) finishRoutine('completed');
			else if (routine.completed.length > 0) finishRoutine('partial');
			else finishRoutine('failed');
			return;
		}
		const step = routine.steps[routine.index];
		const key = routineStepKey(step);
		routine.stepStartedAt = Date.now();
		routine.stepElapsed = 0;
		let started = false;
		let deadline = 5000;
		if (step.kind === 'procedural') {
			started = startProceduralAction(step.action);
			deadline = (PROCEDURAL_DURATIONS[step.action] ?? 1) * 1000 + 1500;
		} else if (step.kind === 'jump') {
			startJumpAction();
			started = true;
			deadline = JUMP_DURATION * 1000 + 1500;
		} else if (step.kind === 'walk') {
			startWalkAction(step.direction ?? 'forward', step.durationMs);
			started = true;
			deadline = Math.min(3000, Math.max(300, Math.round(step.durationMs ?? 1200))) + 1500;
		} else if (step.kind === 'emote' && step.url) {
			routineAwaitingEmote = step.url;
			vrmStore.setCurrentAnimation(step.url);
			started = true;
			deadline = 15000;
		}
		routine.stepDeadlineMs = deadline;
		if (!started) {
			failCurrentStep('unknown step');
			return;
		}
		vrmStore.recordRoutineStep({
			routineId: routine.id,
			stepIndex: routine.index,
			key,
			status: 'running',
			startedAt: routine.stepStartedAt
		});
		syncActionBusy();
	}

	function failCurrentStep(reason: string, timedOut = false) {
		if (!routine) return;
		const step = routine.steps[routine.index];
		if (!step) return;
		const key = routineStepKey(step);
		routine.failures.push({ stepIndex: routine.index, key, reason });
		vrmStore.recordRoutineStep({
			routineId: routine.id,
			stepIndex: routine.index,
			key,
			status: 'failed',
			reason,
			startedAt: routine.stepStartedAt,
			finishedAt: Date.now()
		});
		// Stop the half-played motion.
		if (step.kind === 'walk') locomotion = null;
		if (step.kind === 'jump') {
			jumpState = null;
			avatarRoot.position.y = 0;
		}
		if (step.kind === 'procedural') procedural = null;
		if (step.kind === 'emote') {
			routineAwaitingEmote = null;
			if (step.url && vrmStore.currentAnimation === step.url) vrmStore.setCurrentAnimation(null);
		}
		routine.index++;
		if (!routine.continueOnFailure) {
			if (timedOut) finishRoutine('timed_out');
			else if (routine.completed.length > 0) finishRoutine('partial');
			else finishRoutine('failed');
			return;
		}
		advanceRoutine();
	}

	function completeRoutineStep() {
		if (!routine) return;
		const step = routine.steps[routine.index];
		if (!step) return;
		const key = routineStepKey(step);
		routine.completed.push(key);
		vrmStore.recordRoutineStep({
			routineId: routine.id,
			stepIndex: routine.index,
			key,
			status: 'completed',
			startedAt: routine.stepStartedAt,
			finishedAt: Date.now()
		});
		routine.index++;
		advanceRoutine();
	}

	function startRoutine(request: {
		id: string;
		steps: ActiveRoutine['steps'];
		policy?: { continueOnFailure?: boolean };
	}) {
		// Replacement policy: at most one routine; a new one cancels the old.
		cancelRoutine('superseded');
		routine = {
			id: request.id,
			steps: request.steps,
			index: 0,
			completed: [],
			failures: [],
			continueOnFailure: request.policy?.continueOnFailure ?? false,
			startedAt: Date.now(),
			stepStartedAt: 0,
			stepElapsed: 0,
			stepDeadlineMs: 0
		};
		console.debug('[AvatarCue] routine started', `${request.id} (${request.steps.length} steps)`);
		advanceRoutine();
	}

	$effect(() => {
		const request = vrmStore.actionRequest;
		if (!request) return;
		untrack(() => {
			if (!vrm || !mixer) return;
			if (request.seq === actionSeqSeen) return;
			actionSeqSeen = request.seq;
			handleAvatarActionRequest(request);
		});
	});

	$effect(() => {
		const request = vrmStore.routineRequest;
		if (!request) return;
		untrack(() => {
			if (!vrm || !mixer) return;
			if (request.seq === routineSeqSeen) return;
			routineSeqSeen = request.seq;
			startRoutine(request);
		});
	});

	// Update lip-sync analyser when TTS state changes
	$effect(() => {
		lipSyncAnalyzer.setAnalyser(ttsStore.currentAnalyser);
	});

	// Switch between idle and talking animations based on speaking/talking state
	$effect(() => {
		const speaking = shouldTalk;
		const currentMixer = untrack(() => mixer);
		const currentIdleAction = untrack(() => idleAction);
		const currentTalkingClip = untrack(() => talkingClip);
		const currentEmotePlaying = untrack(() => isEmotePlaying);

		// Don't switch if emote is playing or no mixer/clips available. A held
		// photo pose must not be stomped by TTS either; exit restores the loop.
		if (!currentMixer || currentEmotePlaying || photomodeStore.active) return;

		if (speaking && currentTalkingClip) {
			// Start talking animation, fade out idle
			if (currentIdleAction) {
				currentIdleAction.fadeOut(0.3);
			}

			// Create and play talking action
			let currentTalkingAction = untrack(() => talkingAction);
			if (!currentTalkingAction) {
				currentTalkingAction = currentMixer.clipAction(currentTalkingClip);
				currentTalkingAction.setLoop(THREE.LoopRepeat, Infinity);
				talkingAction = currentTalkingAction;
			}
			currentTalkingAction.reset().fadeIn(0.3).play();

		} else if (!speaking) {
			// Stop talking, resume idle animation
			const currentTalkingAction = untrack(() => talkingAction);
			if (currentTalkingAction) {
				currentTalkingAction.fadeOut(0.3);
			}

			// Resume the current idle action
			if (currentIdleAction) {
				currentIdleAction.reset().fadeIn(0.3).play();
			}

		}
	});

	// Play emote animations when currentAnimation changes
	$effect(() => {
		const animId = currentAnimation;
		const currentVrm = untrack(() => vrm);
		const currentMixer = untrack(() => mixer);
		const currentIdleAction = untrack(() => idleAction);

		if (!currentVrm || !currentMixer) return;

		// Stop any current emote
		const prevEmote = untrack(() => emoteAction);
		if (prevEmote) {
			prevEmote.fadeOut(0.3);
		}

		// If no emote selected, just ensure idle is playing
		if (!animId) {
			isEmotePlaying = false;
			emoteAction = null;
			syncActionBusy();
			if (currentIdleAction && !currentIdleAction.isRunning()) {
				currentIdleAction.reset().fadeIn(0.3).play();
			}
			return;
		}

		// Find the emote animation
		const animationData = vrmStore.availableAnimations.find((a) => a.url === animId || a.id === animId);
		if (!animationData?.url) return;

		// Load emote VRMA file
		loadVrmAnimation(animationData.url)
			.then((vrmAnimation) => {
				untrack(() => {
					if (!vrm || !mixer) return;
					// Stale load (selection moved on while fetching): discard
					// instead of playing a clip nobody asked for anymore.
					if (vrmStore.currentAnimation !== animId) return;

					// Fade out idle animation
					const currentIdle = idleAction;
					if (currentIdle) {
						currentIdle.fadeOut(0.2);
					}

					// Create and play emote; loop mode comes from the action
					// registry (all VRMA entries are one-shots today).
					const clip = createVRMAnimationClip(vrmAnimation, vrm);
					const action = mixer.clipAction(clip);
					const loopForever = actionForAnimationUrl(animationData.url)?.mode === 'loop';
					if (loopForever) action.setLoop(THREE.LoopRepeat, Infinity);
					else action.setLoop(THREE.LoopOnce, 1);
					action.clampWhenFinished = true;
					action.timeScale = 1.5;
					action.reset().fadeIn(0.2).play();
					emoteAction = action;
					isEmotePlaying = true;
					// Routine emote steps get a clip-derived watchdog budget
					// from the moment the clip actually starts playing.
					if (routine && routineAwaitingEmote === animationData.url) {
						routine.stepDeadlineMs =
							(action.getClip().duration / action.timeScale) * 1000 + 3000;
						routine.stepElapsed = 0;
					}
					syncActionBusy();
					console.debug('[AvatarCue] action started', `emote:${animationData.id}`);
					// A manually triggered emote supersedes any routine; the
					// routine's own emote step arrives with its URL awaited.
					if (routine && routineAwaitingEmote !== animationData.url) {
						cancelRoutine('superseded');
					}

					// Face comes from the action's metadata, not a generic
					// grin: unmapped clips leave the mood face alone.
					const face = expressionForAnimationUrl(animationData.url);
					if (face) {
						const clipMs = Math.round((action.getClip().duration / action.timeScale) * 1000);
						vrmStore.requestExpression({
							expression: face.expression,
							intensity: face.intensity,
							durationMs: clipMs
						});
					}

					// When emote finishes, return to idle
					const capturedMixer = mixer;
					const capturedIdleAction = currentIdle;
					const onFinished = (e: { action: THREE.AnimationAction }) => {
						if (e.action === action) {
							capturedMixer.removeEventListener('finished', onFinished);
							isEmotePlaying = false;
							emoteAction = null;
							if (routine && routineAwaitingEmote === animationData.url) {
								routineAwaitingEmote = null;
								completeRoutineStep();
							}
							syncActionBusy();
							console.debug('[AvatarCue] action finished', `emote:${animationData.id}`);

							// The action face fades out with its own envelope; nothing to clear.

							// Resume idle animation
							if (capturedIdleAction) {
								capturedIdleAction.reset().fadeIn(0.3).play();
							}

							vrmStore.setCurrentAnimation(null);
						}
					};
					capturedMixer.addEventListener('finished', onFinished);
				});
			})
			.catch((error) => {
				console.error('Error loading emote animation:', error);
				// Clear the stale selection so the gesture gate's busy flag
				// (currentAnimation !== null) can't stick forever.
				vrmStore.setCurrentAnimation(null);
				if (routine && routineAwaitingEmote) {
					routineAwaitingEmote = null;
					failCurrentStep('emote load failed');
				}
			});
	});

	// Load VRM when URL changes
	$effect(() => {
		if (!url) return;

		// Capture the model this load belongs to, so a fast switch can't save this
		// render under a different model's id.
		const loadModelId = vrmStore.activeModelId;

		// Invalidate this load if the URL changes or the component unmounts
		// before the loader finishes, so a slow load can't clobber a newer one
		let cancelled = false;

		vrmStore.setLoading(true);
		vrmStore.setError(null);

		const loader = new GLTFLoader();
		loader.crossOrigin = 'anonymous';
		loader.register((parser) => {
			const plugin = new VRMLoaderPlugin(parser);
			// Enable thumbnail loading for VRM 1.0 models
			if (plugin.metaPlugin) {
				plugin.metaPlugin.needThumbnailImage = true;
			}
			return plugin;
		});

		loader.load(
			url,
			(gltf) => {
				const loadedVrm = gltf.userData.vrm as VRM;

				if (cancelled) {
					VRMUtils.deepDispose(loadedVrm.scene);
					return;
				}

				// Optimize VRM
				VRMUtils.removeUnnecessaryVertices(loadedVrm.scene);
				VRMUtils.removeUnnecessaryJoints(loadedVrm.scene);

				// Skip frustum culling so animated meshes never pop out at the edges
				loadedVrm.scene.traverse((obj) => {
					obj.frustumCulled = false;
				});

				// Normalize model orientation and position
				normalizeModel(loadedVrm);

				// Set a natural idle pose (arms down instead of T-pose)
				setIdlePose(loadedVrm);

				// Capture this rig's authored spring values before `vrm` flips the
				// physics-intensity effect, so it applies over fresh bases.
				snapshotSpringBase(loadedVrm);

				vrm = loadedVrm;
				avatarRoot.position.set(0, 0, 0);
				avatarRoot.rotation.set(0, 0, 0);
				avatarRoot.add(loadedVrm.scene);
				const newMixer = new THREE.AnimationMixer(loadedVrm.scene);
				mixer = newMixer;
				vrmStore.setVrm(loadedVrm);
				vrmStore.setLoading(false);

				// Start the looping idle animation
				startIdleAnimation(loadedVrm, newMixer);

				// Pre-load the talking animation
				loadTalkingAnimation(loadedVrm, newMixer);

				// Debug: Log available expressions
				// if (loadedVrm.expressionManager) {
				// 	const expressions = loadedVrm.expressionManager.expressions;
				// 	console.log(
				// 		'Available expressions:',
				// 		expressions.map((e) => e.expressionName)
				// 	);
				// }

				// Extract thumbnail from VRM metadata (supports both 0.x and 1.0)
				let thumbnailImage: HTMLImageElement | undefined;

				if (loadedVrm.meta) {
					if (loadedVrm.meta.metaVersion === '1') {
						// VRM 1.0: thumbnailImage is HTMLImageElement
						thumbnailImage = (loadedVrm.meta as any).thumbnailImage;
					} else {
						// VRM 0.x: texture contains the image
						const texture = (loadedVrm.meta as any).texture;
						if (texture?.image) {
							thumbnailImage = texture.image;
						}
					}
				}

				if (thumbnailImage) {
					try {
						const canvas = document.createElement('canvas');
						const width = thumbnailImage.width || (thumbnailImage as any).naturalWidth || 256;
						const height = thumbnailImage.height || (thumbnailImage as any).naturalHeight || 256;
						canvas.width = width;
						canvas.height = height;
						const ctx = canvas.getContext('2d');
						if (ctx) {
							ctx.drawImage(thumbnailImage as CanvasImageSource, 0, 0);
							const thumbnailDataUrl = canvas.toDataURL('image/png');
							vrmStore.setModelPreview(loadModelId, thumbnailDataUrl);
						}
					} catch (e) {
						console.error('Failed to extract thumbnail:', e);
						setTimeout(() => generateThumbnail(loadModelId), 500);
					}
				} else {
					// No embedded thumbnail - generate one from the 3D render
					setTimeout(() => generateThumbnail(loadModelId), 500);
				}

			},
			() => {},
			(error) => {
				if (cancelled) return;
				console.error('Error loading VRM:', error);
				vrmStore.setError('Failed to load VRM model');
			}
		);

		return () => {
			// Cleanup on unmount or URL change
			cancelled = true;
			if (idleCycleTimeout) {
				clearTimeout(idleCycleTimeout);
				idleCycleTimeout = null;
			}
			if (mixer) {
				mixer.stopAllAction();
				mixer = null;
				idleAction = null;
				talkingAction = null;
				talkingClip = null;
				emoteAction = null;
			}
			// If an emote was mid-play, its 'finished' handler (bound to the old
			// mixer) never runs, so reset the flags it would have cleared —
			// otherwise currentAnimation stays stale and the next model can
			// immediately replay the leftover emote.
			if (isEmotePlaying) {
				isEmotePlaying = false;
				vrmStore.setCurrentAnimation(null);
			}
			if (vrm) {
				avatarRoot.remove(vrm.scene);
				avatarRoot.position.set(0, 0, 0);
				avatarRoot.rotation.set(0, 0, 0);
				cancelRoutine('model-unloaded');
				// Frees geometries, materials, and textures (manual traverse missed textures)
				VRMUtils.deepDispose(vrm.scene);
				vrmStore.setVrm(null);
				vrm = null;
				springBase = [];
				poseAction = null;
				poseClipCache.clear();
				activePulses = [];
				appliedNudges = [];
				transientFace = null;
				moodWeights.clear();
				heldExpression = null;
			}
		};
	});

	// Scratch vectors reused every frame — allocating three Vector3s per frame
	// (~180/sec) was needless GC pressure in the render loop.
	const scratchWorld = new THREE.Vector3();
	const scratchProjected = new THREE.Vector3();

	// === Photo-mode head tracking ===
	// Weight eases in/out so toggling never snaps the neck. The look rotation
	// is slerped over whatever the animation wrote this frame, clamped to a
	// natural range. Normalized humanoid bones face +Z in every VRM version.
	let headTrackWeight = 0;
	const headWorld = new THREE.Vector3();
	const camWorld = new THREE.Vector3();
	// Camera jiggle: orbiting excites the spring bones via a damped nudge on
	// the chest and head; the rig's own springs do the visible swinging
	const jiggleCamPos = new THREE.Vector3();
	const jiggleModelPos = new THREE.Vector3();
	let jiggleState = createJiggleState();
	let prevCamAngles: CameraAngles | null = null;
	const lookDir = new THREE.Vector3();
	const parentQuat = new THREE.Quaternion();
	const lookQuat = new THREE.Quaternion();
	const lookEuler = new THREE.Euler();

	// Update VRM each frame
	useTask((delta) => {
		if (!vrm) return;

		// Undo last frame's tap nudges before anything writes bones this frame.
		// When the mixer overwrites the rotation anyway this is a no-op; when it
		// does not, this is what makes accumulation impossible.
		for (const applied of appliedNudges) {
			applied.bone.rotation.z -= applied.z;
			applied.bone.rotation.x -= applied.x;
			if (applied.y) applied.bone.rotation.y -= applied.y;
		}
		appliedNudges.length = 0;

		// Update animation mixer
		mixer?.update(delta);

		// Tap reactions: decaying additive nudges layered over whatever the
		// mixer wrote, rendered this frame (so the body sways with the physics
		// instead of the solver and the render disagreeing, which read as
		// jitter during talking). Overlapping pulses sum; each bone's total is
		// recorded for the undo above.
		if (activePulses.length > 0) {
			const remaining: ReactionPulse[] = [];
			for (const pulse of activePulses) {
				pulse.t += delta;
				const progress = pulse.t / pulse.duration;
				if (progress >= 1) continue;
				// sin^2 has zero slope at both ends: eases in and out
				const wave = Math.sin(progress * Math.PI);
				const envelope = wave * wave * Math.exp(-1.6 * progress);
				const angle = pulse.magnitude * 0.07 * envelope;
				const z = angle * pulse.direction;
				const x = -angle * 0.4;
				pulse.bone.rotation.z += z;
				pulse.bone.rotation.x += x;
				appliedNudges.push({ bone: pulse.bone, z, x });
				remaining.push(pulse);
			}
			activePulses = remaining;
		}

		// Intentional world-motion actions: walk translation with a procedural
		// step swing, and the jump parabola. Root motion is absolute per-frame;
		// bone offsets ride appliedNudges and unwind automatically.
		if (locomotion) {
			const step = Math.min(delta, 0.1);
			const dist = WALK_SPEED * step;
			const clamped = clampWalkOffset(
				avatarRoot.position.x + locomotion.dirX * dist,
				avatarRoot.position.z + locomotion.dirZ * dist,
				WALK_MAX_RADIUS
			);
			avatarRoot.position.x = clamped.x;
			avatarRoot.position.z = clamped.z;
			const targetYaw = Math.atan2(locomotion.dirX, locomotion.dirZ);
			avatarRoot.rotation.y += shortAngle(targetYaw - avatarRoot.rotation.y) * Math.min(1, delta * 6);
			locomotion.phase += delta * Math.PI * 2 * WALK_STEP_HZ;
			const swing = Math.sin(locomotion.phase);
			const humanoid = vrm.humanoid;
			nudge(humanoid.getNormalizedBoneNode('leftUpperLeg'), -0.38 * swing, 0);
			nudge(humanoid.getNormalizedBoneNode('rightUpperLeg'), 0.38 * swing, 0);
			nudge(humanoid.getNormalizedBoneNode('leftLowerLeg'), 0.45 * Math.max(0, -swing), 0);
			nudge(humanoid.getNormalizedBoneNode('rightLowerLeg'), 0.45 * Math.max(0, swing), 0);
			nudge(humanoid.getNormalizedBoneNode('leftUpperArm'), 0.2 * swing, 0);
			nudge(humanoid.getNormalizedBoneNode('rightUpperArm'), -0.2 * swing, 0);
			locomotion.remainingMs -= delta * 1000;
			if (locomotion.remainingMs <= 0) {
				locomotion = null;
				completeRoutineStep();
				syncActionBusy();
				console.debug('[AvatarCue] action finished', 'locomotion:walk');
			}
		}
		if (jumpState) {
			jumpState.t += delta;
			const progress = jumpState.t / JUMP_DURATION;
			if (progress >= 1) {
				avatarRoot.position.y = 0;
				jumpState = null;
				completeRoutineStep();
				syncActionBusy();
				console.debug('[AvatarCue] action finished', 'jump');
			} else {
				avatarRoot.position.y = jumpArcHeight(progress, JUMP_HEIGHT);
			}
		}
		if (procedural) {
			procedural.t += delta;
			const progress = procedural.t / procedural.duration;
			if (progress >= 1) {
				console.debug('[AvatarCue] action finished', `procedural:${procedural.name}`);
				procedural = null;
				completeRoutineStep();
				syncActionBusy();
			} else {
				applyProceduralAction(procedural.name, progress);
			}
		}
		// Routine watchdog: a step that never reports completion fails instead
		// of hanging the routine (and its promise) forever.
		if (routine) {
			routine.stepElapsed += delta;
			if (routine.stepElapsed * 1000 > routine.stepDeadlineMs) {
				console.debug('[AvatarCue] step watchdog fired', routine.id);
				failCurrentStep('step exceeded its deadline', true);
			}
		}

		// Camera-driven jiggle: measure orbit velocity and advance the damped
		// spring. The offsets are applied around vrm.update() further down, so
		// only the spring bones see the movement, never the rendered skeleton.
		{
			camera.current.getWorldPosition(jiggleCamPos);
			vrm.scene.getWorldPosition(jiggleModelPos);
			const angles = cameraAngles(jiggleCamPos, jiggleModelPos);
			if (prevCamAngles && delta > 0) {
				const vel = angularVelocity(prevCamAngles, angles, delta);
				jiggleState = stepJiggle(jiggleState, vel, displayStore.physicsIntensity, delta);
			}
			prevCamAngles = angles;
		}

		// === Mood face + transient arbitration ===
		// Priority: held photo pose (exclusive) > transient reaction (tap flash,
		// emote grin, AI cue) > persistent mood > resting face. Only emotional
		// presets are written here; blink, visemes, and jawOpen are owned by
		// their own systems and the engine's protected set.
		{
			const em = vrm.expressionManager;
			if (em) {
				if (transientFace) {
					transientFace.t += delta;
					if (transientFace.t >= transientFace.duration) {
						const done = transientFace;
						transientFace = null;
						if (done.name === heldExpression) {
							// The reaction borrowed the held expression; hand it back whole.
							moodWeights.delete(done.name);
							safeSetFace(em, done.name, 1);
						}
						// Other names melt back into the mood through moodWeights below.
					}
				}

				const available = vrmStore.availableExpressions;
				const temp =
					transientFace && transientFace.weight > 0
						? {
								name: transientFace.name,
								weight: transientFace.weight * reactionEnvelope(transientFace.t, transientFace.duration)
							}
						: null;
				const held = photomodeStore.active ? heldExpression : null;
				const base = held ? emptyWeights() : moodToExpressionWeights(characterStore.state.mood);
				const blended = blendTemporaryFace(base, temp);
				const targets = new Map<string, number>();
				for (const target of resolveTargets(blended, available)) {
					targets.set(target.name, target.weight);
				}
				if (temp) {
					const direct = directTemporaryTarget(temp, available);
					if (direct) targets.set(direct.name, Math.max(targets.get(direct.name) ?? 0, direct.weight));
				}

				// Drive owned weights toward their targets.
				for (const [name, weight] of targets) {
					const next = approachWeight(moodWeights.get(name) ?? 0, weight, delta, MOOD_FACE_SPEED);
					moodWeights.set(name, next);
					safeSetFace(em, name, next);
				}
				// Decay anything without a target back to rest. A stale entry for
				// the held preset decays silently (the pose owns the actual value)
				// so exiting the pose ramps back up instead of snapping.
				for (const [name, current] of [...moodWeights]) {
					if (targets.has(name)) continue;
					const next = approachWeight(current, 0, delta, MOOD_FACE_SPEED);
					if (next <= 0.001) {
						moodWeights.delete(name);
						if (name !== held) safeSetFace(em, name, 0);
					} else {
						moodWeights.set(name, next);
						if (name !== held) safeSetFace(em, name, next);
					}
				}
			}
		}

		// Photo-mode head tracking toward the scene camera
		const trackTarget = photomodeStore.active && photomodeStore.headTracking ? 1 : 0;
		headTrackWeight += (trackTarget - headTrackWeight) * Math.min(1, delta * 5);
		if (headTrackWeight > 0.001 && camera.current) {
			const head = vrm.humanoid.getNormalizedBoneNode('head');
			if (head?.parent) {
				head.getWorldPosition(headWorld);
				camera.current.getWorldPosition(camWorld);
				lookDir.subVectors(camWorld, headWorld);
				head.parent.getWorldQuaternion(parentQuat).invert();
				lookDir.applyQuaternion(parentQuat).normalize();
				// VRM 0.x rigs face -Z where 1.0 faces +Z (the same split
				// VRM_POSE_CONFIG handles for the scene), so the whole look
				// direction mirrors on v0 models: horizontal AND vertical
				if (vrm.meta?.metaVersion !== '1') {
					lookDir.negate();
				}
				const yaw = THREE.MathUtils.clamp(Math.atan2(lookDir.x, lookDir.z), -0.65, 0.65);
				// Asymmetric pitch range: looking up reads charming well past where
				// looking down starts to double the chin. The wide bound is chosen
				// by world-space geometry (is the camera above her head), which is
				// immune to the v0/v1 sign-convention differences.
				const rawPitch = -Math.asin(THREE.MathUtils.clamp(lookDir.y, -1, 1));
				const pitchLimit = camWorld.y >= headWorld.y ? 0.85 : 0.32;
				const pitch = THREE.MathUtils.clamp(rawPitch, -pitchLimit, pitchLimit);
				lookEuler.set(pitch, yaw, 0, 'YXZ');
				lookQuat.setFromEuler(lookEuler);
				head.quaternion.slerp(lookQuat, headTrackWeight);
			}
		}

		// Camera jiggle, phase 1: displace the chest and head so the spring
		// solver inside vrm.update() reads their movement and swings hair,
		// clothes, and accessories accordingly.
		const jiggleActive =
			Math.abs(jiggleState.yaw) > 1e-5 || Math.abs(jiggleState.pitch) > 1e-5;
		let jiggleChest: THREE.Object3D | null = null;
		let jiggleHead: THREE.Object3D | null = null;
		if (jiggleActive) {
			jiggleChest =
				vrm.humanoid.getNormalizedBoneNode('upperChest') ??
				vrm.humanoid.getNormalizedBoneNode('chest') ??
				vrm.humanoid.getNormalizedBoneNode('spine');
			jiggleHead = vrm.humanoid.getNormalizedBoneNode('head');
			if (jiggleChest) {
				jiggleChest.rotation.z += jiggleState.yaw * 0.8;
				jiggleChest.rotation.y += jiggleState.yaw * 0.4;
				jiggleChest.rotation.x += jiggleState.pitch;
			}
			if (jiggleHead) {
				jiggleHead.rotation.z += jiggleState.yaw * 0.45;
				jiggleHead.rotation.y += jiggleState.yaw * 0.25;
				jiggleHead.rotation.x += jiggleState.pitch * 0.5;
			}
		}

		// Update VRM core. The delta is clamped because a huge frame gap (tab
		// refocus, window drag) otherwise launches the spring bones violently.
		vrm.update(clampFrameDelta(delta));

		// Camera jiggle, phase 2: put the skeleton straight back. The solver
		// already sampled the displaced pose; re-syncing the humanoid pushes
		// the rest pose back onto the raw render skeleton (vrm.update copied
		// the displaced one), so the body stays planted while only the spring
		// bones carry the motion.
		if (jiggleActive) {
			if (jiggleChest) {
				jiggleChest.rotation.z -= jiggleState.yaw * 0.8;
				jiggleChest.rotation.y -= jiggleState.yaw * 0.4;
				jiggleChest.rotation.x -= jiggleState.pitch;
			}
			if (jiggleHead) {
				jiggleHead.rotation.z -= jiggleState.yaw * 0.45;
				jiggleHead.rotation.y -= jiggleState.yaw * 0.25;
				jiggleHead.rotation.x -= jiggleState.pitch * 0.5;
			}
			if (jiggleChest || jiggleHead) vrm.humanoid.update();
		}

		// Track head position for 3D speech bubble
		const headBone = vrm.humanoid.getNormalizedBoneNode('head');
		if (headBone && camera.current) {
			headBone.getWorldPosition(scratchWorld);
			// Offset above and slightly in front of head
			scratchProjected.set(scratchWorld.x, scratchWorld.y + 0.25, scratchWorld.z + 0.1);
			vrmStore.setHeadPosition([scratchProjected.x, scratchProjected.y, scratchProjected.z]);

			// Project to screen coordinates (in place)
			scratchProjected.project(camera.current);
			// Convert from NDC (-1 to 1) to screen percentage (0 to 100)
			const x = (scratchProjected.x + 1) * 50;
			const y = (-scratchProjected.y + 1) * 50;
			vrmStore.setHeadScreenPosition({ x, y });
		}

		const expressionManager = vrm.expressionManager;
		if (!expressionManager) return;

		// Helper to set expression (silently ignores if not found)
		const setExpression = (name: string, value: number) => {
			try {
				expressionManager.setValue(name, value);
			} catch {
				// Expression doesn't exist on this model
			}
		};

		// === Blinking Animation (runs during idle, disabled during emotes) ===
		if (!isEmotePlaying) {
			blinkTimer += delta;

			if (!isBlinking && blinkTimer >= nextBlinkTime) {
				// Start blink
				isBlinking = true;
				blinkProgress = 0;
			}

			if (isBlinking) {
				blinkProgress += delta * 8; // Blink duration ~0.125s

				// Asymmetric blink curve: quick close (30%), slow open (70%)
				let blinkValue: number;
				if (blinkProgress < 0.3) {
					// Quick close
					blinkValue = blinkProgress / 0.3;
				} else {
					// Slow open
					blinkValue = 1 - (blinkProgress - 0.3) / 0.7;
				}

				const finalBlinkValue = Math.max(0, blinkValue);

				if (blinkProgress >= 1) {
					// End blink
					isBlinking = false;
					blinkTimer = 0;
					nextBlinkTime = Math.random() * 4 + 2; // Random 2-6 seconds
					// Try all blink expression variants
					setExpression('blink', 0);
					setExpression('Blink', 0);
					setExpression('eyeBlinkLeft', 0);
					setExpression('eyeBlinkRight', 0);
				} else {
					// Try all blink expression variants
					setExpression('blink', finalBlinkValue);
					setExpression('Blink', finalBlinkValue);
					setExpression('eyeBlinkLeft', finalBlinkValue);
					setExpression('eyeBlinkRight', finalBlinkValue);
				}
			}
		}

		// Apply expression changes
		expressionManager.update();

		// === Lip-sync Animation ===
		const visemes = lipSyncAnalyzer.update(delta);

		// Apply viseme weights - try multiple naming conventions
		// VRM 1.0 style
		setExpression('aa', visemes.aa);
		setExpression('ee', visemes.ee);
		setExpression('ih', visemes.ih);
		setExpression('oh', visemes.oh);
		setExpression('ou', visemes.ou);
		// VRM 0.x style
		setExpression('a', visemes.aa);
		setExpression('i', visemes.ih);
		setExpression('u', visemes.ou);
		setExpression('e', visemes.ee);
		setExpression('o', visemes.oh);
		// ARKit style (jawOpen for mouth)
		setExpression('jawOpen', visemes.aa * 0.7);
	});
</script>

<T is={avatarRoot} />
