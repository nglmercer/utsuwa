<script lang="ts">
	import { T, useThrelte, useTask } from '@threlte/core';
	import { GLTFLoader } from 'three/addons/loaders/GLTFLoader.js';
	import { VRMLoaderPlugin, VRMUtils, type VRM } from '@pixiv/three-vrm';
	import { vrmStore } from '$lib/stores/vrm.svelte';
	import { ttsStore } from '$lib/stores/tts.svelte';
	import { displayStore } from '$lib/stores/display.svelte';
	import { photomodeStore } from '$lib/stores/photomode.svelte';
	import { characterStore } from '$lib/stores/character.svelte';
	import {
		clampWalkDuration,
		GOTO_SPEED_MPS,
		shortAngleDelta,
		TURN_ARRIVE_RAD,
		WALK_DURATION_DEFAULT_MS,
		yawToFacePoint
	} from '$lib/engine/avatar-actions';
	import {
		JUMP_DURATION,
		MotionController,
		resolveGotoTarget,
		type MotionRoot
	} from '$lib/avatar/motion-controller';
	import {
		createRoutineController,
		type RoutineStepInput
	} from '$lib/avatar/routine-controller';
	import { PROCEDURAL_DURATIONS } from '$lib/avatar/procedural-pose-controller';
	import type { PoseHumanoid } from '$lib/avatar/procedural-pose-controller';
	import { AnimationController } from '$lib/avatar/animation-controller';
	import {
		ExpressionController,
		type ExpressionHumanoid
	} from '$lib/avatar/expression-controller';
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

	const currentAnimation = $derived(vrmStore.currentAnimation);
	// Talking animation plays when TTS is speaking OR when text-based talking is triggered
	const shouldTalk = $derived(ttsStore.isSpeaking || vrmStore.isTalking);

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



	// Photo enter/exit: the animation controller freezes the stance on the
	// way in and hands back to the idle cycler on the way out. Entering
	// also cancels any routine — a posed avatar must not keep dancing.
	$effect(() => {
		const active = photomodeStore.active;
		untrack(() => {
			const transition = animation.setPhotoActive(active);
			if (transition === 'entered') cancelRoutine('photo-mode');
		});
	});

	// Apply pose selections while photo mode is active.
	$effect(() => {
		const active = photomodeStore.active;
		const poseId = photomodeStore.selectedPoseId;
		if (!active) return;
		untrack(() => {
			void animation.applyPose(poseId);
		});
	});

	// Held photo expression: applied exclusively, cleared on change and
	// exit. Tap reactions layer a transient expression on top and restore it.
	$effect(() => {
		const active = photomodeStore.active;
		const name = photomodeStore.selectedExpression;
		untrack(() => {
			expression.setPhotoExpression(active, name);
		});
	});

	// Every nudge applied to a bone is explicitly undone at the start of the
	// next frame, so nothing can accumulate no matter what the mixer weights are.
	let appliedNudges: Array<{ bone: THREE.Object3D; z: number; x: number; y?: number }> = [];

	// Transient faces (tap flash, emote grin, AI cue) stage into the
	// expression controller, which arbitrates them against the mood face.
	$effect(() => {
		const request = vrmStore.expressionRequest;
		untrack(() => {
			expression.stageExpression(
				request
					? {
							expression: request.expression,
							intensity: request.intensity,
							durationMs: request.durationMs,
							seq: request.seq
						}
					: null
			);
		});
	});

	// Tap reactions: an expression flash plus a decaying rotation nudge
	// whose motion the spring bones inherit.
	$effect(() => {
		const request = vrmStore.reactionRequest;
		if (!request) return;
		untrack(() => {
			expression.stageReaction(request.zone);
		});
	});

	// === Intentional avatar actions (procedural / jump / walk) ===
	// At most one runs at a time; staging a new one cancels the current.
	// Motion state and per-frame integration live in MotionController (see
	// src/lib/avatar/); this component owns the THREE scene, the stores,
	// and the wiring between them.
	const motionRoot: MotionRoot = {
		position: avatarRoot.position,
		get yaw() {
			return avatarRoot.rotation.y;
		},
		set yaw(value: number) {
			avatarRoot.rotation.y = value;
		}
	};

	function nudge(bone: THREE.Object3D | null, x: number, z: number, y = 0) {
		if (!bone) return;
		bone.rotation.x += x;
		bone.rotation.z += z;
		if (y) bone.rotation.y += y;
		appliedNudges.push({ bone, z, x, y });
	}

	const scratchWalkDir = new THREE.Vector3();

	function measureHipsWorldY(): number {
		const hips = vrm?.humanoid.getNormalizedBoneNode('hips');
		if (!hips) return 0.9;
		const p = new THREE.Vector3();
		hips.getWorldPosition(p);
		return p.y;
	}

	const motion = new MotionController({
		root: motionRoot,
		humanoid: () => (vrm?.humanoid ?? null) as unknown as PoseHumanoid | null,
		nudge: (bone, x, z, y) => nudge(bone as THREE.Object3D | null, x, z, y ?? 0),
		cameraPosition: (out) => {
			if (!camera.current) return false;
			camera.current.getWorldPosition(scratchWalkDir);
			out.x = scratchWalkDir.x;
			out.y = scratchWalkDir.y;
			out.z = scratchWalkDir.z;
			return true;
		},
		measureHipsY: () => measureHipsWorldY()
	});

	let actionSeqSeen = 0;

	function syncActionBusy() {
		vrmStore.setActionBusy(motion.isBusy(animation.isEmotePlaying));
	}

	function startAmbientFaceCamera(): void {
		if (photomodeStore.active || !camera.current || !vrm) return;
		camera.current.getWorldPosition(scratchWalkDir);
		const target = yawToFacePoint(
			avatarRoot.position.x,
			avatarRoot.position.z,
			scratchWalkDir.x,
			scratchWalkDir.z
		);
		// Already facing: skip the turn entirely instead of micro-rotating.
		if (Math.abs(shortAngleDelta(target - avatarRoot.rotation.y)) < TURN_ARRIVE_RAD * 2) return;
		motion.startFaceCamera(false);
	}

	function handleAvatarActionRequest(req: {
		kind: string;
		action: string;
		direction?: string;
		durationMs?: number;
		anchorId?: string;
		x?: number;
		z?: number;
	}) {
		if (req.kind === 'stop') {
			cancelRoutine('stopped');
			vrmStore.setCurrentAnimation(null);
			return;
		}
		if (req.kind === 'reset') {
			cancelRoutine('reset');
			motion.resetPose();
			return;
		}
		// A new action interrupts the current one (and any routine); every
		// program below is absolute per-frame, so interruption strands nothing.
		cancelRoutine('superseded');
		if (req.kind === 'procedural') {
			if (!motion.startProcedural(req.action, req.anchorId)) return;
		} else if (req.kind === 'jump') {
			motion.startJump();
		} else if (req.kind === 'walk') {
			if (req.action === 'run') motion.startRun(req.direction ?? 'forward', req.durationMs);
			else motion.startWalk(req.direction ?? 'forward', req.durationMs);
		} else if (req.kind === 'return_home') {
			motion.startReturnHome();
		} else if (req.kind === 'turn') {
			if (!motion.startTurn(req.direction ?? 'back', false)) return;
		} else if (req.kind === 'face_camera') {
			if (!motion.startFaceCamera(false)) return;
		} else if (req.kind === 'goto') {
			const target = resolveGotoTarget(req);
			if (!target) return;
			motion.startGoto(target.x, target.z, `goto:${target.label}`);
		} else {
			return;
		}
		syncActionBusy();
	}

	// === Avatar routines: ordered step sequences with completion ===
	// The step ledger, watchdog, and finish truth table live in the routine
	// controller; this wiring starts/halts physical motion per step and
	// records the authoritative receipt into the VRM store.
	let routineAwaitingEmote: string | null = null;
	let routineSeqSeen = 0;

	// Watchdog budget for a steered arrival: travel time at goto speed plus
	// a fixed margin, floored so adjacent anchors never starve the step.
	function gotoDeadlineMs(targetX: number, targetZ: number): number {
		const dist = Math.hypot(targetX - avatarRoot.position.x, targetZ - avatarRoot.position.z);
		return Math.min(15000, Math.max(3500, (dist / GOTO_SPEED_MPS) * 1000 + 2500));
	}

	const routineController = createRoutineController({
		executor: {
			startStep(step) {
				let started = false;
				let deadline = 5000;
				if (step.kind === 'procedural') {
					started = motion.startProcedural(step.action, step.anchorId);
					deadline = (PROCEDURAL_DURATIONS[step.action] ?? 1) * 1000 + 1500;
				} else if (step.kind === 'jump') {
					motion.startJump();
					started = true;
					deadline = JUMP_DURATION * 1000 + 1500;
				} else if (step.kind === 'walk') {
					if (step.action === 'run') motion.startRun(step.direction ?? 'forward', step.durationMs);
					else motion.startWalk(step.direction ?? 'forward', step.durationMs);
					started = true;
					deadline = clampWalkDuration(step.durationMs ?? WALK_DURATION_DEFAULT_MS) + 1500;
				} else if (step.kind === 'return_home') {
					motion.startReturnHome();
					started = true;
					deadline = gotoDeadlineMs(0, 0);
				} else if (step.kind === 'turn') {
					started = motion.startTurn(step.direction ?? 'back', true);
					deadline = 4000;
				} else if (step.kind === 'face_camera') {
					started = motion.startFaceCamera(true);
					deadline = 4000;
				} else if (step.kind === 'goto') {
					const target = resolveGotoTarget(step);
					if (target) {
						motion.startGoto(target.x, target.z, `goto:${target.label}`);
						started = true;
						deadline = gotoDeadlineMs(target.x, target.z);
					}
				} else if (step.kind === 'emote' && step.url) {
					routineAwaitingEmote = step.url;
					vrmStore.setCurrentAnimation(step.url);
					started = true;
					deadline = 15000;
				}
				return { started, deadlineMs: deadline };
			},
			stopStep(step) {
				motion.stopKind(step.kind);
				if (step.kind === 'emote') {
					routineAwaitingEmote = null;
					if (step.url && vrmStore.currentAnimation === step.url) vrmStore.setCurrentAnimation(null);
				}
			}
		},
		reporter: {
			recordStep: (result) => vrmStore.recordRoutineStep(result),
			recordResult: (result) => vrmStore.recordRoutineResult(result),
			now: () => Date.now()
		},
		getEndPosition: () => ({
			x: avatarRoot.position.x,
			y: avatarRoot.position.y,
			z: avatarRoot.position.z
		}),
		onChange: () => {
			if (!routineController.active) routineAwaitingEmote = null;
			syncActionBusy();
		}
	});

	function cancelRoutine(reason: string) {
		// Drop a routine-owned emote selection so a pending load cannot
		// play after the cancel; a manual selection is left untouched.
		if (
			routineController.active &&
			routineAwaitingEmote &&
			vrmStore.currentAnimation === routineAwaitingEmote
		) {
			vrmStore.setCurrentAnimation(null);
		}
		routineController.cancel(reason);
		motion.cancel();
		syncActionBusy();
	}

	function startRoutine(request: {
		id: string;
		steps: RoutineStepInput[];
		policy?: { continueOnFailure?: boolean };
	}) {
		routineController.start(request.id, request.steps, request.policy);
	}

	// Clip animation (idle cycle, talking loop, emotes, photo poses) and
	// the face (reactions, mood, blink, visemes) live in their controllers
	// (see src/lib/avatar/); this wiring connects them to the stores, the
	// routine ledger, and the live humanoid.
	const animation = new AnimationController({
		idleUrls: () => vrmStore.idleAnimationUrls,
		talkingUrl: () => vrmStore.talkingAnimationUrl,
		findAnimation: (id) => vrmStore.availableAnimations.find((a) => a.url === id || a.id === id),
		readEmoteSelection: () => vrmStore.currentAnimation,
		clearEmoteSelection: () => vrmStore.setCurrentAnimation(null),
		requestFace: (expression, intensity, durationMs) =>
			vrmStore.requestExpression({ expression, intensity, durationMs }),
		photoActive: () => photomodeStore.active,
		shouldTalk: () => shouldTalk,
		hasActiveRoutine: () => routineController.active !== null,
		isRoutineAwaitingEmote: (url) => routineController.active !== null && routineAwaitingEmote === url,
		isAnyEmoteAwaited: () => routineController.active !== null && routineAwaitingEmote !== null,
		onEmoteClipStarted: (url, deadlineMs) => {
			if (routineController.active && routineAwaitingEmote === url) {
				routineController.setStepDeadline(deadlineMs);
				routineController.resetStepElapsed();
			}
		},
		onEmoteFinished: (url) => {
			if (routineController.active && routineAwaitingEmote === url) {
				routineAwaitingEmote = null;
				routineController.completeCurrentStep();
			}
		},
		onEmoteLoadFailed: () => {
			if (routineController.active && routineAwaitingEmote) {
				routineAwaitingEmote = null;
				routineController.failCurrentStep('emote load failed');
			}
		},
		onManualEmoteWhileRoutine: () => cancelRoutine('superseded'),
		onBusyChanged: () => syncActionBusy()
	});

	const expression = new ExpressionController({
		getHumanoid: () => (vrm?.humanoid ?? null) as unknown as ExpressionHumanoid | null,
		nudge: (bone, x, z, y) => nudge(bone as THREE.Object3D | null, x, z, y ?? 0),
		getRelationshipStage: () => characterStore.state.relationshipStage,
		shouldTalk: () => shouldTalk,
		requestFace: (expressionName, intensity, durationMs) =>
			vrmStore.requestExpression({ expression: expressionName, intensity, durationMs }),
		getExpressionManager: () => vrm?.expressionManager ?? null,
		getAvailableExpressionNames: () =>
			vrm?.expressionManager?.expressions.map((e) => e.expressionName) ?? []
	});

	$effect(() => {
		const request = vrmStore.actionRequest;
		if (!request) return;
		untrack(() => {
			if (!animation.isAttached) return;
			if (request.seq === actionSeqSeen) return;
			actionSeqSeen = request.seq;
			handleAvatarActionRequest(request);
		});
	});

	$effect(() => {
		const request = vrmStore.routineRequest;
		if (!request) return;
		untrack(() => {
			if (!animation.isAttached) return;
			if (request.seq === routineSeqSeen) return;
			routineSeqSeen = request.seq;
			startRoutine(request);
		});
	});

	// Update lip-sync analyser when TTS state changes
	$effect(() => {
		lipSyncAnalyzer.setAnalyser(ttsStore.currentAnalyser);
	});

	// Switch between idle and talking animations based on speaking state.
	// A held photo pose must not be stomped by TTS either; exit restores.
	// Photo activity is a tracked read: exiting photo mode while TTS is
	// still speaking must re-run the switch so the talking loop fades back
	// in (the exit path only resumes the idle cycler when silent).
	$effect(() => {
		const speaking = shouldTalk;
		const photoOpen = photomodeStore.active;
		untrack(() => {
			animation.setTalking(speaking, photoOpen);
		});
	});

	// Play emote animations when the selection changes.
	$effect(() => {
		const animId = currentAnimation;
		untrack(() => {
			animation.playEmoteSelection(animId);
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
				motion.resetPose();
				avatarRoot.add(loadedVrm.scene);
				animation.attach(loadedVrm, new THREE.AnimationMixer(loadedVrm.scene));
				vrmStore.setVrm(loadedVrm);
				vrmStore.setLoading(false);

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
			// Cleanup on unmount or URL change. Detaching the animation
			// controller stops timers and actions and clears a mid-play
			// emote selection whose 'finished' handler (bound to the old
			// mixer) would otherwise never run.
			cancelled = true;
			animation.detach();
			if (vrm) {
				avatarRoot.remove(vrm.scene);
				motion.resetPose();
				cancelRoutine('model-unloaded');
				// Frees geometries, materials, and textures (manual traverse missed textures)
				VRMUtils.deepDispose(vrm.scene);
				vrmStore.setVrm(null);
				vrm = null;
				springBase = [];
				expression.reset();
				appliedNudges = [];
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

		// Advance clip animation (idle cycle, talking loop, emotes, poses).
		animation.update(delta);

		// Tap reactions: decaying additive nudges layered over whatever
		// the mixer wrote, rendered this frame so the body sways with the
		// physics. Overlapping pulses sum; totals ride the undo buffer.
		expression.updatePulses(delta);

		// Intentional world-motion actions: walk/run translation, steered
		// goto/return-home arrivals, and in-place turns with a procedural step
		// swing, plus the jump parabola. Root motion is absolute per-frame;
		// bone offsets ride appliedNudges and unwind automatically.
		for (const completion of motion.update(delta)) {
			if (completion.routine) routineController.completeCurrentStep();
			syncActionBusy();
			// Walks end in profile; ease back to the viewer unless a queued
			// routine step (or photo mode) owns the body next.
			if (
				(completion.type === 'locomotion' || completion.type === 'goto') &&
				!routineController.active
			) {
				startAmbientFaceCamera();
			}
		}
		// Live root pose for the prompt and camera follow. The store skips
		// the write unless something moved, so this stays cheap per frame.
		vrmStore.setAvatarPose({
			x: avatarRoot.position.x,
			y: avatarRoot.position.y,
			z: avatarRoot.position.z,
			yaw: avatarRoot.rotation.y,
			sitting: motion.sitting
		});
		// Routine watchdog: a step that never reports completion fails instead
		// of hanging the routine (and its promise) forever. Driven by render
		// time, so a hidden tab pauses deadlines together with motion.
		routineController.tick(delta);

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

		// Mood face + transient arbitration. Priority: held photo pose
		// (exclusive) > transient reaction (tap flash, emote grin, AI cue) >
		// persistent mood > resting face. Only emotional presets are written
		// here; blink, visemes, and jawOpen are owned by their own systems.
		{
			const em = vrm.expressionManager;
			if (em) {
				expression.updateFace(delta, {
					expressionManager: em,
					availableExpressions: vrmStore.availableExpressions,
					mood: characterStore.state.mood,
					photoActive: photomodeStore.active
				});
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

		// Blinking (suppressed while an emote plays), the expression manager
		// update, then lip-sync visemes across VRM 1.0, 0.x, and ARKit names.
		expression.updateBlinkAndVoice(delta, {
			expressionManager,
			emotePlaying: animation.isEmotePlaying,
			visemes: lipSyncAnalyzer.update(delta)
		});
	});
</script>

<T is={avatarRoot} />
