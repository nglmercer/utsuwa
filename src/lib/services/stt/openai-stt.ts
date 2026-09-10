import type { SpeechRecognitionCallbacks } from './web-speech.ts';
import {
	isAbortError,
	recordedSttService,
	type RecordedSttTransport,
	type SttTransportContext
} from './recorded-stt.ts';

export interface OpenAiSttConfig {
	// Base URL ending in /v1 (or a full custom URL). Trailing slashes are trimmed.
	baseUrl: string;
	model: string;
	// Omitted for local servers that don't require auth.
	apiKey?: string;
	// Shown in error messages, e.g. "Groq" or "the local STT server".
	label: string;
	// Optional richer message for a network failure (CORS/unreachable hints).
	connectionHint?: string;
}

// The transcription request shape is split out so it can be unit tested without
// a browser (MediaRecorder/getUserMedia). OpenAI-compatible servers — OpenAI,
// Groq, Speaches, faster-whisper-server, whisper.cpp — all accept this.
export function buildTranscriptionRequest(
	config: OpenAiSttConfig,
	audio: Blob,
	filename: string
): { url: string; headers: Record<string, string>; body: FormData } {
	const base = config.baseUrl.replace(/\/+$/, '');
	const form = new FormData();
	form.append('file', audio, filename);
	form.append('model', config.model);

	const headers: Record<string, string> = {};
	// Local servers usually need no key; only send auth when one is set.
	if (config.apiKey) {
		headers.Authorization = `Bearer ${config.apiKey}`;
	}

	return { url: `${base}/audio/transcriptions`, headers, body: form };
}

/** Transport for OpenAI-compatible /audio/transcriptions endpoints. */
export class OpenAiSttTransport implements RecordedSttTransport {
	readonly timeoutMs = 30_000;
	readonly emptyResultMessage: string;
	private readonly config: OpenAiSttConfig;

	constructor(config: OpenAiSttConfig) {
		this.config = config;
		this.emptyResultMessage = `${config.label} returned an empty transcription.`;
	}

	async transcribe(audio: Blob, context: SttTransportContext): Promise<string> {
		const { url, headers, body } = buildTranscriptionRequest(this.config, audio, context.filename);
		context.onStage?.('upload-start');
		console.debug('[STT provider] upload-start', {
			provider: this.config.label,
			bytes: audio.size,
			mimeType: audio.type,
			filename: context.filename
		});
		context.onStage?.('transcription-start');

		let response: Response;
		try {
			response = await fetch(url, {
				method: 'POST',
				headers,
				body,
				signal: context.signal
			});
		} catch (error) {
			if (isAbortError(error)) throw error;
			throw new Error(
				this.config.connectionHint ||
					(error instanceof Error ? error.message : `Failed to reach ${this.config.label}`)
			);
		}

		if (!response.ok) {
			const errorData = await response.json().catch(() => ({}));
			const msg =
				(errorData as { error?: { message?: string } })?.error?.message ||
				`${this.config.label} error (${response.status})`;
			throw new Error(msg);
		}

		context.onStage?.('upload-success');
		const data = (await response.json()) as { text?: string };
		context.onStage?.('transcription-success');
		const text = data.text?.trim() ?? '';
		console.debug('[STT provider] transcription-success', {
			provider: this.config.label,
			characters: text.length
		});
		return text;
	}
}

/**
 * Compatibility wrapper for callers that used the old OpenAI-specific service.
 * The actual recorder is shared with Gemini and all other recorded transports.
 */
class OpenAiSttService {
	private config: OpenAiSttConfig | null = null;

	configure(config: OpenAiSttConfig): void {
		this.config = config;
		recordedSttService.configure(new OpenAiSttTransport(config));
	}

	isSupported(): boolean {
		return recordedSttService.isSupported();
	}

	getIsListening(): boolean {
		return recordedSttService.getIsListening();
	}

	getIsTranscribing(): boolean {
		return recordedSttService.getIsTranscribing();
	}

	isConfigured(): boolean {
		return !!this.config?.baseUrl && !!this.config?.model;
	}

	startListening(callbacks: SpeechRecognitionCallbacks): Promise<boolean> {
		return recordedSttService.startListening(callbacks);
	}

	stopListening(): void {
		recordedSttService.stopListening();
	}

	abort(): void {
		recordedSttService.abort();
	}
}

export const openAiSttService = new OpenAiSttService();
