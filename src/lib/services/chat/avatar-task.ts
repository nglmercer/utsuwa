// Chat avatar plans via durable host tasks, with local fallback.
//
// When the native host is available, chat routines ("jump!", "walk left")
// run as host `avatar_routine` tasks: the renderer still executes them
// (same speed, same queue) but the run is durable, receipt-verified, and
// retried like any other task. Without a bridge — or when host submission
// itself fails — plans run locally exactly as before. A routine that was
// accepted by the host is never duplicated locally: transport failures
// after submission cancel best-effort before falling back, and poll
// timeouts report `timed_out` while the host owns the outcome.
//
// Pure logic + injected deps: node-safe (no Svelte, no DOM, no bridge).
import { routineStepKey } from '../../tasks/host-avatar.ts';
import type {
	HostTask,
	HostTaskClient,
	NewHostTask
} from '../../tasks/host.ts';
import type {
	RoutineResult,
	RoutineStatus,
	RoutineStepInput
} from '../../stores/vrm.svelte.ts';

export interface RoutineSummary {
	status: RoutineStatus;
	completed: string[];
	failures: Array<{ stepIndex: number; key: string; reason: string }>;
}

export interface AvatarPlanRequest {
	steps: RoutineStepInput[];
	label?: string;
}

export interface AvatarTaskDeps {
	useHost: () => boolean;
	createTask: HostTaskClient['create'];
	getTask: HostTaskClient['get'];
	cancelTask: HostTaskClient['cancel'];
	runLocal: (
		steps: RoutineStepInput[]
	) => Promise<Pick<RoutineResult, 'status' | 'completed' | 'failures'>>;
	sleep?: (ms: number) => Promise<void>;
	log?: (message: string) => void;
}

const DEFAULT_POLL_INTERVAL_MS = 250;
const DEFAULT_ROUTINE_BUDGET_MS = 120_000;

const defaultSleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

function isTerminal(status: string): boolean {
	return status === 'completed' || status === 'failed' || status === 'cancelled';
}

export interface WaitOptions {
	timeoutMs?: number;
	intervalMs?: number;
	sleep?: (ms: number) => Promise<void>;
}

/** Poll `task.get` until the task reaches a terminal status. Throws on
 * timeout or when the task disappears. */
export async function waitForHostTask(
	getTask: HostTaskClient['get'],
	taskId: string,
	options: WaitOptions = {}
): Promise<HostTask> {
	const timeoutMs = options.timeoutMs ?? DEFAULT_ROUTINE_BUDGET_MS;
	const intervalMs = options.intervalMs ?? DEFAULT_POLL_INTERVAL_MS;
	const sleep = options.sleep ?? defaultSleep;
	const started = Date.now();
	for (;;) {
		const task = await getTask(taskId);
		if (!task) throw new Error(`host task ${taskId} disappeared while waiting`);
		if (isTerminal(task.status)) return task;
		if (Date.now() - started >= timeoutMs) {
			throw new Error(`host task ${taskId} did not finish within ${timeoutMs}ms`);
		}
		await sleep(intervalMs);
	}
}

interface ReceiptShape {
	status?: unknown;
	completed_steps?: unknown;
	failures?: unknown;
}

function asReceipt(value: unknown): ReceiptShape | null {
	if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
	const record = value as Record<string, unknown>;
	if (!Array.isArray(record.completed_steps)) return null;
	return record as ReceiptShape;
}

function failuresOf(receipt: ReceiptShape): RoutineSummary['failures'] {
	if (!Array.isArray(receipt.failures)) return [];
	return (receipt.failures as Array<Record<string, unknown>>)
		.filter((entry) => entry && typeof entry === 'object')
		.map((entry, index) => ({
			stepIndex: typeof entry.stepIndex === 'number' ? entry.stepIndex : index,
			key: typeof entry.key === 'string' ? entry.key : '',
			reason: typeof entry.reason === 'string' ? entry.reason : 'failed'
		}));
}

/** Map a terminal host task to the chat-facing routine summary. Reads the
 * renderer receipt out of the aggregated step results; falls back to the
 * task error when no receipt exists. */
export function hostTaskToRoutineSummary(task: HostTask): RoutineSummary {
	let receipt: ReceiptShape | null = null;
	for (let index = task.steps.length - 1; index >= 0; index -= 1) {
		receipt = asReceipt(task.steps[index]?.result);
		if (receipt) break;
	}
	const completed = (
		Array.isArray(receipt?.completed_steps) ? (receipt?.completed_steps as unknown[]) : []
	)
		.filter((key): key is string => typeof key === 'string')
		.slice();
	const failures = receipt ? failuresOf(receipt) : [];
	if (task.status === 'completed') return { status: 'completed', completed, failures: [] };
	if (task.status === 'cancelled') return { status: 'cancelled', completed, failures };
	if (failures.length === 0 && task.last_error) {
		failures.push({ stepIndex: -1, key: '', reason: task.last_error.message });
	}
	return {
		status: completed.length > 0 ? 'partial' : 'failed',
		completed,
		failures
	};
}

export function routineResultToSummary(
	result: Pick<RoutineResult, 'status' | 'completed' | 'failures'>
): RoutineSummary {
	return {
		status: result.status,
		completed: [...result.completed],
		failures: result.failures.map((failure) => ({ ...failure }))
	};
}

export function buildRoutineTaskInput(plan: AvatarPlanRequest): NewHostTask {
	return {
		title: 'Chat avatar routine',
		instruction: plan.label ?? 'Run the requested avatar routine.',
		verification: {
			type: 'avatar_routine',
			expected_steps: plan.steps.map((step) => routineStepKey(step))
		},
		steps: [
			{
				step_type: 'avatar_routine',
				input: { steps: plan.steps, receipt_timeout_ms: DEFAULT_ROUTINE_BUDGET_MS }
			}
		]
	};
}

/**
 * Execute a chat avatar plan and resolve with its authoritative summary.
 * Host path submits a durable task and waits; local fallback runs the
 * renderer queue directly. Host submission failures fall back to local;
 * post-submission transport errors cancel best-effort first so the
 * routine never runs twice.
 */
export async function executeAvatarPlan(
	deps: AvatarTaskDeps,
	plan: AvatarPlanRequest,
	wait: WaitOptions = {}
): Promise<RoutineSummary> {
	if (!deps.useHost()) {
		return routineResultToSummary(await deps.runLocal(plan.steps));
	}
	let task: HostTask;
	try {
		task = await deps.createTask(buildRoutineTaskInput(plan));
	} catch (err) {
		deps.log?.(`host routine submission failed, running locally: ${(err as Error)?.message ?? err}`);
		return routineResultToSummary(await deps.runLocal(plan.steps));
	}
	try {
		const terminal = await waitForHostTask(deps.getTask, task.id, {
			...wait,
			sleep: wait.sleep ?? deps.sleep
		});
		return hostTaskToRoutineSummary(terminal);
	} catch (err) {
		const message = (err as Error)?.message ?? String(err);
		if (/did not finish within/.test(message)) {
			deps.log?.(`host routine ${task.id} timed out waiting; host owns the outcome`);
			return { status: 'timed_out', completed: [], failures: [] };
		}
		try {
			await deps.cancelTask(task.id, 'chat wait transport failed; falling back to local run');
		} catch {
			// Best effort: the host wait-timeout is the backstop.
		}
		deps.log?.(`host routine wait failed, running locally: ${message}`);
		return routineResultToSummary(await deps.runLocal(plan.steps));
	}
}

/**
 * Launch a chat avatar plan without waiting (mixed requests where the
 * conversation continues). Fire-and-forget on both paths; failures only log.
 */
export function launchAvatarPlan(deps: AvatarTaskDeps, plan: AvatarPlanRequest): void {
	if (!deps.useHost()) {
		void deps.runLocal(plan.steps).catch((err) => deps.log?.(`local routine failed: ${(err as Error)?.message ?? err}`));
		return;
	}
	void (async () => {
		try {
			await deps.createTask(buildRoutineTaskInput(plan));
		} catch (err) {
			deps.log?.(`host routine submission failed, running locally: ${(err as Error)?.message ?? err}`);
			try {
				await deps.runLocal(plan.steps);
			} catch (localErr) {
				deps.log?.(`local routine fallback failed: ${(localErr as Error)?.message ?? localErr}`);
			}
		}
	})();
}
