<script lang="ts">
	import SttSettings from '$lib/components/settings/SttSettings.svelte';
	import SttDiagnostics from '$lib/components/settings/SttDiagnostics.svelte';
	import '../settings-page.css';
</script>

<div class="page">
	<header class="page-header">
		<h2>Voice Input</h2>
		<p>Configure speech-to-text providers and microphone sensitivity.</p>
	</header>

	<section class="section">
		<SttSettings />
	</section>

	<section class="section">
		<SttDiagnostics />
	</section>
</div>
