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
		const path = (step.output as Record<string, unknown>).path;
		return typeof path === 'string' ? path : null;
	}

	function successLabel(step: NativeToolStep): string {
		const output = typeof step.output === 'object' && step.output !== null
			? (step.output as Record<string, unknown>)
			: null;
		if (
			step.name === 'filesystem.write' ||
			step.name === 'filesystem.write_user_file' ||
			step.name === 'filesystem.create_user_file'
		) {
			return output?.created === true ? 'Created' : 'Updated';
		}
		if (step.name === 'filesystem.patch') return 'Updated';
		if (step.name === 'process.spawn') return 'Started process';
		if (step.name === 'process.kill') return 'Stopped process';
		return `${step.name} succeeded`;
	}

	function failureLabel(step: NativeToolStep): string {
		return step.status === 'denied' ? 'denied' : 'failed';
	}

	function failureMessage(step: NativeToolStep): string {
		return (step.error ?? 'Native operation failed').replace(/^tool [^:]+ failed:\s*/i, '');
	}

	function failedSteps(): NativeToolStep[] {
		return steps.filter((step) => !step.ok);
	}
</script>

{#if steps.some(shouldShow)}
	<div class="native-tool-receipts" class:floating aria-label="Native tool results">
		{#if summarizeNativeToolSteps(steps) === 'recovered'}
			<div class="native-tool-recovered" role="status">✓ Completed after retry</div>
		{:else if summarizeNativeToolSteps(steps) === 'failed'}
			<div class="native-tool-warning" role="alert">⚠ Native operation failed</div>
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
					<summary>Earlier failed attempts ({failedSteps().length})</summary>
					{#each failedSteps() as step, index (`failure-${step.id}-${index}`)}
						<div class="native-tool-receipt failure">
							<span>✗ {step.name} {failureLabel(step)}</span>
							<small>{failureMessage(step)}</small>
						</div>
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
				{:else}
					<div class="native-tool-receipt failure">
						<span>✗ {step.name} {failureLabel(step)}</span>
						<small>{failureMessage(step)}</small>
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
