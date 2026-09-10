<script lang="ts">
	import type { NativeToolStep } from '$lib/services/native/agent';
	import {
		isNativeMutation,
		summarizeNativeToolSteps
	} from '$lib/services/native/tool-receipts';

	interface Props {
		steps?: NativeToolStep[];
		floating?: boolean;
	}

	let { steps = [], floating = false }: Props = $props();

	function shouldShow(step: NativeToolStep): boolean {
		return !step.ok || isNativeMutation(step.name);
	}

	function outputPath(step: NativeToolStep): string | null {
		if (typeof step.output !== 'object' || step.output === null) return null;
		const output = step.output as Record<string, unknown>;
		const displayPath = output.display_path;
		if (typeof displayPath === 'string') return displayPath;
		const path = output.path;
		return typeof path === 'string' ? path : null;
	}

	function successLabel(step: NativeToolStep): string {
		const output = typeof step.output === 'object' && step.output !== null
			? (step.output as Record<string, unknown>)
			: null;
		if (
			step.name === 'filesystem.write' ||
			step.name === 'filesystem.write_user_file' ||
			step.name === 'filesystem.create_user_file' ||
			step.name === 'filesystem.replace_user_file'
		) {
			return output?.created === true ? 'Created' : 'Updated';
		}
		if (
			step.name === 'filesystem.patch' ||
			step.name === 'filesystem.edit' ||
			step.name === 'filesystem.edit_user_file' ||
			step.name === 'filesystem.edit_file'
		)
			return 'Updated';
		if (step.name === 'filesystem.append_user_file' || step.name === 'filesystem.append_file')
			return 'Appended';
		if (step.name === 'process.spawn') return 'Started process';
		if (step.name === 'process.kill') return 'Stopped process';
		return `${step.name} succeeded`;
	}

	function failureLabel(step: NativeToolStep): string {
		return step.status === 'denied' ? 'denied' : 'failed';
	}

	function isRetry(step: NativeToolStep): boolean {
		return step.status === 'retry';
	}

	interface FailureDetails {
		code?: string;
		message?: string;
		reason?: string;
		retryAction?: string;
		nextTool?: string;
		suggestedTarget?: { directory?: string; relative_path?: string };
	}

	function failureDetails(step: NativeToolStep): FailureDetails | null {
		if (!step.error) return null;
		try {
			const parsed = JSON.parse(step.error) as unknown;
			if (typeof parsed !== 'object' || parsed === null) return null;
			const root = parsed as Record<string, unknown>;
			const nested = typeof root.error === 'object' && root.error !== null
				? root.error as Record<string, unknown>
				: root;
			const suggested = nested.suggested_target;
			return {
				...(typeof nested.code === 'string' ? { code: nested.code } : {}),
				...(typeof nested.message === 'string' ? { message: nested.message } : {}),
				...(typeof nested.reason === 'string' ? { reason: nested.reason } : {}),
				...(typeof nested.retry_action === 'string' ? { retryAction: nested.retry_action } : {}),
				...(typeof nested.next_tool === 'string' ? { nextTool: nested.next_tool } : {}),
				...(typeof suggested === 'object' && suggested !== null
					? { suggestedTarget: suggested as FailureDetails['suggestedTarget'] }
					: {})
			};
		} catch {
			return null;
		}
	}

	function failureMessage(step: NativeToolStep): string {
		const raw = (step.error ?? 'Native operation failed').replace(/^tool [^:]+ failed:\s*/i, '');
		const details = failureDetails(step);
		if (details?.message) return details.message;
		if (details?.reason) return details.reason;
		return raw;
	}

	function failureTitle(step: NativeToolStep): string {
		const code = failureDetails(step)?.code;
		switch (code) {
			case 'file_not_found':
				return 'File does not exist';
			case 'stale_file_ref':
				return 'File reference is stale';
			case 'target_is_directory':
				return 'Target is a directory';
			case 'repeated_tool_call':
				return 'Repeated operation blocked';
			case 'invalid_file_target':
				return 'File target needs correction';
			default:
				return step.status === 'retry' ? 'Operation needs correction' : `${step.name} ${failureLabel(step)}`;
		}
	}

	function failureAction(step: NativeToolStep): string | null {
		const details = failureDetails(step);
		if (!details) return null;
		if (details.code === 'repeated_tool_call' || details.retryAction === 'change_arguments') {
			return 'Change the target or operation; the same call will not be retried.';
		}
		if (details.code === 'file_not_found' || details.retryAction === 'change_operation_or_target') {
			const suggested = details.suggestedTarget;
			const target = suggested?.directory && suggested.relative_path
				? ` Suggested target: ${suggested.directory}/${suggested.relative_path}.`
				: '';
			return `Check the target, or create the file with filesystem.create_user_file.${target}`;
		}
		if (details.code === 'stale_file_ref' || details.retryAction === 'refresh_file_reference') {
			return 'Refresh the file with filesystem.read, filesystem.stat, or filesystem.list.';
		}
		if (details.code === 'target_is_directory') {
			return 'Choose a file inside this directory with filesystem.list.';
		}
		return details.nextTool ? `Next step: ${details.nextTool}.` : null;
	}

	function retrySummaryLabel(): string {
		const step = steps.find((candidate) => candidate.status === 'retry');
		return step ? failureTitle(step) : 'Operation needs correction';
	}

	function failureSummaryLabel(): string {
		const step = steps.find((candidate) => !candidate.ok);
		return step ? failureTitle(step) : 'Native operation failed';
	}

	function failedSteps(): NativeToolStep[] {
		return steps.filter((step) => !step.ok);
	}
</script>

{#if steps.some(shouldShow)}
	<div class="native-tool-receipts" class:floating aria-label="Native tool results">
		{#if summarizeNativeToolSteps(steps) === 'recovered'}
			<div class="native-tool-recovered" role="status">✓ Completed after retry</div>
		{:else if summarizeNativeToolSteps(steps) === 'retry'}
			<div class="native-tool-retry" role="status">↻ {retrySummaryLabel()}</div>
		{:else if summarizeNativeToolSteps(steps) === 'failed'}
			<div class="native-tool-warning" role="alert">⚠ {failureSummaryLabel()}</div>
		{/if}
		{#if summarizeNativeToolSteps(steps) === 'recovered'}
			{#each steps.filter((step) => step.ok && isNativeMutation(step.name)) as step, index (`success-${step.id}-${index}`)}
				{@const path = outputPath(step)}
				<div class="native-tool-receipt success">
					<span>✓ {successLabel(step)}</span>
					{#if path}<code>{path}</code>{/if}
				</div>
			{/each}
			{#if failedSteps().length > 0}
				<details class="native-tool-details">
					<summary>Earlier attempts ({failedSteps().length})</summary>
					{#each failedSteps() as step, index (`failure-${step.id}-${index}`)}
						{#if isRetry(step)}
							<div class="native-tool-receipt retry">
								<span>↻ {failureTitle(step)}</span>
								<small>{failureMessage(step)}</small>
								{#if failureAction(step)}<small>{failureAction(step)}</small>{/if}
							</div>
						{:else}
							<div class="native-tool-receipt failure">
								<span>✗ {failureTitle(step)}</span>
								<small>{failureMessage(step)}</small>
								{#if failureAction(step)}<small>{failureAction(step)}</small>{/if}
							</div>
						{/if}
					{/each}
				</details>
			{/if}
		{:else}
			{#each steps.filter(shouldShow) as step, index (`${step.id}-${index}`)}
				{@const path = outputPath(step)}
				{#if step.ok}
					<div class="native-tool-receipt success">
						<span>✓ {successLabel(step)}</span>
						{#if path}<code>{path}</code>{/if}
					</div>
				{:else if isRetry(step)}
					<div class="native-tool-receipt retry">
						<span>↻ {failureTitle(step)}</span>
						<small>{failureMessage(step)}</small>
						{#if failureAction(step)}<small>{failureAction(step)}</small>{/if}
					</div>
				{:else}
					<div class="native-tool-receipt failure">
						<span>✗ {failureTitle(step)}</span>
						<small>{failureMessage(step)}</small>
						{#if failureAction(step)}<small>{failureAction(step)}</small>{/if}
					</div>
				{/if}
			{/each}
		{/if}
	</div>
{/if}

<style>
	.native-tool-receipts {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		font-size: 0.75rem;
	}

	.native-tool-receipts.floating {
		position: fixed;
		left: 50%;
		bottom: 7rem;
		z-index: 55;
		width: min(560px, calc(100vw - 2rem));
		transform: translateX(-50%);
		pointer-events: auto;
	}

	.native-tool-warning {
		padding: 0.4rem 0.55rem;
		border-radius: var(--radius-md);
		background: color-mix(in srgb, #f59e0b, transparent 86%);
		color: #b45309;
		font-weight: 600;
	}

	.native-tool-recovered {
		padding: 0.4rem 0.55rem;
		border-radius: var(--radius-md);
		background: color-mix(in srgb, #22c55e, transparent 88%);
		color: #15803d;
		font-weight: 700;
	}

	.native-tool-retry {
		padding: 0.4rem 0.55rem;
		border-radius: var(--radius-md);
		background: color-mix(in srgb, #f59e0b, transparent 90%);
		color: #b45309;
		font-weight: 600;
	}

	.native-tool-details {
		margin-top: 0.1rem;
		color: var(--text-secondary);
	}

	.native-tool-details summary {
		cursor: pointer;
		padding: 0.25rem 0.55rem;
		font-size: 0.9em;
	}

	.native-tool-receipt {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		padding: 0.4rem 0.55rem;
		border-left: 2px solid var(--border-subtle);
		background: color-mix(in srgb, var(--bg-secondary), transparent 18%);
	}

	.native-tool-receipt.success {
		border-left-color: #22c55e;
		color: var(--text-secondary);
	}

	.native-tool-receipt.failure {
		border-left-color: #ef4444;
		color: var(--text-secondary);
	}

	.native-tool-receipt.failure > span {
		color: #b91c1c;
		font-weight: 600;
	}

	.native-tool-receipt.retry {
		border-left-color: #f59e0b;
		color: var(--text-secondary);
	}

	.native-tool-receipt.retry > span {
		color: #b45309;
		font-weight: 600;
	}

	.native-tool-receipt small {
		white-space: pre-wrap;
		word-break: break-word;
	}

	.native-tool-receipt code {
		font-family: var(--font-mono);
		font-size: 0.9em;
		word-break: break-all;
	}
</style>
