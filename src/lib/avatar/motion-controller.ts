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
import { VRM1_MOTION_BASIS, type HumanoidMotionBasis } from './humanoid-motion-basis.ts';

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

// Captured when a sit/stand transition starts so root Y and seated weight
// interpolate between measured endpoints. Never assume a transition begins
// from standing (Y=0, weight=0): reversals begin mid-pose.
interface PostureBaseline {
	fromRootY: number;
	targetRootY: number;
	fromWeight: number;
	targetWeight: number;
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
	// Continuous seated blend (0 standing .. 1 fully seated), tracked every
	// frame a posture transition runs so reversals and cancels resume from
	// the true current pose instead of an assumed endpoint.
	private seatedWeight = 0;
	private postureBaseline: PostureBaseline | null = null;
	// Movement deferred behind a stand transition (seated walk/run/goto/jump
	// stands first, then moves — never teleports upright).
	private pendingAfterStand: (() => void) | null = null;
	// Completions for idempotent no-ops (sit while seated, ...): the caller
	// was promised a routine completion, so one is emitted next update.
	private instantCompletions: MotionCompletion[] = [];
	private basis: HumanoidMotionBasis = VRM1_MOTION_BASIS;
	private readonly scratchCam = { x: 0, y: 0, z: 0 };

	constructor(env: MotionEnvironment) {
		this.env = env;
	}

	// Anatomical rotation convention for the loaded rig. The renderer sets
	// this from the VRM meta version at load; programs using it pose VRM0
	// and VRM1 identically despite opposite forward conventions.
	setMotionBasis(basis: HumanoidMotionBasis): void {
		this.basis = basis;
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
		this.restorePostureAfterHalt();
		this.procedural = null;
		this.pendingAfterStand = null;
		if (this.jumpState) {
			this.jumpState = null;
			this.env.root.position.y = this.sittingFlag ? this.sitOffsetY : 0;
		}
	}

	// A halted posture transition reverts to its origin posture: a sit that
	// never completed was never seated (standing Y=0), a stand that never
	// completed is still seated (seated baseline). No intermediate posture
	// survives cancellation.
	private restorePostureAfterHalt(): void {
		if (this.procedural?.name === 'sit') {
			this.env.root.position.y = 0;
			this.seatedWeight = 0;
			this.sitTargetY = 0;
		} else if (this.procedural?.name === 'stand') {
			this.env.root.position.y = this.sitOffsetY;
			this.seatedWeight = 1;
		}
		this.postureBaseline = null;
	}

	// Standing up is implicit in every translation and jump, but sequenced:
	// she plays the stand transition first, then moves. Only an explicit
	// sit step (or a reset/model swap) changes the seated baseline
	// otherwise. A sit/stand already in flight reverses smoothly from the
	// captured baseline instead of snapping.
	private standThen(action: () => void): void {
		this.pendingAfterStand = action;
		if (this.sittingFlag || this.procedural?.name === 'sit') {
			if (this.procedural?.name !== 'stand') this.startProcedural('stand');
			return;
		}
		if (this.procedural?.name === 'stand') return;
		this.pendingAfterStand = null;
		action();
	}

	// Instantly clear the seated state. Only for resets: every movement
	// path sequences through standThen so posture transitions stay visible.
	private clearSeated(): void {
		this.sittingFlag = false;
		this.sitOffsetY = 0;
		this.sitTargetY = 0;
		this.seatedWeight = 0;
		this.postureBaseline = null;
		this.env.root.position.y = 0;
	}

	// Root transform + posture baseline for resets and model swaps.
	resetPose(): void {
		this.cancel();
		this.clearSeated();
		this.pendingAfterStand = null;
		this.instantCompletions = [];
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
		// Idempotent posture: re-sitting while seated (or standing while
		// standing) completes at once instead of re-measuring or crouching.
		// A stand/sit already reversing the other way is NOT a no-op — it
		// re-captures the baseline and reverses smoothly from mid-pose.
		if (name === 'sit' && this.sittingFlag && this.procedural?.name !== 'stand') {
			this.instantCompletions.push({ type: 'procedural', routine: true });
			this.log('[AvatarCue] action finished', 'procedural:sit (already seated)');
			return true;
		}
		if (name === 'stand' && !this.sittingFlag && this.procedural?.name !== 'sit') {
			this.instantCompletions.push({ type: 'procedural', routine: true });
			this.log('[AvatarCue] action finished', 'procedural:stand (already standing)');
			return true;
		}
		if (name === 'sit') {
			// Seat height comes from the anchor when the plan traveled to one
			// ("sit on the chair"); otherwise the default chair height.
			const seatHeight =
				(anchorId && resolveSceneAnchor(anchorId)?.seatHeight) || DEFAULT_SEAT_HEIGHT;
			// Hips measured live include any current root drop; subtract it
			// so reversals target full seat depth instead of a shallower one.
			const hipsRestY = this.env.measureHipsY() - this.env.root.position.y;
			this.sitTargetY = computeSitOffsetY(hipsRestY, seatHeight);
			this.postureBaseline = {
				fromRootY: this.env.root.position.y,
				targetRootY: this.sitTargetY,
				fromWeight: this.seatedWeight,
				targetWeight: 1
			};
		} else if (name === 'stand') {
			this.postureBaseline = {
				fromRootY: this.env.root.position.y,
				targetRootY: 0,
				fromWeight: this.seatedWeight,
				targetWeight: 0
			};
		}
		this.procedural = { name, t: 0, duration };
		this.log('[AvatarCue] action started', `procedural:${name}`);
		return true;
	}

	startJump(): void {
		this.standThen(() => {
			this.jumpState = { t: 0 };
			this.log('[AvatarCue] action started', 'jump');
		});
	}

	private startLocomotion(direction: string, durationMs: number | undefined, action: string): void {
		this.standThen(() => {
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
		});
	}

	startWalk(direction: string, durationMs?: number): void {
		this.startLocomotion(direction, durationMs, 'walk');
	}

	startRun(direction: string, durationMs?: number): void {
		this.startLocomotion(direction, durationMs, 'run');
	}

	startGoto(targetX: number, targetZ: number, label: string): void {
		this.standThen(() => {
			const clamped = clampWalkOffset(targetX, targetZ, WALK_MAX_RADIUS);
			this.gotoState = { targetX: clamped.x, targetZ: clamped.z, phase: 0 };
			this.log('[AvatarCue] action started', label);
		});
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
	// turns are never routine-owned, so a turn stop always clears. Halting
	// a movement kind also drops its deferred start: a failed step must not
	// begin moving after its stand transition finishes.
	stopKind(kind: string): void {
		if (kind === 'walk') {
			this.locomotion = null;
			this.pendingAfterStand = null;
		}
		if (kind === 'return_home' || kind === 'goto') {
			this.gotoState = null;
			this.pendingAfterStand = null;
		}
		if (kind === 'turn' || kind === 'face_camera') this.turnState = null;
		if (kind === 'jump') {
			this.jumpState = null;
			this.pendingAfterStand = null;
			this.env.root.position.y = this.sittingFlag ? this.sitOffsetY : 0;
		}
		if (kind === 'procedural') {
			this.restorePostureAfterHalt();
			this.procedural = null;
			this.pendingAfterStand = null;
		}
	}

	// Advance every motion by `delta` seconds. Returns completions in
	// check order; the caller maps them to routine completions (routine
	// ones), ambient re-facing (locomotion/goto arrivals), and busy sync.
	update(delta: number): MotionCompletion[] {
		const completions: MotionCompletion[] = [];
		if (this.instantCompletions.length > 0) {
			completions.push(...this.instantCompletions);
			this.instantCompletions = [];
		}
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
			applyWalkSwing(humanoid, nudge, this.basis, Math.sin(this.locomotion.phase));
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
				applyWalkSwing(humanoid, nudge, this.basis, Math.sin(this.gotoState.phase));
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
					this.seatedWeight = 1;
					root.position.y = this.sitOffsetY;
				} else if (this.procedural.name === 'stand') {
					this.sittingFlag = false;
					this.sitOffsetY = 0;
					this.sitTargetY = 0;
					this.seatedWeight = 0;
					root.position.y = 0;
				}
				this.postureBaseline = null;
				this.log('[AvatarCue] action finished', `procedural:${this.procedural.name}`);
				this.procedural = null;
				completions.push({ type: 'procedural', routine: true });
				// A movement deferred behind this stand starts now, on the
				// same frame the posture settles — no teleport, no gap. If a
				// replacement program stole the transition (still seated), the
				// movement re-queues behind a fresh stand instead of walking
				// out of the chair.
				const pending = this.pendingAfterStand;
				this.pendingAfterStand = null;
				if (pending) {
					if (this.sittingFlag) this.standThen(pending);
					else pending();
				}
			} else if (this.procedural.name === 'sit' || this.procedural.name === 'stand') {
				// Posture transitions interpolate from the captured baseline,
				// so mid-transition reversals glide instead of snapping.
				const w = smoothstep(0, 0.8, progress);
				const baseline = this.postureBaseline;
				if (baseline) {
					root.position.y = baseline.fromRootY + (baseline.targetRootY - baseline.fromRootY) * w;
					this.seatedWeight =
						baseline.fromWeight + (baseline.targetWeight - baseline.fromWeight) * w;
				} else {
					this.seatedWeight = this.procedural.name === 'sit' ? w : 1 - w;
				}
				applyProceduralAction(
					humanoid,
					nudge,
					this.basis,
					this.procedural.name,
					progress,
					this.seatedWeight
				);
			} else {
				applyProceduralAction(humanoid, nudge, this.basis, this.procedural.name, progress);
			}
		}
		// Held sitting posture: re-applied every frame once the sit transition
		// hands off, until stand or any locomotion (which stands first).
		if (
			this.sittingFlag &&
			!this.procedural &&
			!this.locomotion &&
			!this.gotoState &&
			!this.jumpState
		) {
			root.position.y = this.sitOffsetY;
			applySittingPose(humanoid, nudge, this.basis, 1);
		}
		return completions;
	}
}
