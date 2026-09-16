// Host avatar listener: answers `avatar.routine.requested` from the native
// task host by running the routine through the renderer and posting the
// renderer receipt back via `task.event`. The renderer produces the receipt;
// Rust verifies it; only then does the task complete.
//
// Deps are injected (event target, routine runner, delivery) so node tests
// cover this without Svelte, DOM, or the bridge; `browser.ts` wires the real
// implementations.
import { HOST_EVENT, type HostEventDetail } from '../services/native/bridge.ts';
import type { RoutineResult, RoutineStepInput } from '../stores/vrm.svelte.ts';

export const AVATAR_ROUTINE_REQUESTED = 'avatar.routine.requested';
export const AVATAR_ROUTINE_COMPLETED = 'avatar.routine.completed';

export interface HostRoutineRequest {
	taskId: string;
	stepId: string;
	steps: RoutineStepInput[];
	policy?: { continueOnFailure?: boolean };
	/** Host-side receipt wait budget; the local run stays strictly under it. */
	receiptTimeoutMs?: number;
	runTimeoutMs?: number;
}

export interface AvatarReceipt {
	status: 'success' | 'failed';
	/** Renderer step keys as produced by `routineStepKey`: `kind:action[:direction]`. */
	completed_steps: string[];
	failures: Array<{ stepIndex: number; key: string; reason: string }>;
	routine_id: string;
}

const DEFAULT_RUN_TIMEOUT_MS = 55_000;
const RECEIPT_MARGIN_MS = 2_000;
const MIN_RUN_TIMEOUT_MS = 5_000;

/** Pure mapping: renderer truth table → host receipt. Only `completed` is success. */
export function routineResultToReceipt(result: RoutineResult): AvatarReceipt {
	return {
		status: result.status === 'completed' ? 'success' : 'failed',
		completed_steps: [...result.completed],
		failures: result.failures.map((f) => ({ ...f })),
		routine_id: result.routineId
	};
}

/** Parse one host event into a routine request; `null` means "not ours". */
export function parseRoutineRequest(detail: HostEventDetail | null | undefined): HostRoutineRequest | null {
	if (!detail || detail.event !== AVATAR_ROUTINE_REQUESTED) return null;
	const data = detail.data as Record<string, unknown> | null;
	if (!data || typeof data !== 'object') return null;
	const taskId = data.task_id;
	const stepId = data.step_id;
	const routine = data.routine as Record<string, unknown> | null;
	if (typeof taskId !== 'string' || !taskId) return null;
	if (typeof stepId !== 'string' || !stepId) return null;
	if (!routine || typeof routine !== 'object' || !Array.isArray(routine.steps)) return null;
	const policy =
		routine.policy && typeof routine.policy === 'object'
			? (routine.policy as { continueOnFailure?: boolean })
			: undefined;
	const receiptTimeoutMs =
		typeof routine.receipt_timeout_ms === 'number' ? routine.receipt_timeout_ms : undefined;
	const runTimeoutMs = typeof routine.timeoutMs === 'number' ? routine.timeoutMs : undefined;
	return {
		taskId,
		stepId,
		steps: routine.steps as RoutineStepInput[],
		policy,
		receiptTimeoutMs,
		runTimeoutMs
	};
}

export function runTimeoutFor(request: HostRoutineRequest): number {
	const budget = request.receiptTimeoutMs ?? request.runTimeoutMs;
	if (typeof budget === 'number' && budget > 0) {
		return Math.max(MIN_RUN_TIMEOUT_MS, budget - RECEIPT_MARGIN_MS);
	}
	return DEFAULT_RUN_TIMEOUT_MS;
}

export type RoutineRunner = (
	steps: RoutineStepInput[],
	options: { policy?: { continueOnFailure?: boolean }; timeoutMs: number }
) => Promise<RoutineResult>;

export type ReceiptDelivery = (
	eventType: string,
	correlationId: string | undefined,
	payload: unknown
) => Promise<unknown>;

export interface HostAvatarListenerDeps {
	events: Pick<EventTarget, 'addEventListener' | 'removeEventListener'>;
	runRoutine: RoutineRunner;
	deliver: ReceiptDelivery;
	log?: (message: string) => void;
}

/**
 * Handle one routine request end-to-end: run, then ALWAYS post a receipt
 * (success, renderer failure, or local timeout/error). A missing receipt
 * would strand the host task until its wait expires; explicit failure lets
 * verification retry immediately.
 */
export async function handleRoutineRequest(
	request: HostRoutineRequest,
	deps: Pick<HostAvatarListenerDeps, 'runRoutine' | 'deliver' | 'log'>
): Promise<void> {
	let receipt: AvatarReceipt;
	try {
		const result = await deps.runRoutine(request.steps, {
			policy: request.policy,
			timeoutMs: runTimeoutFor(request)
		});
		receipt = routineResultToReceipt(result);
	} catch (err) {
		deps.log?.(`routine for task ${request.taskId} threw: ${(err as Error)?.message ?? err}`);
		receipt = {
			status: 'failed',
			completed_steps: [],
			failures: [{ stepIndex: -1, key: '', reason: (err as Error)?.message ?? 'routine threw' }],
			routine_id: ''
		};
	}
	try {
		await deps.deliver(AVATAR_ROUTINE_COMPLETED, request.taskId, receipt);
	} catch (err) {
		// Delivery failed; the host wait-timeout is the backstop. Log loudly.
		deps.log?.(
			`failed to deliver avatar receipt for task ${request.taskId}: ${(err as Error)?.message ?? err}`
		);
	}
}

/** Subscribe to host routine requests; returns an unsubscribe function. */
export function startHostAvatarListener(deps: HostAvatarListenerDeps): () => void {
	const onEvent = (event: Event) => {
		const detail = (event as CustomEvent<HostEventDetail>).detail;
		const request = parseRoutineRequest(detail);
		if (!request) return;
		void handleRoutineRequest(request, deps);
	};
	deps.events.addEventListener(HOST_EVENT, onEvent);
	return () => {
		deps.events.removeEventListener(HOST_EVENT, onEvent);
	};
}
