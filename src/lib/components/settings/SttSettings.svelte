<script lang="ts">
	import { settingsStore } from '$lib/stores/settings.svelte';
	import { STT_PROVIDERS } from '$lib/services/providers/registry';
	import type { SttProviderId } from '$lib/types';
	import { Icon } from '$lib/components/ui';
	import './ai-services-settings.css';

	const selectedProvider = $derived(settingsStore.selectedSttProvider);

	function isSttProviderId(value: string): value is SttProviderId {
		return value === 'web-speech' || value === 'local-stt' || value === 'groq-stt' || value === 'openai-stt' || value === 'gemini-stt';
	}

	function handleProviderChange(value: string): void {
		settingsStore.setSelectedSttProvider(isSttProviderId(value) ? value : null);
	}

	function updateApiKey(providerId: string, value: string): void {
		settingsStore.setProviderConfig(providerId, { apiKey: value });
		if (value) settingsStore.markProviderAdded(providerId);
		else settingsStore.removeProvider(providerId);
	}
</script>

<div class="service-group">
	<div class="service-header">
		<Icon name="mic" size={14} />
		<span>Voice Input (STT)</span>
	</div>
	<p class="stt-hint">Choose a voice input provider. Automatic preserves the existing local → Groq → OpenAI → Gemini → browser priority for installations that have not selected one explicitly.</p>

	<span class="stt-sublabel">Provider</span>
	<select
		class="stt-provider-select"
		value={selectedProvider ?? ''}
		onchange={(e) => handleProviderChange(e.currentTarget.value)}
	>
		<option value="">Automatic (legacy priority)</option>
		<option value="web-speech">Browser Web Speech</option>
		{#each STT_PROVIDERS as provider}
			<option value={provider.id}>{provider.name}</option>
		{/each}
	</select>

	<div class="stt-provider-section">
		<span class="stt-sublabel">Gemini API Key</span>
		<div class="api-key-row">
			<input
				type="password"
				class="api-key-input"
				placeholder="Gemini API Key"
				value={settingsStore.getProviderConfig('gemini-stt').apiKey ?? ''}
				oninput={(e) => updateApiKey('gemini-stt', e.currentTarget.value)}
			/>
		</div>
		<div class="api-key-row">
			<input
				type="text"
				class="api-key-input stt-readonly-input"
				value="gemini-3.5-transcribe"
				readonly
				aria-label="Gemini STT model"
			/>
		</div>
		<p class="stt-note">Google Gemini uses automatic language detection and Smart transcription. Free-tier availability and quotas are controlled by Google. Google's free API tier may use submitted content to improve products.</p>
	</div>

	<div class="stt-provider-section">
		<span class="stt-sublabel">Groq API Key</span>
		<div class="api-key-row">
			<input
				type="password"
				class="api-key-input"
				placeholder="Groq API Key"
				value={settingsStore.getProviderConfig('groq-stt').apiKey ?? ''}
				oninput={(e) => updateApiKey('groq-stt', e.currentTarget.value)}
			/>
		</div>
	</div>

	<div class="stt-provider-section">
		<span class="stt-sublabel">OpenAI API Key (Whisper)</span>
		<div class="api-key-row">
			<input
				type="password"
				class="api-key-input"
				placeholder="OpenAI API Key"
				value={settingsStore.getProviderConfig('openai-stt').apiKey ?? ''}
				oninput={(e) => updateApiKey('openai-stt', e.currentTarget.value)}
			/>
		</div>
	</div>

	<div class="stt-provider-section">
		<span class="stt-sublabel">Local server (Speaches, faster-whisper-server, whisper.cpp)</span>
		<div class="api-key-row">
			<input
				type="text"
				class="api-key-input"
				placeholder="http://localhost:8000/v1/"
				value={settingsStore.getProviderConfig('local-stt').baseUrl ?? ''}
				oninput={(e) => {
					const value = e.currentTarget.value.trim();
					settingsStore.setProviderConfig('local-stt', { baseUrl: value });
					if (value) settingsStore.markProviderAdded('local-stt');
					else settingsStore.removeProvider('local-stt');
				}}
			/>
		</div>
		<div class="api-key-row">
			<input
				type="text"
				class="api-key-input"
				placeholder="Model (e.g. Systran/faster-whisper-large-v3)"
				value={settingsStore.getProviderConfig('local-stt').modelId ?? ''}
				oninput={(e) => settingsStore.setProviderConfig('local-stt', { modelId: e.currentTarget.value.trim() })}
			/>
		</div>
	</div>
</div>

<style>
	.stt-hint {
		font-size: 0.75rem;
		color: var(--text-tertiary);
		margin: 0;
		line-height: 1.4;
	}

	.stt-sublabel {
		display: block;
		font-size: 0.7rem;
		font-weight: 600;
		color: var(--text-secondary);
		margin-top: 0.25rem;
	}

	.stt-provider-select {
		width: 100%;
		padding: 0.5rem 0.75rem;
		background: var(--bg-secondary);
		border: 1px solid transparent;
		border-radius: var(--radius-lg);
		font-size: 0.8rem;
		color: var(--text-primary);
	}

	.stt-provider-select:focus {
		outline: none;
		background: var(--bg-primary);
		border-color: var(--accent);
		box-shadow: 0 0 0 3px var(--accent-muted);
	}

	.stt-provider-section {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.stt-readonly-input {
		opacity: 0.8;
	}

	.stt-note {
		font-size: 0.68rem;
		line-height: 1.4;
		color: var(--text-tertiary);
		margin: 0;
	}
</style>
