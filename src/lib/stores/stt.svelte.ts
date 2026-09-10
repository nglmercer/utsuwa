import { browser } from '$app/environment';
import { webSpeechService } from '$lib/services/stt/web-speech';
import { recordedSttService } from '$lib/services/stt/recorded-stt';
import { GeminiSttTransport, DEFAULT_GEMINI_STT_MODEL } from '$lib/services/stt/gemini-stt';
import { OpenAiSttTransport } from '$lib/services/stt/openai-stt';
import { getSTTBaseUrl, getLocalSTTConnectionHint } from '$lib/services/providers/local-endpoints';
import { getSTTProvider } from '$lib/services/providers/registry';
import { isDesktopBuild } from '$lib/services/platform/platform';
import { settingsStore } from '$lib/stores/settings.svelte';
import type { SttProviderId } from '$lib/types';

type RecordedSttProviderId = Exclude<SttProviderId, 'web-speech'>;

export interface SttSessionObserver {
	/** Called when the activation ends, including an empty/no-speech result. */
	onEnd?: (text: string) => void;
	/** Called for a provider, microphone, or transcription failure. */
	onError?: (message: string) => void;
	/** Called with the raw normalized RMS level while recorded STT is active. */
	onAudioLevel?: (level: number) => void;
	/** Called after recording stops and before the provider request begins. */
	onTranscriptionStart?: () => void;
}

export interface SttStartOptions {
	/** Keep recording until the caller presses stop, useful for provider diagnostics. */
	autoStop?: boolean;
}

function createSttStore() {
	let isListening = $state(false);
	let isTranscribing = $state(false);
	let transcript = $state('');
	let interimTranscript = $state('');
	let error = $state<string | null>(null);
	let audioLevel = $state(0);
	let errorTimeout: ReturnType<typeof setTimeout> | null = null;
	let sessionId = 0;
	let sessionMode: 'recorded' | 'web-speech' | null = null;

	// An explicit selection wins. A null selection preserves the legacy priority
	// for existing installations and users who have not chosen a provider yet.
	const activeSttProvider = $derived.by<SttProviderId>(() => {
		if (!browser) return 'web-speech';
		if (settingsStore.selectedSttProvider) return settingsStore.selectedSttProvider;
		if (settingsStore.isProviderAdded('local-stt')) return 'local-stt';
		if (settingsStore.getProviderConfig('groq-stt').apiKey) return 'groq-stt';
		if (settingsStore.getProviderConfig('openai-stt').apiKey) return 'openai-stt';
		if (settingsStore.getProviderConfig('gemini-stt').apiKey) return 'gemini-stt';
		return 'web-speech';
	});

	function configureRecordedStt(providerId: RecordedSttProviderId): void {
		const config = settingsStore.getProviderConfig(providerId);
		const meta = getSTTProvider(providerId);

		if (providerId === 'gemini-stt') {
			recordedSttService.configure(
				new GeminiSttTransport({
					apiKey: config.apiKey ?? '',
					model: config.modelId || meta?.models?.[0]?.id || DEFAULT_GEMINI_STT_MODEL
				})
			);
			return;
		}

		const baseUrl = getSTTBaseUrl(providerId, config.baseUrl || meta?.defaultBaseUrl);
		const model =
			config.modelId ||
			meta?.models?.[0]?.id ||
			(providerId === 'groq-stt' ? 'whisper-large-v3-turbo' : 'whisper-1');
		const isLocal = providerId === 'local-stt';
		recordedSttService.configure(
			new OpenAiSttTransport({
				baseUrl,
				model,
				apiKey: config.apiKey || undefined,
				label: isLocal ? 'the local STT server' : (meta?.name ?? 'the STT server'),
				connectionHint: isLocal
					? getLocalSTTConnectionHint(baseUrl, browser ? window.location.origin : undefined)
					: undefined
			})
		);
	}

	function ensureProviderConfigured(providerId: SttProviderId): boolean {
		if (providerId === 'web-speech' || providerId === 'local-stt') return true;
		const config = settingsStore.getProviderConfig(providerId);
		if (config.apiKey) return true;
		const provider = getSTTProvider(providerId);
		setError(`${provider?.name ?? 'This STT provider'} API key is required. Set it up in Settings → Voice Input.`);
		return false;
	}

	function handleResult(currentSessionId: number, text: string, isFinal: boolean): void {
		if (currentSessionId !== sessionId) return;
		if (isFinal) {
			transcript = transcript ? `${transcript} ${text}` : text;
			interimTranscript = '';
		} else {
			interimTranscript = text;
		}
	}

	function handleEnd(
		currentSessionId: number,
		onComplete: (text: string) => void,
		observer?: SttSessionObserver
	): void {
		if (currentSessionId !== sessionId) return;
		isListening = false;
		isTranscribing = false;
		sessionMode = null;
		audioLevel = 0;
		const finalText = transcript.trim();
		transcript = '';
		interimTranscript = '';
		if (finalText) onComplete(finalText);
		observer?.onEnd?.(finalText);
	}

	function handleError(currentSessionId: number, message: string, observer?: SttSessionObserver): void {
		if (currentSessionId !== sessionId) return;
		console.error('[STT Store] Error:', message);
		observer?.onError?.(message);
		setError(message);
		isListening = false;
		isTranscribing = false;
		sessionMode = null;
		transcript = '';
		interimTranscript = '';
		audioLevel = 0;
	}

	async function startListening(
		onComplete: (text: string) => void,
		observer: SttSessionObserver = {},
		options: SttStartOptions = {}
	): Promise<boolean> {
		if (!browser) return false;
		if (isListening || isTranscribing || sessionMode) return false;

		const providerId = activeSttProvider;
		if (!ensureProviderConfigured(providerId)) return false;
		if (providerId !== 'web-speech' && !recordedSttService.isSupported()) {
			showUnsupportedError();
			return false;
		}

		const currentSessionId = ++sessionId;
		sessionMode = providerId === 'web-speech' ? 'web-speech' : 'recorded';
		error = null;
		transcript = '';
		interimTranscript = '';
		audioLevel = 0;

		const callbacks = {
			onResult: (text: string, isFinal: boolean) => {
				handleResult(currentSessionId, text, isFinal);
				if (providerId === 'web-speech' && currentSessionId === sessionId) {
					audioLevel = isFinal ? 0.3 : 0.5 + Math.random() * 0.5;
				}
			},
			onEnd: () => handleEnd(currentSessionId, onComplete, observer),
			onError: (message: string) => handleError(currentSessionId, message, observer),
			onAudioLevel: (level: number) => {
				if (currentSessionId === sessionId) {
					audioLevel = level;
					observer.onAudioLevel?.(level);
				}
			},
			onTranscriptionStart: () => {
				if (currentSessionId !== sessionId) return;
				isListening = false;
				isTranscribing = true;
				observer.onTranscriptionStart?.();
			}
		};

		let started = false;
		if (providerId === 'web-speech') {
			started = webSpeechService.startListening(callbacks);
		} else {
			configureRecordedStt(providerId);
			started = await recordedSttService.startListening(callbacks, options);
		}

		if (started && currentSessionId === sessionId) {
			isListening = true;
		} else if (!started && currentSessionId === sessionId) {
			sessionMode = null;
			audioLevel = 0;
		}
		return started;
	}

	function stopListening(): void {
		if (sessionMode === 'recorded') {
			isListening = false;
			isTranscribing = true;
			recordedSttService.stopListening();
		} else if (sessionMode === 'web-speech') {
			isListening = false;
			webSpeechService.stopListening();
		}
	}

	function cancel(): void {
		const mode = sessionMode;
		++sessionId;
		sessionMode = null;
		if (mode === 'recorded') {
			recordedSttService.abort();
		} else if (mode === 'web-speech') {
			webSpeechService.abort();
		} else {
			// Covers a start canceled while getUserMedia is still pending.
			recordedSttService.abort();
			webSpeechService.abort();
		}
		isListening = false;
		isTranscribing = false;
		transcript = '';
		interimTranscript = '';
		audioLevel = 0;
	}

	function isSupported(): boolean {
		if (!browser) return false;
		if (activeSttProvider === 'web-speech') return webSpeechService.isSupported();
		return recordedSttService.isSupported();
	}

	function showUnsupportedError(): void {
		if (isDesktopBuild()) {
			setError('Add an STT API key or a local STT server in Settings → Voice Input for voice input on desktop.');
		} else {
			setError('Voice input is not supported in this browser. Add an STT provider in Settings → Voice Input, or try Chrome/Edge.');
		}
	}

	function setError(message: string): void {
		if (errorTimeout) clearTimeout(errorTimeout);
		error = message;
		errorTimeout = setTimeout(() => {
			error = null;
			errorTimeout = null;
		}, 4000);
	}

	function clearError(): void {
		if (errorTimeout) {
			clearTimeout(errorTimeout);
			errorTimeout = null;
		}
		error = null;
	}

	return {
		get activeProvider() {
			return activeSttProvider;
		},
		get isListening() {
			return isListening;
		},
		get isTranscribing() {
			return isTranscribing;
		},
		get transcript() {
			return transcript;
		},
		get interimTranscript() {
			return interimTranscript;
		},
		get displayTranscript() {
			if (transcript && interimTranscript) return `${transcript} ${interimTranscript}`;
			return transcript || interimTranscript;
		},
		get error() {
			return error;
		},
		get audioLevel() {
			return audioLevel;
		},
		startListening,
		stopListening,
		cancel,
		isSupported,
		showUnsupportedError,
		clearError
	};
}

export const sttStore = createSttStore();
