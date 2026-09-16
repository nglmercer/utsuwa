// Native task authority client (`task.*` IPC). The Rust host owns durable
// orchestration; this module is a thin typed pipe. Pure except for the
// default singleton's bridge lookup — node tests inject a fake invoke.
import {
	NATIVE_BRIDGE_UNAVAILABLE_ERROR,
	getBridge,
	type UtsuwaBridge
} from '../services/native/bridge.ts';

export type HostTaskStatus =
	| 'pending'
	| 'scheduled'
	| 'ready'
	| 'running'
	| 'waiting'
	| 'needs_review'
	| 'completed'
	| 'failed'
	| 'cancelled';

export type HostStepType =
	| 'avatar_routine'
	| 'notification'
	| 'wait'
	| 'agent'
	| 'tool'
	| 'approval';

export type HostStepStatus = 'pending' | 'running' | 'completed' | 'failed' | 'skipped';

export interface HostTaskStep {
	id: string;
	step_type: HostStepType;
	status: HostStepStatus;
	input: Record<string, unknown>;
	result?: unknown;
	attempts: number;
	max_attempts: number;
	started_at?: number;
	finished_at?: number;
	error?: string;
}

export type HostVerificationSpec =
	| { type: 'none' }
	| { type: 'result_present' }
	| { type: 'avatar_routine'; expected_steps: string[] }
	| { type: 'tool_receipt'; tool_name: string }
	| { type: 'file_exists'; file_ref: string }
	| { type: 'agent_review' }
	| { type: 'human_review' };

export interface HostTask {
	id: string;
	title: string;
	instruction: string;
	status: HostTaskStatus;
	priority: number;
	created_at: number;
	updated_at: number;
	scheduled_at?: number;
	started_at?: number;
	finished_at?: number;
	attempts: number;
	max_attempts: number;
	lease_until?: number;
	next_attempt_at?: number;
	wait_for?: {
		event_type?: string;
		correlation_id?: string;
		timeout_at?: number;
	};
	steps: HostTaskStep[];
	current_step_index: number;
	result?: unknown;
	last_error?: { message: string; retryable: boolean; timestamp: number };
	verification?: HostVerificationSpec;
	parent_task_id?: string;
}

export interface NewHostTaskStep {
	step_type: HostStepType;
	input: Record<string, unknown>;
	max_attempts?: number;
}

export interface NewHostTask {
	title: string;
	instruction: string;
	scheduled_at?: number;
	priority?: number;
	max_attempts?: number;
	verification?: HostVerificationSpec;
	parent_task_id?: string;
	steps: NewHostTaskStep[];
}

export type InvokeFn = (
	method: string,
	params?: Record<string, unknown>
) => Promise<unknown>;

function asRecord(value: unknown, method: string): Record<string, unknown> {
	if (value && typeof value === 'object' && !Array.isArray(value)) {
		return value as Record<string, unknown>;
	}
	throw new Error(`${method} returned a non-object response`);
}

function asHostTask(value: unknown, method: string): HostTask {
	const record = asRecord(value, method);
	if (typeof record.id !== 'string' || typeof record.status !== 'string') {
		throw new Error(`${method} returned a malformed task`);
	}
	return record as unknown as HostTask;
}

function asHostTaskList(value: unknown, method: string): HostTask[] {
	if (!Array.isArray(value)) throw new Error(`${method} returned a non-array response`);
	return value.map((entry) => asHostTask(entry, method));
}

export interface HostTaskClient {
	create(task: NewHostTask): Promise<HostTask>;
	get(taskId: string): Promise<HostTask | null>;
	list(status?: HostTaskStatus, limit?: number): Promise<HostTask[]>;
	cancel(taskId: string, reason?: string): Promise<HostTask>;
	review(taskId: string, approved: boolean, note?: string): Promise<HostTask>;
	deliverEvent(
		eventType: string,
		correlationId: string | undefined,
		payload: unknown
	): Promise<{ delivered: boolean; task: HostTask | null }>;
}

export function createHostTaskClient(invoke: InvokeFn): HostTaskClient {
	return {
		async create(task: NewHostTask): Promise<HostTask> {
			const result = await invoke('task.create', task as unknown as Record<string, unknown>);
			return asHostTask(result, 'task.create');
		},
		async get(taskId: string): Promise<HostTask | null> {
			try {
				const result = await invoke('task.get', { task_id: taskId });
				return asHostTask(result, 'task.get');
			} catch (err) {
				if (err instanceof Error && /unknown task/i.test(err.message)) return null;
				throw err;
			}
		},
		async list(status?: HostTaskStatus, limit = 50): Promise<HostTask[]> {
			const params: Record<string, unknown> = { limit };
			if (status) params.status = status;
			const result = await invoke('task.list', params);
			return asHostTaskList(result, 'task.list');
		},
		async cancel(taskId: string, reason?: string): Promise<HostTask> {
			const params: Record<string, unknown> = { task_id: taskId };
			if (reason !== undefined) params.reason = reason;
			const result = await invoke('task.cancel', params);
			return asHostTask(result, 'task.cancel');
		},
		async review(taskId: string, approved: boolean, note?: string): Promise<HostTask> {
			const params: Record<string, unknown> = { task_id: taskId, approved };
			if (note !== undefined) params.note = note;
			const result = await invoke('task.review', params);
			return asHostTask(result, 'task.review');
		},
		async deliverEvent(
			eventType: string,
			correlationId: string | undefined,
			payload: unknown
		): Promise<{ delivered: boolean; task: HostTask | null }> {
			const params: Record<string, unknown> = { event_type: eventType, payload };
			if (correlationId !== undefined) params.correlation_id = correlationId;
			const result = await invoke('task.event', params);
			const record = asRecord(result, 'task.event');
			const task =
				record.task && typeof record.task === 'object'
					? asHostTask(record.task, 'task.event')
					: null;
			return { delivered: record.delivered === true, task };
		}
	};
}

function bridgeInvoke(bridge: UtsuwaBridge): InvokeFn {
	return (method, params) => bridge.invoke(method, params) as Promise<unknown>;
}

export function isHostTasksAvailable(): boolean {
	return getBridge() !== null;
}

/** Default client bound to `window.utsuwa`. Throws when no bridge exists. */
export function hostTasks(): HostTaskClient {
	const bridge = getBridge();
	if (!bridge) throw new Error(NATIVE_BRIDGE_UNAVAILABLE_ERROR);
	return createHostTaskClient(bridgeInvoke(bridge));
}

/** Parse a capability-gated `needs_review` reason (see task-host runners). */
export interface CapabilityReviewRequest {
	kind: 'capability';
	tool: string;
	capability: string;
	resource: unknown;
	detail: string;
}

export function parseCapabilityReview(reason: string | undefined): CapabilityReviewRequest | null {
	if (!reason) return null;
	try {
		const parsed = JSON.parse(reason) as Partial<CapabilityReviewRequest>;
		if (parsed.kind === 'capability' && typeof parsed.tool === 'string') {
			return parsed as CapabilityReviewRequest;
		}
		return null;
	} catch {
		return null;
	}
}
