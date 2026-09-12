<script lang="ts">
	import FileAccessSettings from '$lib/components/settings/FileAccessSettings.svelte';
	import ScreenShareControl from '$lib/components/settings/ScreenShareControl.svelte';
</script>

<div class="page">
	<FileAccessSettings />
	<ScreenShareControl />
</div>

<style>
	.page {
		max-width: 600px;
	}
</style>
