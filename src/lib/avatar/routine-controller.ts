// Routine controller: the authoritative avatar-routine state machine.
// Owns the step ledger (pending -> running -> completed|failed|cancelled),
// the per-step watchdog (driven by render time, so a hidden tab pauses
// deadlines together with motion), and the finish truth table:
//
// completed = every expected step completed, zero failures.
// partial    = halted or finished with some steps completed and some failed.
// failed     = nothing completed, at least one failure (and not cancelled).
// cancelled  = interrupted by photo/swap/stop/new routine (never a success).
// timed_out  = halted because a step exceeded its watchdog deadline.
//
// Framework-free and THREEE-free: step execution, receipt recording, and
// the wall clock arrive by dependency injection, so node tests drive the
// whole machine with fakes. Invariants: every step lands in the ledger
// with an explicit status, failed steps never silently become completed,
// and the runtime receipt (not LLM text) is the proof of completion.
import { routineStepReceiptKey } from '../engine/avatar-action-key.ts';
import type {
	RoutineResult,
	RoutineStatus,
	RoutineStepResult,
	RoutineStepStatus
} from '../stores/vrm.svelte.ts';

export interface RoutineStepInput {
	kind: string;
	action: string;
	direction?: string;
	durationMs?: number;
	url?: string;
	anchorId?: string;
	x?: number;
	z?: number;
}

export interface RoutineStepStart {
	started: boolean;
	deadlineMs: number;
}

// Starts (and halts) the physical motion for one step. Implemented by the
// renderer wiring: the controller never touches THREE, Svelte, or stores.
export interface RoutineStepExecutor {
	startStep(step: RoutineStepInput, index: number): RoutineStepStart;
	stopStep(step: RoutineStepInput): void;
}

export interface RoutineReporter {
	recordStep(result: Omit<RoutineStepResult, 'at'>): void;
	recordResult(result: Omit<RoutineResult, 'at' | 'seq'>): void;
	now(): number;
}

export interface RoutinePolicy {
	continueOnFailure?: boolean;
}

export interface ActiveRoutineSnapshot {
	id: string;
	index: number;
	stepCount: number;
	completed: string[];
	failures: Array<{ stepIndex: number; key: string; reason: string }>;
}

interface ActiveRoutine {
	id: string;
	steps: RoutineStepInput[];
	index: number;
	completed: string[];
	failures: Array<{ stepIndex: number; key: string; reason: string }>;
	continueOnFailure: boolean;
	startedAt: number;
	stepStartedAt: number;
	stepElapsed: number;
	stepDeadlineMs: number;
}

export interface RoutineControllerOptions {
	executor: RoutineStepExecutor;
	reporter: RoutineReporter;
	getEndPosition?: () => { x: number; y: number; z: number };
	// Called after every transition (busy sync, UI refresh).
	onChange?: () => void;
	log?: (message: string, detail?: string) => void;
}

export interface RoutineController {
	readonly active: ActiveRoutineSnapshot | null;
	start(id: string, steps: RoutineStepInput[], policy?: RoutinePolicy): void;
	advance(): void;
	completeCurrentStep(): void;
	failCurrentStep(reason: string, timedOut?: boolean): void;
	cancel(reason: string): void;
	/** Advance the render-time watchdog; fails the step past its deadline. */
	tick(deltaSeconds: number): void;
	/** Clip-derived budget for emote steps, from first actual playback. */
	setStepDeadline(deadlineMs: number): void;
	resetStepElapsed(): void;
}

export function createRoutineController(options: RoutineControllerOptions): RoutineController {
	const { executor, reporter } = options;
	let routine: ActiveRoutine | null = null;

	const log = (message: string, detail?: string) => {
		if (options.log) options.log(message, detail);
		else console.debug(message, detail);
	};
	const changed = () => options.onChange?.();

	function snapshot(): ActiveRoutineSnapshot | null {
		if (!routine) return null;
		return {
			id: routine.id,
			index: routine.index,
			stepCount: routine.steps.length,
			completed: [...routine.completed],
			failures: routine.failures.map((failure) => ({ ...failure }))
		};
	}

	function finish(status: RoutineStatus): void {
		if (!routine) return;
		const finished: ActiveRoutine = routine;
		reporter.recordResult({
			routineId: finished.id,
			status,
			expected: finished.steps.map(routineStepReceiptKey),
			completed: [...finished.completed],
			failures: finished.failures.map((failure) => ({ ...failure })),
			startedAt: finished.startedAt,
			finishedAt: reporter.now(),
			endPosition: options.getEndPosition?.()
		});
		log('[AvatarCue] routine finished', `${finished.id} (${status})`);
		routine = null;
		changed();
	}

	function advance(): void {
		if (!routine) return;
		if (routine.index >= routine.steps.length) {
			if (routine.failures.length === 0) finish('completed');
			else if (routine.completed.length > 0) finish('partial');
			else finish('failed');
			return;
		}
		const step = routine.steps[routine.index];
		const key = routineStepReceiptKey(step);
		routine.stepStartedAt = reporter.now();
		routine.stepElapsed = 0;
		const { started, deadlineMs } = executor.startStep(step, routine.index);
		routine.stepDeadlineMs = deadlineMs;
		if (!started) {
			failCurrentStep('unknown step');
			return;
		}
		reporter.recordStep({
			routineId: routine.id,
			stepIndex: routine.index,
			key,
			status: 'running',
			startedAt: routine.stepStartedAt,
		});
		changed();
	}

	function completeCurrentStep(): void {
		if (!routine) return;
		const step = routine.steps[routine.index];
		if (!step) return;
		const key = routineStepReceiptKey(step);
		routine.completed.push(key);
		reporter.recordStep({
			routineId: routine.id,
			stepIndex: routine.index,
			key,
			status: 'completed',
			startedAt: routine.stepStartedAt,
			finishedAt: reporter.now(),
		});
		routine.index++;
		advance();
	}

	function failCurrentStep(reason: string, timedOut = false): void {
		if (!routine) return;
		const step = routine.steps[routine.index];
		if (!step) return;
		const key = routineStepReceiptKey(step);
		routine.failures.push({ stepIndex: routine.index, key, reason });
		const status: RoutineStepStatus = 'failed';
		reporter.recordStep({
			routineId: routine.id,
			stepIndex: routine.index,
			key,
			status,
			reason,
			startedAt: routine.stepStartedAt,
			finishedAt: reporter.now(),
		});
		// Stop the half-played motion.
		executor.stopStep(step);
		routine.index++;
		if (!routine.continueOnFailure) {
			if (timedOut) finish('timed_out');
			else if (routine.completed.length > 0) finish('partial');
			else finish('failed');
			return;
		}
		advance();
	}

	function cancel(reason: string): void {
		if (routine) {
			const step = routine.steps[routine.index];
			if (step) {
				reporter.recordStep({
					routineId: routine.id,
					stepIndex: routine.index,
					key: routineStepReceiptKey(step),
					status: 'cancelled',
					reason,
					startedAt: routine.stepStartedAt,
					finishedAt: reporter.now()
				});
			}
			// Halt the half-played motion before the result lands.
			if (step) executor.stopStep(step);
			log('[AvatarCue] routine cancelled', `${routine.id} (${reason})`);
		}
		if (!routine) return;
		finish('cancelled');
	}

	return {
		get active() {
			return snapshot();
		},
		start(id, steps, policy) {
			// Replacement policy: at most one routine; a new one cancels
			// the old.
			cancel('superseded');
			routine = {
				id,
				steps,
				index: 0,
				completed: [],
				failures: [],
				continueOnFailure: policy?.continueOnFailure ?? false,
				startedAt: reporter.now(),
				stepStartedAt: 0,
				stepElapsed: 0,
				stepDeadlineMs: 0
			};
			log('[AvatarCue] routine started', `${id} (${steps.length} steps)`);
			advance();
		},
		advance,
		completeCurrentStep,
		failCurrentStep,
		cancel,
		tick(deltaSeconds) {
			if (!routine) return;
			routine.stepElapsed += deltaSeconds;
			if (routine.stepElapsed * 1000 > routine.stepDeadlineMs) {
				log('[AvatarCue] step watchdog fired', routine.id);
				failCurrentStep('step exceeded its deadline', true);
			}
		},
		setStepDeadline(deadlineMs) {
			if (routine) routine.stepDeadlineMs = deadlineMs;
		},
		resetStepElapsed() {
			if (routine) routine.stepElapsed = 0;
		}
	};
}
