<script lang="ts">
	import { onMount } from 'svelte';
	import {
		attachSensorActivityListener,
		cameraActivityState,
		microphoneActivityState
	} from '$lib/services/native/sensor-activity.svelte';
	import { sensorIndicatorItems } from '$lib/services/native/sensor-activity';

	onMount(() => attachSensorActivityListener());

	const camera = $derived(cameraActivityState());
	const microphone = $derived(microphoneActivityState());
	const items = $derived(sensorIndicatorItems(camera, microphone));
</script>

{#if items.length > 0}
	<div class="sensor-indicator" role="status" aria-live="polite" aria-label="Active privacy-sensitive sensors">
		{#each items as item}
			<span class={`sensor ${item.kind}`} title={item.device ? `${item.label}: ${item.device}` : `${item.label} active`}>
				<span class="dot" aria-hidden="true"></span>
				<span>{item.label}{item.session_count > 1 ? ` (${item.session_count})` : ''}</span>
			</span>
		{/each}
	</div>
{/if}

<style>
	.sensor-indicator {
		position: fixed;
		top: 0.75rem;
		right: 0.75rem;
		z-index: 1501;
		display: flex;
		align-items: center;
		gap: 0.6rem;
		padding: 0.45rem 0.7rem;
		border: 1px solid var(--border-light, #d9d9d9);
		border-radius: 999px;
		background: color-mix(in srgb, var(--bg-primary, #fff) 94%, #ef4444);
		color: var(--text-primary, #1c2b33);
		font-size: 0.75rem;
		font-weight: 600;
		box-shadow: var(--shadow-lg, 0 8px 24px rgba(28, 43, 51, 0.1));
		pointer-events: none;
	}
	.sensor { display: inline-flex; align-items: center; gap: 0.3rem; }
	.dot {
		width: 0.5rem;
		height: 0.5rem;
		border-radius: 50%;
		background: #ef4444;
		box-shadow: 0 0 0 0.16rem rgba(239, 68, 68, 0.18);
	}
	.camera .dot { background: #f97316; box-shadow: 0 0 0 0.16rem rgba(249, 115, 22, 0.18); }
</style>
