<script lang="ts">
	import McpSettings from '$lib/components/settings/McpSettings.svelte';
	import '../settings-page.css';
</script>

<div class="page">
	<div class="page-header">
		<h2>MCP Tools</h2>
		<p>Connect Model Context Protocol servers to give chat real-world tools.</p>
	</div>
	<div class="section">
		<McpSettings />
	</div>
</div>

<style>
	.page {
		max-width: 640px;
	}
</style>
