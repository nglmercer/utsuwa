<script lang="ts">
	import { onDestroy } from 'svelte';
	import { Button, Icon } from '$lib/components/ui';
	import {
		MicrophoneMonitor,
		type MicrophoneMonitorError,
		type MicrophoneMonitorState
	} from '$lib/services/media/microphone-monitor';
	import { sttStore, type SttSessionObserver } from '$lib/stores/stt.svelte';
	import { getSTTProvider } from '$lib/services/providers/registry';

	type SttTestState = 'idle' | 'starting' | 'listening' | 'transcribing' | 'success' | 'error';

	let microphoneState = $state<MicrophoneMonitorState>('idle');
	let microphoneLevel = $state(0);
	let microphoneError = $state<MicrophoneMonitorError | null>(null);
	let hasLevelMeter = $state(false);
	let sttTestState = $state<SttTestState>('idle');
	let sttTestTranscript = $state('');
	let sttTestError = $state<string | null>(null);
	let sttTestSessionId = 0;

	const activeProvider = $derived(sttStore.activeProvider);
	const activeProviderName = $derived(
		activeProvider === 'web-speech'
			? 'Browser Web Speech'
			: (getSTTProvider(activeProvider)?.name ?? activeProvider)
	);

	const microphoneMonitor = new MicrophoneMonitor({
		onStateChange: (state) => {
			microphoneState = state;
			if (state !== 'monitoring') microphoneLevel = 0;
			hasLevelMeter = microphoneMonitor.getHasLevelMeter();
		},
		onLevel: (level) => {
			microphoneLevel = level;
		},
		onError: (error) => {
			microphoneError = error;
		}
	});

	function getMicrophoneStatus(): string {
		switch (microphoneState) {
			case 'requesting':
				return 'Requesting microphone permission…';
			case 'monitoring':
				return hasLevelMeter ? 'Microphone is active' : 'Microphone access works';
			case 'error':
				return 'Microphone test failed';
			default:
				return 'Not tested yet';
		}
	}

	function formatBoolean(value: boolean | undefined): string {
		return value === undefined ? 'unknown' : value ? 'yes' : 'no';
	}

	async function toggleMicrophoneMonitor(): Promise<void> {
		if (microphoneState === 'monitoring') {
			microphoneMonitor.stop();
			return;
		}
		if (microphoneState === 'requesting') return;

		microphoneError = null;
		hasLevelMeter = false;
		await microphoneMonitor.start();
		hasLevelMeter = microphoneMonitor.getHasLevelMeter();
	}

	function handleSttError(message: string, sessionId: number): void {
		if (sessionId !== sttTestSessionId) return;
		sttTestError = message;
		sttTestState = 'error';
	}

	function handleSttEnd(text: string, sessionId: number): void {
		if (sessionId !== sttTestSessionId) return;
		if (text.trim()) return;
		sttTestError = 'No speech was detected. Speak clearly, then pause to finish the test.';
		sttTestState = 'error';
	}

	async function startSttTest(): Promise<void> {
		if (sttTestState === 'starting' || sttTestState === 'listening' || sttTestState === 'transcribing') return;
		if (sttStore.isListening || sttStore.isTranscribing) {
			sttTestError = 'Voice input is already active. Stop the current recording before running this test.';
			sttTestState = 'error';
			return;
		}

		microphoneMonitor.stop();
		const sessionId = ++sttTestSessionId;
		sttTestTranscript = '';
		sttTestError = null;
		sttTestState = 'starting';

		const observer: SttSessionObserver = {
			onError: (message) => handleSttError(message, sessionId),
			onEnd: (text) => handleSttEnd(text, sessionId)
		};
		const started = await sttStore.startListening(
			(text) => {
				if (sessionId !== sttTestSessionId) return;
				sttTestTranscript = text;
				sttTestState = 'success';
			},
			observer
		);

		if (sessionId !== sttTestSessionId) return;
		if (sttTestError) {
			sttTestState = 'error';
			return;
		}
		if (sttTestTranscript) {
			sttTestState = 'success';
			return;
		}
		if (!started) {
			sttTestError = sttStore.error ?? 'Voice input could not start. Check the microphone test above.';
			sttTestState = 'error';
			return;
		}
		sttTestState = sttStore.isTranscribing ? 'transcribing' : 'listening';
	}

	function stopSttTest(): void {
		if (sttTestState === 'starting' || !sttStore.isListening) {
			++sttTestSessionId;
			sttStore.cancel();
			sttTestState = 'idle';
			return;
		}

		sttTestState = 'transcribing';
		sttStore.stopListening();
	}

	function resetSttTest(): void {
		if (
			sttTestState === 'starting' ||
			sttTestState === 'listening' ||
			sttTestState === 'transcribing'
		) {
			sttStore.cancel();
		}
		++sttTestSessionId;
		sttTestState = 'idle';
		sttTestTranscript = '';
		sttTestError = null;
	}

	onDestroy(() => {
		microphoneMonitor.stop();
		if (
			sttTestState === 'starting' ||
			sttTestState === 'listening' ||
			sttTestState === 'transcribing'
		) {
			sttStore.cancel();
		}
	});
</script>

<div class="service-group diagnostics">
	<div class="service-header">
		<Icon name="sliders" size={14} />
		<span>Voice diagnostics</span>
	</div>
	<p class="diagnostics-intro">
		Use these checks to locate the failure. The microphone check never records or uploads audio;
		the STT check sends one short test recording to the active provider but never sends a chat message.
	</p>

	<div class="diagnostic-grid">
		<section class="diagnostic-card" aria-labelledby="microphone-test-heading">
			<div class="diagnostic-card-header">
				<div>
					<h3 id="microphone-test-heading">Microphone access</h3>
					<p>Checks WebView permission, OS access, and live input level.</p>
				</div>
				<span
					class="state-dot"
					class:active={microphoneState === 'monitoring'}
					class:failed={microphoneState === 'error'}
					aria-hidden="true"
				></span>
			</div>

			<div class="diagnostic-status" role="status" aria-live="polite">{getMicrophoneStatus()}</div>

			{#if microphoneState === 'monitoring'}
				<div class="level-meter" aria-label="Microphone input level">
					<div class="level-label">
						<span>Live input</span>
						<span>{Math.round(microphoneLevel * 100)}%</span>
					</div>
					<div class="level-track" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow={Math.round(microphoneLevel * 100)}>
						<div class="level-fill" style:width={`${microphoneLevel * 100}%`}></div>
					</div>
					{#if !hasLevelMeter}
						<p class="diagnostic-note">Capture succeeded, but this WebView does not expose an audio level meter.</p>
					{:else}
						<p class="diagnostic-note">Talk or tap near the microphone. Stop the test when finished.</p>
					{/if}
				</div>
			{/if}

			{#if microphoneError}
				<div class="diagnostic-error" role="alert">
					<strong>{microphoneError.userMessage}</strong>
					{#if microphoneError.category === 'permission-denied'}
						<p class="diagnostic-note">
							If access was denied earlier, clear this app's saved WebView permission and verify the operating-system microphone privacy setting, then retry.
						</p>
					{/if}
					<details open>
						<summary>Technical details</summary>
						<dl>
							<dt>Category</dt>
							<dd>{microphoneError.category}</dd>
							<dt>Error</dt>
							<dd>{microphoneError.name ?? 'unknown'}{microphoneError.message ? ` — ${microphoneError.message}` : ''}</dd>
							<dt>Origin</dt>
							<dd>{microphoneError.origin ?? 'unknown'}</dd>
							<dt>Secure context</dt>
							<dd>{formatBoolean(microphoneError.isSecureContext)}</dd>
							<dt>mediaDevices</dt>
							<dd>{formatBoolean(microphoneError.hasMediaDevices)}</dd>
							<dt>getUserMedia</dt>
							<dd>{formatBoolean(microphoneError.hasGetUserMedia)}</dd>
						</dl>
					</details>
				</div>
			{/if}

			<Button
				variant={microphoneState === 'monitoring' ? 'secondary' : 'primary'}
				size="sm"
				type="button"
				onclick={() => void toggleMicrophoneMonitor()}
				disabled={microphoneState === 'requesting'}
			>
				{#if microphoneState === 'requesting'}
					<Icon name="loader" size={13} /> Requesting permission…
				{:else if microphoneState === 'monitoring'}
					<Icon name="stop" size={12} /> Stop microphone test
				{:else}
					<Icon name="mic" size={13} /> Test microphone access
				{/if}
			</Button>
		</section>

		<section class="diagnostic-card" aria-labelledby="stt-test-heading">
			<div class="diagnostic-card-header">
				<div>
					<h3 id="stt-test-heading">Speech-to-text</h3>
					<p>Active provider: <strong>{activeProviderName}</strong></p>
				</div>
				<span
					class="state-dot"
					class:active={sttTestState === 'listening' || sttTestState === 'transcribing'}
					class:success={sttTestState === 'success'}
					class:failed={sttTestState === 'error'}
					aria-hidden="true"
				></span>
			</div>

			<div class="diagnostic-status" role="status" aria-live="polite">
				{#if sttTestState === 'starting'}
					Requesting microphone…
				{:else if sttTestState === 'listening'}
					Speak now, then pause to finish automatically.
				{:else if sttTestState === 'transcribing'}
					Transcribing test audio…
				{:else if sttTestState === 'success'}
					Transcript received.
				{:else if sttTestState === 'error'}
					Test failed.
				{:else}
					No transcription test run yet.
				{/if}
			</div>

			{#if sttTestState === 'success'}
				<div class="transcript-result">
					<span>Result</span>
					<p>{sttTestTranscript}</p>
				</div>
			{/if}

			{#if sttTestError}
				<div class="diagnostic-error" role="alert">
					<strong>{sttTestError}</strong>
					<p class="diagnostic-note">If this is a microphone error, run the microphone access check first.</p>
				</div>
			{/if}

			<div class="diagnostic-actions">
				{#if sttTestState === 'listening' || sttTestState === 'starting'}
					<Button variant="secondary" size="sm" type="button" onclick={stopSttTest}>
						<Icon name="stop" size={12} />
						{sttTestState === 'starting' ? 'Cancel test' : 'Stop and transcribe'}
					</Button>
				{:else if sttTestState === 'transcribing'}
					<Button variant="secondary" size="sm" type="button" disabled>
						<Icon name="loader" size={13} /> Transcribing…
					</Button>
				{:else}
					<Button variant="primary" size="sm" type="button" onclick={() => void startSttTest()}>
						<Icon name="mic" size={13} /> Test speech-to-text
					</Button>
				{/if}

				{#if sttTestState === 'success' || sttTestState === 'error'}
					<Button variant="ghost" size="sm" type="button" onclick={resetSttTest}>Clear</Button>
				{/if}
			</div>
		</section>
	</div>
</div>

<style>
	.diagnostics {
		gap: 0.65rem;
	}

	.diagnostics-intro {
		max-width: 780px;
		margin: 0;
		font-size: 0.75rem;
		line-height: 1.45;
		color: var(--text-tertiary);
	}

	.diagnostic-grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(250px, 1fr));
		gap: 0.75rem;
		margin-top: 0.25rem;
	}

	.diagnostic-card {
		display: flex;
		min-width: 0;
		flex-direction: column;
		gap: 0.7rem;
		padding: 0.85rem;
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-md);
		background: var(--bg-secondary);
	}

	.diagnostic-card-header {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 0.75rem;
	}

	.diagnostic-card h3 {
		margin: 0;
		font-size: 0.85rem;
		font-weight: 650;
		color: var(--text-primary);
	}

	.diagnostic-card-header p {
		margin: 0.2rem 0 0;
		font-size: 0.7rem;
		line-height: 1.35;
		color: var(--text-tertiary);
	}

	.state-dot {
		width: 0.55rem;
		height: 0.55rem;
		flex: 0 0 auto;
		margin-top: 0.2rem;
		border-radius: 50%;
		background: var(--text-tertiary);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--text-tertiary) 14%, transparent);
	}

	.state-dot.active {
		background: var(--accent);
		box-shadow: 0 0 0 3px var(--accent-muted);
	}

	.state-dot.success {
		background: var(--color-success);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--color-success) 14%, transparent);
	}

	.state-dot.failed {
		background: var(--color-error);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--color-error) 14%, transparent);
	}

	.diagnostic-status {
		min-height: 1.1rem;
		font-size: 0.76rem;
		font-weight: 600;
		color: var(--text-secondary);
	}

	.level-meter {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.level-label {
		display: flex;
		justify-content: space-between;
		font-size: 0.68rem;
		color: var(--text-tertiary);
	}

	.level-track {
		height: 0.5rem;
		overflow: hidden;
		border-radius: var(--radius-full);
		background: var(--bg-tertiary);
	}

	.level-fill {
		height: 100%;
		min-width: 2px;
		border-radius: inherit;
		background: var(--accent);
		transition: width 80ms linear;
	}

	.diagnostic-note {
		margin: 0;
		font-size: 0.68rem;
		line-height: 1.4;
		color: var(--text-tertiary);
	}

	.diagnostic-error {
		display: flex;
		flex-direction: column;
		gap: 0.45rem;
		padding: 0.65rem;
		border: 1px solid color-mix(in srgb, var(--color-error) 30%, var(--border-subtle));
		border-radius: var(--radius-sm);
		background: color-mix(in srgb, var(--color-error) 7%, var(--bg-primary));
		font-size: 0.72rem;
		line-height: 1.4;
		color: var(--text-primary);
	}

	.diagnostic-error > strong {
		color: var(--color-error);
	}

	.diagnostic-error details {
		font-size: 0.67rem;
		color: var(--text-secondary);
	}

	.diagnostic-error summary {
		cursor: pointer;
		font-weight: 600;
		color: var(--text-secondary);
	}

	.diagnostic-error dl {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		gap: 0.2rem 0.5rem;
		margin: 0.45rem 0 0;
	}

	.diagnostic-error dt {
		font-weight: 600;
	}

	.diagnostic-error dd {
		min-width: 0;
		margin: 0;
		overflow-wrap: anywhere;
		font-family: var(--font-mono);
	}

	.diagnostic-error p {
		margin: 0;
	}

	.transcript-result {
		padding: 0.65rem;
		border-left: 3px solid var(--color-success);
		border-radius: var(--radius-sm);
		background: var(--bg-primary);
	}

	.transcript-result span {
		font-size: 0.66rem;
		font-weight: 700;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--color-success);
	}

	.transcript-result p {
		margin: 0.3rem 0 0;
		font-size: 0.78rem;
		line-height: 1.4;
		color: var(--text-primary);
		word-break: break-word;
	}

	.diagnostic-actions {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 0.45rem;
		margin-top: auto;
	}
</style>
