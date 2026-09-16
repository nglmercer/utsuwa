// Shared avatar-routine execution: stage steps and await the authoritative
// renderer receipt. Used by pure chat commands and the task bridge alike so
// "done" always means the runtime said so. Browser-only (Svelte store).
import { vrmStore, type RoutineResult, type RoutineStepInput } from '$lib/stores/vrm.svelte';

// Resolves with the routine result (any terminal status) or rejects on the
// timeout. Callers decide what each status means; this helper never invents
// completion.
export async function runAvatarRoutineAndWait(
	steps: RoutineStepInput[],
	options?: { policy?: { continueOnFailure?: boolean }; timeoutMs?: number }
): Promise<RoutineResult> {
	const routineId = vrmStore.requestAvatarRoutine(steps, options?.policy);
	const bound = options?.timeoutMs ?? 120000;
	return new Promise<RoutineResult>((resolve, reject) => {
		const timer = setTimeout(() => {
			off();
			reject(new Error(`routine ${routineId} produced no result within ${bound}ms`));
		}, bound);
		const off = vrmStore.onRoutineResult((result) => {
			if (result.routineId !== routineId) return;
			clearTimeout(timer);
			off();
			resolve(result);
		});
	});
}
