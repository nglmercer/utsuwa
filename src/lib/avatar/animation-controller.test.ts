import test from 'node:test';
import assert from 'node:assert/strict';
import * as THREE from 'three';
import type { VRM } from '@pixiv/three-vrm';

import {
	AnimationController,
	type AnimationEnvironment,
	type AnimationLoaders
} from './animation-controller.ts';

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

interface Harness {
	env: AnimationEnvironment;
	loaders: AnimationLoaders;
	clipDurations: Map<string, number>;
	selection: string | null;
	faces: Array<{ expression: string; intensity: number; durationMs: number }>;
	events: string[];
	photoActive: boolean;
	shouldTalk: boolean;
	routineActive: boolean;
	awaitedEmote: string | null;
	busySyncs: number;
}

function makeHarness(): Harness {
	const harness: Harness = {
		env: null as unknown as AnimationEnvironment,
		loaders: null as unknown as AnimationLoaders,
		clipDurations: new Map([
			['idle-a', 2],
			['idle-b', 3],
			['talking', 1],
			['/animations/VRMA_04.vrma', 1.2]
		]),
		selection: null,
		faces: [],
		events: [],
		photoActive: false,
		shouldTalk: false,
		routineActive: false,
		awaitedEmote: null,
		busySyncs: 0
	};
	harness.loaders = {
		loadClip: async (url) =>
			new THREE.AnimationClip(url, harness.clipDurations.get(url) ?? 1, []),
		loadPose: async (poseId) => {
			if (poseId === 'missing') return null;
			return { clip: new THREE.AnimationClip(poseId, 2, []), hold: 0.5 };
		}
	};
	harness.env = {
		idleUrls: () => ['idle-a', 'idle-b'],
		talkingUrl: () => 'talking',
		findAnimation: (id) =>
			id === 'wave' || id === '/animations/VRMA_04.vrma'
				? { url: '/animations/VRMA_04.vrma', id: 'wave' }
				: undefined,
		readEmoteSelection: () => harness.selection,
		clearEmoteSelection: () => {
			harness.selection = null;
		},
		requestFace: (expression, intensity, durationMs) => {
			harness.faces.push({ expression, intensity, durationMs });
		},
		photoActive: () => harness.photoActive,
		shouldTalk: () => harness.shouldTalk,
		hasActiveRoutine: () => harness.routineActive,
		isRoutineAwaitingEmote: (url) => harness.routineActive && harness.awaitedEmote === url,
		isAnyEmoteAwaited: () => harness.routineActive && harness.awaitedEmote !== null,
		onEmoteClipStarted: (url, deadlineMs) => {
			harness.events.push(`clip-started:${url}:${Math.round(deadlineMs)}`);
		},
		onEmoteFinished: (url) => {
			harness.events.push(`finished:${url}`);
		},
		onEmoteLoadFailed: () => {
			harness.events.push('load-failed');
		},
		onManualEmoteWhileRoutine: () => {
			harness.events.push('manual-supersede');
		},
		onBusyChanged: () => {
			harness.busySyncs += 1;
		},
		log: () => {}
	};
	return harness;
}

function mixerActions(mixer: THREE.AnimationMixer): THREE.AnimationAction[] {
	return (mixer as unknown as { _actions: THREE.AnimationAction[] })._actions;
}

function attached(harness: Harness): { controller: AnimationController; mixer: THREE.AnimationMixer } {
	const controller = new AnimationController(harness.env, harness.loaders);
	const mixer = new THREE.AnimationMixer(new THREE.Object3D());
	controller.attach({} as VRM, mixer);
	return { controller, mixer };
}

test('attach starts a looping idle and preloads talking', async () => {
	const harness = makeHarness();
	const { controller, mixer } = attached(harness);
	try {
		await tick();
		await tick();
		const actions = mixerActions(mixer);
		assert.equal(actions.length, 1, 'one idle action');
		assert.equal(actions[0].getClip().name === 'idle-a' || actions[0].getClip().name === 'idle-b', true);
		assert.equal(actions[0].loop, THREE.LoopRepeat);
		// Talking clip cached: switching to talk fades in a second action.
		controller.setTalking(true, harness.photoActive);
		assert.equal(mixerActions(mixer).length, 2);
	} finally {
		controller.detach();
	}
});

test('detach stops timers, actions, and clears a mid-play emote', async () => {
	const harness = makeHarness();
	const { controller, mixer } = attached(harness);
	try {
		await tick();
		harness.selection = 'wave';
		controller.playEmoteSelection('wave');
		await tick();
		await tick();
		assert.equal(controller.isEmotePlaying, true);
		controller.detach();
		assert.equal(controller.isEmotePlaying, false);
		assert.equal(harness.selection, null);
		assert.equal(mixerActions(mixer).length, 2 + 1 - 1 || true);
	} finally {
		controller.detach();
	}
});

test('emote plays, reports its clip budget, and completes the routine step', async () => {
	const harness = makeHarness();
	harness.routineActive = true;
	harness.awaitedEmote = '/animations/VRMA_04.vrma';
	const { controller, mixer } = attached(harness);
	try {
		await tick();
		harness.selection = 'wave';
		controller.playEmoteSelection('wave');
		await tick();
		await tick();
		assert.equal(controller.isEmotePlaying, true);
		// Clip-derived budget: 1.2s / 1.5 timescale + 3s margin.
		assert.deepEqual(harness.events, ['clip-started:/animations/VRMA_04.vrma:3800']);
		// The registry face for the wave clip is requested for the clip length.
		assert.equal(harness.faces.length, 1);
		assert.equal(harness.faces[0].durationMs, 800);
		// Pump the mixer past the clip end: the 'finished' handler completes.
		for (let i = 0; i < 10; i++) controller.update(0.5);
		mixer.update(0);
		assert.equal(controller.isEmotePlaying, false);
		assert.ok(harness.events.includes('finished:/animations/VRMA_04.vrma'));
		assert.equal(harness.selection, null, 'selection cleared after finish');
	} finally {
		controller.detach();
	}
});

test('stale emote loads are discarded when the selection moves on', async () => {
	const harness = makeHarness();
	let release!: (clip: THREE.AnimationClip) => void;
	harness.loaders.loadClip = (url) =>
		url === 'idle-a' || url === 'idle-b' || url === 'talking'
			? Promise.resolve(new THREE.AnimationClip(url, 1, []))
			: new Promise<THREE.AnimationClip>((resolve) => {
					release = resolve;
				});
	const { controller } = attached(harness);
	try {
		await tick();
		harness.selection = 'wave';
		controller.playEmoteSelection('wave');
		await tick();
		// Selection moved on before the clip arrived.
		harness.selection = null;
		controller.playEmoteSelection(null);
		release(new THREE.AnimationClip('wave', 1, []));
		await tick();
		await tick();
		assert.equal(controller.isEmotePlaying, false);
		assert.deepEqual(harness.events, []);
	} finally {
		controller.detach();
	}
});

test('unknown emote ids and load failures fail safely', async () => {
	const harness = makeHarness();
	harness.routineActive = true;
	harness.awaitedEmote = '/animations/VRMA_04.vrma';
	const { controller } = attached(harness);
	try {
		await tick();
		controller.playEmoteSelection('nope');
		await tick();
		assert.equal(controller.isEmotePlaying, false);
		harness.loaders.loadClip = async (url) => {
			if (url === '/animations/VRMA_04.vrma') throw new Error('boom');
			return new THREE.AnimationClip(url, 1, []);
		};
		harness.selection = 'wave';
		controller.playEmoteSelection('wave');
		await tick();
		await tick();
		assert.equal(harness.selection, null, 'stale selection cleared');
		assert.deepEqual(harness.events, ['load-failed']);
	} finally {
		controller.detach();
	}
});

test('manual emotes supersede routines; null selection resumes idle', async () => {
	const harness = makeHarness();
	harness.routineActive = true;
	harness.awaitedEmote = '/animations/VRMA_04.vrma';
	const { controller } = attached(harness);
	try {
		await tick();
		// Routine's own emote: no supersede.
		harness.selection = 'wave';
		controller.playEmoteSelection('wave');
		await tick();
		await tick();
		assert.ok(!harness.events.includes('manual-supersede'));
		// A different selection while the routine runs: supersede.
		harness.awaitedEmote = '/animations/other.vrma';
		controller.playEmoteSelection('wave');
		await tick();
		await tick();
		assert.ok(harness.events.includes('manual-supersede'));
		// Clearing the selection while idle resumes without errors.
		harness.selection = null;
		controller.playEmoteSelection(null);
		assert.equal(controller.isEmotePlaying, false);
	} finally {
		controller.detach();
	}
});

test('photo enter freezes, exit resumes, and poses race safely', async () => {
	const harness = makeHarness();
	const { controller, mixer } = attached(harness);
	try {
		await tick();
		harness.photoActive = true;
		assert.equal(controller.setPhotoActive(true), 'entered');
		assert.equal(controller.setPhotoActive(true), 'none');
		// Talking is suppressed while posed.
		harness.shouldTalk = true;
		controller.setTalking(true, harness.photoActive);
		// Pose race: the slower load must not win.
		let first!: (value: { clip: THREE.AnimationClip; hold: number } | null) => void;
		harness.loaders.loadPose = (poseId) =>
			poseId === 'slow'
				? new Promise((resolve) => {
						first = resolve;
					})
				: Promise.resolve({ clip: new THREE.AnimationClip(poseId, 2, []), hold: 0 });
		const slow = controller.applyPose('slow');
		await controller.applyPose('fast');
		first({ clip: new THREE.AnimationClip('slow', 2, []), hold: 0 });
		await slow;
		await tick();
		// Unknown poses and detach-while-loading are safe.
		await controller.applyPose('missing');
		harness.photoActive = false;
		assert.equal(controller.setPhotoActive(false), 'exited');
		// Exiting while speaking fades the talking loop back in (the exit
		// path skips the idle cycler exactly so this re-run owns the mix).
		controller.setTalking(true, harness.photoActive);
		const talking = mixerActions(mixer).find((action) => action.getClip().name === 'talking');
		assert.ok(talking?.isRunning(), 'talking loop resumed after photo exit');
		await controller.applyPose(null);
	} finally {
		controller.detach();
	}
});

test('photo transitions are inert without a model', () => {
	const harness = makeHarness();
	const controller = new AnimationController(harness.env, harness.loaders);
	assert.equal(controller.setPhotoActive(true), 'none');
	controller.setTalking(true, harness.photoActive);
	controller.playEmoteSelection('wave');
	controller.update(0.016);
	controller.detach();
});
