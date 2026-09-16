// Durable task types: agent turns are temporary, tasks are durable. A task is
// persisted structured state that survives app close/reopen; turns, tool
// calls, and avatar actions are merely how steps execute. Pure: no Svelte,
// no Dexie, no DOM — safe for node tests and shared by all adapters.

export type TaskStatus =
	| 'pending'
	| 'scheduled'
	| 'ready'
	| 'running'
	| 'waiting'
	| 'needs_review'
	| 'completed'
	| 'failed'
	| 'cancelled';

export type StepStatus = 'pending' | 'running' | 'completed' | 'failed' | 'skipped';

export type TaskStepType =
	| 'avatar_routine'
	| 'notification'
	| 'wait'
	| 'agent'
	| 'tool'
	| 'approval';

export interface AvatarRoutineStepInput {
	steps: Array<{
		kind: 'procedural' | 'jump' | 'walk' | 'emote';
		action: string;
		direction?: 'left' | 'right' | 'forward' | 'back';
		durationMs?: number;
		url?: string;
	}>;
	// Hard cap for the whole routine; the step fails retryably past it.
	timeoutMs?: number;
}

export interface NotificationStepInput {
	title: string;
	body: string;
}

export interface WaitStepInput {
	// Resume after this long (marks the step completed).
	durationMs?: number;
	// ...or when a matching event arrives first (marks it completed).
	eventType?: string;
	correlationId?: string;
	// A wait with an event but no timeout waits until cancelled; prefer both.
	timeoutMs?: number;
}

export type TaskStepInput = AvatarRoutineStepInput | NotificationStepInput | WaitStepInput | Record<string, unknown>;

export interface TaskStep {
	id: string;
	type: TaskStepType;
	status: StepStatus;
	input: TaskStepInput;
	result?: unknown;
	attempts: number;
	maxAttempts: number;
	error?: string;
}

export type VerificationSpec =
	| { type: 'none' }
	| { type: 'result_present' }
	| { type: 'agent_review' }
	| { type: 'human_review' };

export type TaskPriority = 10 | 50 | 80 | 100;
export const TaskPriority = {
	Background: 10,
	Normal: 50,
	UserRequested: 80,
	Urgent: 100
} as const;

export interface DurableTask {
	id: string;
	title: string;
	instruction: string;
	status: TaskStatus;
	priority: number;
	createdAt: number;
	updatedAt: number;
	scheduledAt?: number;
	startedAt?: number;
	finishedAt?: number;
	attempts: number;
	maxAttempts: number;
	// A running task holds the lease until this time; expiry means the worker
	// died and the task may be retried. Renewed by heartbeat while in-flight.
	leaseUntil?: number;
	// Earliest time a ready task may dispatch (retry backoff).
	nextAttemptAt?: number;
	waitFor?: {
		eventType?: string;
		correlationId?: string;
		timeoutAt?: number;
	};
	steps: TaskStep[];
	currentStepIndex: number;
	result?: unknown;
	lastError?: { message: string; retryable: boolean; timestamp: number };
	verification?: VerificationSpec;
	parentTaskId?: string;
}

export type NewTaskStep = Pick<TaskStep, 'type' | 'input'> &
	Partial<Pick<TaskStep, 'id' | 'maxAttempts'>>;

export type NewTask = Pick<DurableTask, 'title' | 'instruction'> &
	Partial<
		Pick<
			DurableTask,
			| 'scheduledAt'
			| 'priority'
			| 'maxAttempts'
			| 'verification'
			| 'parentTaskId'
		>
	> & { steps: NewTaskStep[] };

export type TaskEventType =
	| 'task.due'
	| 'task.completed'
	| 'task.failed'
	| 'task.review_required'
	| 'notification.fired'
	| 'avatar.completed'
	| 'timer.fired';

export interface TaskEvent {
	id: string;
	type: TaskEventType;
	sourceId?: string;
	correlationId?: string;
	payload?: unknown;
	createdAt: number;
}

export const TERMINAL_STATUSES: readonly TaskStatus[] = ['completed', 'failed', 'cancelled'];

export function isTerminalStatus(status: TaskStatus): boolean {
	return (TERMINAL_STATUSES as readonly string[]).includes(status);
}

export function newStepId(): string {
	return typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
		? crypto.randomUUID()
		: `step-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

export function newTaskId(): string {
	return typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
		? crypto.randomUUID()
		: `task-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}
