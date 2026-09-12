<script lang="ts">
	import { onNavigate } from '$app/navigation';
	import PermissionDialog from '$lib/components/permissions/PermissionDialog.svelte';
	import ScreenShareControl from '$lib/components/settings/ScreenShareControl.svelte';

	let { children } = $props();

	// Crossfade app-side navigations (app <-> settings) where supported.
	onNavigate((navigation) => {
		if (!document.startViewTransition) return;
		return new Promise((resolve) => {
			document.startViewTransition(async () => {
				resolve();
				await navigation.complete;
			});
		});
	});
</script>

<svelte:head>
	<meta name="robots" content="noindex, nofollow" />
</svelte:head>

<div class="app">
	{@render children()}
	<ScreenShareControl compact />
	<PermissionDialog />
</div>

<style>
	.app {
		height: 100vh;
		width: 100vw;
		overflow: hidden;
	}
</style>
