// Motion controller: owns every intentional root-motion and procedural
// body state machine — locomotion (walk/run), steered goto/return-home,
// in-place turn/face-camera, jump arcs, procedural bone programs, and the
// held sitting posture. The Svelte component owns the THREE scene and the
// stores; this controller owns the motion state and the per-frame
// integration, behind a narrow injected environment (root pose, camera,
// humanoid, nudge sink) so it stays testable under node.
//
// Root motion is absolute per-frame and bone offsets ride the nudge sink,
// so interrupting (cancel/new action) can never strand a pose.
import {
	clampWalkDuration,
	clampWalkOffset,
	computeSitOffsetY,
	jumpArcHeight,
	shortAngleDelta,
	turnTargetYaw,
	yawToFacePoint,
	GOTO_ARRIVE_DIST,
	GOTO_SPEED_MPS,
	RUN_SPEED_MPS,
	RUN_STEP_HZ,
	TURN_ARRIVE_RAD,
	TURN_SPEED_RPS,
	WALK_DURATION_DEFAULT_MS,
	WALK_MAX_RADIUS,
	WALK_SPEED_MPS,
	WALK_STEP_HZ,
	DEFAULT_SEAT_HEIGHT
} from '../engine/avatar-actions.ts';
import { resolveSceneAnchor } from '../engine/scene-anchors.ts';
import {
	applyProceduralAction,
	applySittingPose,
	applyWalkSwing,
	PROCEDURAL_DURATIONS,
	smoothstep,
	type PoseHumanoid,
	type PoseNudge
} from './procedural-pose-controller.ts';

export const JUMP_DURATION = 0.65; // s
export const JUMP_HEIGHT = 0.28; // m at the apex

// Structural root pose: satisfied by a THREE.Group via a tiny adapter.
export interface MotionRoot {
	readonly position: { x: number; y: number; z: number };
	yaw: number;
}

export interface MotionEnvironment {
	root: MotionRoot;
	// Live humanoid, or null before a model loads.
	humanoid: () => PoseHumanoid | null;
	// Additive bone-offset sink (unwound by the renderer each frame).
	nudge: PoseNudge;
	// Writes the live scene-camera position; false when there is no camera.
	cameraPosition: (out: { x: number; y: number; z: number }) => boolean;
	measureHipsY: () => number;
	log?: (message: string, detail?: string) => void;
}

export type MotionCompletionType = 'locomotion' | 'goto' | 'turn' | 'jump' | 'procedural';

export interface MotionCompletion {
	type: MotionCompletionType;
	// False for ambient turns (the automatic post-walk re-face): they clear
	// silently and never complete a routine step or hold busy.
	routine: boolean;
}

interface LocomotionState {
	dirX: number;
	dirZ: number;
	remainingMs: number;
	phase: number;
	speed: number;
	stepHz: number;
}

interface GotoState {
	targetX: number;
	targetZ: number;
	phase: number;
}

interface TurnState {
	targetYaw: number;
	routineStep: boolean;
}

export function resolveGotoTarget(req: {
	anchorId?: string;
	x?: number;
	z?: number;
}): { x: number; z: number; label: string } | null {
	if (req.anchorId) {
		const anchor = resolveSceneAnchor(req.anchorId);
		if (!anchor) return null;
		return { x: anchor.x, z: anchor.z, label: anchor.id };
	}
	if (typeof req.x === 'number' && typeof req.z === 'number') {
		return { x: req.x, z: req.z, label: `${req.x.toFixed(2)},${req.z.toFixed(2)}` };
	}
	return null;
}

export class MotionController {
	private readonly env: MotionEnvironment;
	private locomotion: LocomotionState | null = null;
	private gotoState: GotoState | null = null;
	private turnState: TurnState | null = null;
	private jumpState: { t: number } | null = null;
	private procedural: { name: string; t: number; duration: number } | null = null;
	private sittingFlag = false;
	private sitOffsetY = 0;
	private sitTargetY = 0;
	private readonly scratchCam = { x: 0, y: 0, z: 0 };

	constructor(env: MotionEnvironment) {
		this.env = env;
	}

	private log(message: string, detail?: string): void {
		if (this.env.log) this.env.log(message, detail);
		else console.debug(message, detail);
	}

	get sitting(): boolean {
		return this.sittingFlag;
	}

	get hasLocomotion(): boolean {
		return this.locomotion !== null;
	}

	// Busy predicate for the gesture gate: an emote (owned by the
	// animation system) or any routine-tracked motion counts; ambient
	// turns clear silently and never hold busy.
	isBusy(emotePlaying: boolean): boolean {
		return (
			emotePlaying ||
			this.locomotion !== null ||
			this.gotoState !== null ||
			(this.turnState !== null && this.turnState.routineStep) ||
			this.jumpState !== null ||
			this.procedural !== null
		);
	}

	cancel(): void {
		this.locomotion = null;
		this.gotoState = null;
		this.turnState = null;
		this.procedural = null;
		if (this.jumpState) {
			this.jumpState = null;
			this.env.root.position.y = this.sittingFlag ? this.sitOffsetY : 0;
		}
	}

	// Standing up is implicit in every translation and jump: she stands,
	// then moves. Only an explicit sit step (or a reset/model swap) changes
	// the seated baseline otherwise.
	private autoStand(): void {
		if (!this.sittingFlag) return;
		this.sittingFlag = false;
		this.sitOffsetY = 0;
		this.sitTargetY = 0;
		this.env.root.position.y = 0;
	}

	// Root transform + posture baseline for resets and model swaps.
	resetPose(): void {
		this.cancel();
		this.sittingFlag = false;
		this.sitOffsetY = 0;
		this.sitTargetY = 0;
		this.env.root.position.x = 0;
		this.env.root.position.y = 0;
		this.env.root.position.z = 0;
		this.env.root.yaw = 0;
	}

	// Walk directions are viewer-relative: "left" means screen-left from
	// the current camera, "forward" means toward the viewer.
	private walkDirectionVector(direction: string): { x: number; z: number } {
		const root = this.env.root;
		if (this.env.cameraPosition(this.scratchCam)) {
			const dx = root.position.x - this.scratchCam.x;
			const dz = root.position.z - this.scratchCam.z;
			if (dx * dx + dz * dz > 1e-6) {
				const len = Math.hypot(dx, dz);
				const vx = dx / len;
				const vz = dz / len;
				// Screen-right = view direction × up = (-vz, 0, vx).
				const rx = -vz;
				const rz = vx;
				if (direction === 'left') return { x: -rx, z: -rz };
				if (direction === 'right') return { x: rx, z: rz };
				if (direction === 'back') return { x: vx, z: vz };
				return { x: -vx, z: -vz };
			}
		}
		if (direction === 'left') return { x: -1, z: 0 };
		if (direction === 'right') return { x: 1, z: 0 };
		if (direction === 'back') return { x: 0, z: -1 };
		return { x: 0, z: 1 };
	}

	startProcedural(name: string, anchorId?: string): boolean {
		const duration = PROCEDURAL_DURATIONS[name];
		if (!duration) return false;
		if (name === 'sit') {
			// Seat height comes from the anchor when the plan traveled to one
			// ("sit on the chair"); otherwise the default chair height.
			const seatHeight =
				(anchorId && resolveSceneAnchor(anchorId)?.seatHeight) || DEFAULT_SEAT_HEIGHT;
			this.sitTargetY = computeSitOffsetY(this.env.measureHipsY(), seatHeight);
		}
		this.procedural = { name, t: 0, duration };
		this.log('[AvatarCue] action started', `procedural:${name}`);
		return true;
	}

	startJump(): void {
		this.autoStand();
		this.jumpState = { t: 0 };
		this.log('[AvatarCue] action started', 'jump');
	}

	private startLocomotion(direction: string, durationMs: number | undefined, action: string): void {
		this.autoStand();
		const dir = this.walkDirectionVector(direction);
		const running = action === 'run';
		this.locomotion = {
			dirX: dir.x,
			dirZ: dir.z,
			remainingMs: clampWalkDuration(durationMs ?? WALK_DURATION_DEFAULT_MS),
			phase: 0,
			speed: running ? RUN_SPEED_MPS : WALK_SPEED_MPS,
			stepHz: running ? RUN_STEP_HZ : WALK_STEP_HZ
		};
		this.log('[AvatarCue] action started', `${running ? 'run' : 'walk'}:${direction}`);
	}

	startWalk(direction: string, durationMs?: number): void {
		this.startLocomotion(direction, durationMs, 'walk');
	}

	startRun(direction: string, durationMs?: number): void {
		this.startLocomotion(direction, durationMs, 'run');
	}

	startGoto(targetX: number, targetZ: number, label: string): void {
		this.autoStand();
		const clamped = clampWalkOffset(targetX, targetZ, WALK_MAX_RADIUS);
		this.gotoState = { targetX: clamped.x, targetZ: clamped.z, phase: 0 };
		this.log('[AvatarCue] action started', label);
	}

	startReturnHome(): void {
		this.startGoto(0, 0, 'return_home');
	}

	startTurn(direction: string, routineStep: boolean): boolean {
		if (direction !== 'left' && direction !== 'right' && direction !== 'back') return false;
		this.turnState = {
			targetYaw: turnTargetYaw(this.env.root.yaw, direction),
			routineStep
		};
		this.log('[AvatarCue] action started', `turn:${direction}`);
		return true;
	}

	// Yaw toward the live scene camera. Routine steps report completion;
	// the ambient post-walk re-face clears silently.
	startFaceCamera(routineStep: boolean): boolean {
		if (!this.env.cameraPosition(this.scratchCam)) return false;
		const root = this.env.root;
		this.turnState = {
			targetYaw: yawToFacePoint(
				root.position.x,
				root.position.z,
				this.scratchCam.x,
				this.scratchCam.z
			),
			routineStep
		};
		this.log('[AvatarCue] action started', 'face_camera');
		return true;
	}

	// Halt one half-played motion kind (routine step failure). Ambient
	// turns are never routine-owned, so a turn stop always clears.
	stopKind(kind: string): void {
		if (kind === 'walk') this.locomotion = null;
		if (kind === 'return_home' || kind === 'goto') this.gotoState = null;
		if (kind === 'turn' || kind === 'face_camera') this.turnState = null;
		if (kind === 'jump') {
			this.jumpState = null;
			this.env.root.position.y = this.sittingFlag ? this.sitOffsetY : 0;
		}
		if (kind === 'procedural') this.procedural = null;
	}

	// Advance every motion by `delta` seconds. Returns completions in
	// check order; the caller maps them to routine completions (routine
	// ones), ambient re-facing (locomotion/goto arrivals), and busy sync.
	update(delta: number): MotionCompletion[] {
		const completions: MotionCompletion[] = [];
		const root = this.env.root;
		const humanoid = this.env.humanoid();
		const nudge = this.env.nudge;

		if (this.locomotion) {
			const step = Math.min(delta, 0.1);
			const dist = this.locomotion.speed * step;
			const clamped = clampWalkOffset(
				root.position.x + this.locomotion.dirX * dist,
				root.position.z + this.locomotion.dirZ * dist,
				WALK_MAX_RADIUS
			);
			root.position.x = clamped.x;
			root.position.z = clamped.z;
			const targetYaw = Math.atan2(this.locomotion.dirX, this.locomotion.dirZ);
			root.yaw += shortAngleDelta(targetYaw - root.yaw) * Math.min(1, delta * 6);
			this.locomotion.phase += delta * Math.PI * 2 * this.locomotion.stepHz;
			applyWalkSwing(humanoid, nudge, Math.sin(this.locomotion.phase));
			this.locomotion.remainingMs -= delta * 1000;
			if (this.locomotion.remainingMs <= 0) {
				this.locomotion = null;
				completions.push({ type: 'locomotion', routine: true });
				this.log('[AvatarCue] action finished', 'locomotion');
			}
		}
		if (this.gotoState) {
			const dx = this.gotoState.targetX - root.position.x;
			const dz = this.gotoState.targetZ - root.position.z;
			const dist = Math.hypot(dx, dz);
			if (dist <= GOTO_ARRIVE_DIST) {
				this.gotoState = null;
				completions.push({ type: 'goto', routine: true });
				this.log('[AvatarCue] action finished', 'goto');
			} else {
				const step = Math.min(delta, 0.1);
				const move = Math.min(dist, GOTO_SPEED_MPS * step);
				const clamped = clampWalkOffset(
					root.position.x + (dx / dist) * move,
					root.position.z + (dz / dist) * move,
					WALK_MAX_RADIUS
				);
				root.position.x = clamped.x;
				root.position.z = clamped.z;
				const targetYaw = Math.atan2(dx, dz);
				root.yaw += shortAngleDelta(targetYaw - root.yaw) * Math.min(1, delta * 6);
				this.gotoState.phase += delta * Math.PI * 2 * WALK_STEP_HZ;
				applyWalkSwing(humanoid, nudge, Math.sin(this.gotoState.phase));
			}
		}
		if (this.turnState) {
			const diff = shortAngleDelta(this.turnState.targetYaw - root.yaw);
			const maxStep = TURN_SPEED_RPS * Math.min(delta, 0.1);
			if (Math.abs(diff) <= Math.max(TURN_ARRIVE_RAD, maxStep)) {
				const wasRoutine = this.turnState.routineStep;
				const arrived = this.turnState.targetYaw;
				this.turnState = null;
				// Snap to the target, normalized so long sessions never stack spins.
				root.yaw = shortAngleDelta(arrived);
				completions.push({ type: 'turn', routine: wasRoutine });
				this.log('[AvatarCue] action finished', 'turn');
			} else {
				root.yaw += Math.sign(diff) * maxStep;
			}
		}
		if (this.jumpState) {
			this.jumpState.t += delta;
			const progress = this.jumpState.t / JUMP_DURATION;
			if (progress >= 1) {
				root.position.y = 0;
				this.jumpState = null;
				completions.push({ type: 'jump', routine: true });
				this.log('[AvatarCue] action finished', 'jump');
			} else {
				root.position.y = jumpArcHeight(progress, JUMP_HEIGHT);
			}
		}
		if (this.procedural) {
			this.procedural.t += delta;
			const progress = this.procedural.t / this.procedural.duration;
			if (progress >= 1) {
				// Sit/stand hand a persistent posture to the frame loop; every
				// other procedural fully unwinds via the nudge sink.
				if (this.procedural.name === 'sit') {
					this.sittingFlag = true;
					this.sitOffsetY = this.sitTargetY;
					root.position.y = this.sitOffsetY;
				} else if (this.procedural.name === 'stand') {
					this.sittingFlag = false;
					this.sitOffsetY = 0;
					this.sitTargetY = 0;
					root.position.y = 0;
				}
				this.log('[AvatarCue] action finished', `procedural:${this.procedural.name}`);
				this.procedural = null;
				completions.push({ type: 'procedural', routine: true });
			} else {
				if (this.procedural.name === 'sit') {
					root.position.y = this.sitTargetY * smoothstep(0, 0.8, progress);
				} else if (this.procedural.name === 'stand') {
					root.position.y = this.sitOffsetY * (1 - smoothstep(0, 0.8, progress));
				}
				applyProceduralAction(humanoid, nudge, this.procedural.name, progress);
			}
		}
		// Held sitting posture: re-applied every frame once the sit transition
		// hands off, until stand or any locomotion (which auto-stands first).
		if (
			this.sittingFlag &&
			!this.procedural &&
			!this.locomotion &&
			!this.gotoState &&
			!this.jumpState
		) {
			root.position.y = this.sitOffsetY;
			applySittingPose(humanoid, nudge, 1);
		}
		return completions;
	}
}
