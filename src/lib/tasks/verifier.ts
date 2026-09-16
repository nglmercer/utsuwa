// Task verifier: completion depends on the work type, never on a model saying
// "done". Pure and node-safe.
import type { DurableTask, VerificationSpec } from './types.ts';

export type VerificationOutcome =
	| { ok: true }
	| { ok: false; reason: string }
	| { ok: 'review'; reason: string };

export function verifyTask(task: DurableTask, spec?: VerificationSpec): VerificationOutcome {
	const effective = spec ?? task.verification ?? { type: 'none' as const };
	switch (effective.type) {
		case 'none':
			return { ok: true };
		case 'result_present':
			return task.result !== undefined && task.result !== null
				? { ok: true }
				: { ok: false, reason: 'task finished without a result' };
		case 'agent_review':
		case 'human_review':
			return { ok: 'review', reason: `${effective.type} required before completion` };
	}
}
