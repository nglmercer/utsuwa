import type { NativeToolStep } from './agent';

export type NativeToolReceiptSummary = 'success' | 'recovered' | 'failed';

/** Tool calls that can be shown as a deterministic native mutation receipt. */
export function isNativeMutation(name: string): boolean {
	return /^(filesystem\.(write|write_user_file|create_user_file|edit|edit_user_file|edit_file|replace_user_file|append_user_file|append_file|patch|create|delete|move|mkdir)|process\.(spawn|kill)|desktop\.(click|invoke_element|type_text|set_value)|clipboard\.(write|set)|application\.launch|mcp\.|plugin\.)/.test(name);
}

function mutationFamily(name: string): string | null {
	if (name.startsWith('filesystem.')) return 'filesystem';
	if (name.startsWith('process.')) return 'process';
	if (name.startsWith('desktop.')) return 'desktop';
	if (name.startsWith('clipboard.')) return 'clipboard';
	if (name === 'application.launch') return 'application';
	if (name.startsWith('mcp.')) return 'mcp';
	if (name.startsWith('plugin.')) return 'plugin';
	return null;
}

/**
 * Whether a failed attempt has a later successful mutation in the same
 * native capability family. The sequence is intentionally conservative:
 * unrelated read failures do not disappear just because another operation
 * later mutated something.
 */
export function isSupersededFailure(steps: NativeToolStep[], index: number): boolean {
	const failed = steps[index];
	if (!failed || failed.ok) return false;
	const family = mutationFamily(failed.name);
	if (!family) return false;
	return steps.slice(index + 1).some(
		(step) => step.ok && isNativeMutation(step.name) && mutationFamily(step.name) === family
	);
}

/**
 * Classify the native outcome independently of the assistant's final prose.
 * A failed native attempt followed by a successful mutation in the same
 * family is a recovered turn, while an unrecovered failure remains failed.
 */
export function summarizeNativeToolSteps(steps: NativeToolStep[]): NativeToolReceiptSummary {
	const failures = steps
		.map((step, index) => ({ step, index }))
		.filter(({ step }) => !step.ok);
	if (failures.length === 0) return 'success';

	const hasRecoveredFailure = failures.some(({ index }) => isSupersededFailure(steps, index));
	const hasUnrecoveredFailure = failures.some(({ index }) => !isSupersededFailure(steps, index));
	if (hasRecoveredFailure && !hasUnrecoveredFailure) return 'recovered';
	return 'failed';
}
