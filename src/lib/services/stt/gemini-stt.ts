import { GoogleGenAI, type Interactions } from '@google/genai';
import {
	isAbortError,
	RecordedSttProviderEmptyError,
	type RecordedSttTransport,
	type SttTransportContext
} from './recorded-stt.ts';

export const DEFAULT_GEMINI_STT_MODEL = 'gemini-3.5-transcribe';
export const DEFAULT_GEMINI_STT_MODE = 'smart' as const;
export const GEMINI_STT_TIMEOUT_MS = 60_000;

export type GeminiSttMode = 'smart' | 'verbatim';

export interface GeminiSttConfig {
	apiKey: string;
	model?: string;
	mode?: GeminiSttMode;
	languageCodes?: string[];
}

export type GeminiTranscriptionRequest = Interactions.CreateModelInteractionParamsNonStreaming;

export interface GeminiUploadedFile {
	name?: string;
	uri?: string;
	mimeType?: string;
}

export interface GeminiSttClient {
	files: {
		upload(params: {
			file: Blob;
			config?: {
				mimeType?: string;
				displayName?: string;
				abortSignal?: AbortSignal;
			};
		}): Promise<GeminiUploadedFile>;
		delete(params: { name: string }): Promise<unknown>;
	};
	interactions: {
		create(
			params: GeminiTranscriptionRequest,
			options?: { signal?: AbortSignal }
		): Promise<Pick<Interactions.Interaction, 'output_text'>>;
	};
}

export type GeminiSttClientFactory = (apiKey: string) => GeminiSttClient;

function createGeminiClient(apiKey: string): GeminiSttClient {
	const client = new GoogleGenAI({ apiKey });
	return {
		files: {
			upload: (params) => client.files.upload(params),
			delete: (params) => client.files.delete(params)
		},
		interactions: {
			create: async (params, options) => {
				const response = await client.interactions.create(params, options);
				if ('output_text' in response) return response;
				throw new Error('Gemini returned a streaming interaction unexpectedly.');
			}
		}
	};
}

/** Remove codec parameters from upload metadata without changing Blob bytes. */
export function normalizeAudioMimeType(mimeType: string): string {
	const normalized = mimeType.split(';', 1)[0]?.trim().toLowerCase();
	return normalized || 'audio/webm';
}

export function buildGeminiTranscriptionRequest(
	config: Pick<GeminiSttConfig, 'model' | 'mode' | 'languageCodes'>,
	uri: string,
	mimeType: string
): GeminiTranscriptionRequest {
	return {
		model: config.model || DEFAULT_GEMINI_STT_MODEL,
		input: [
			{
				type: 'audio',
				uri,
				mime_type: mimeType
			}
		],
		generation_config: {
			transcription_config: {
				language_codes: config.languageCodes ?? [],
				mode: config.mode ?? DEFAULT_GEMINI_STT_MODE
			}
		}
	};
}

interface ErrorDetails {
	status?: number;
	message?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null;
}

function getErrorDetails(error: unknown): ErrorDetails {
	const errorRecord = isRecord(error) ? error : undefined;
	const nestedError = errorRecord && isRecord(errorRecord.error) ? errorRecord.error : undefined;
	const status =
		typeof errorRecord?.status === 'number'
			? errorRecord.status
			: typeof errorRecord?.statusCode === 'number'
				? errorRecord.statusCode
				: typeof nestedError?.status === 'number'
					? nestedError.status
					: undefined;
	const message =
		error instanceof Error
			? error.message
			: typeof errorRecord?.message === 'string'
				? errorRecord.message
			: typeof nestedError?.message === 'string'
				? nestedError.message
				: undefined;

	return { status, message };
}

function isNetworkError(error: unknown, message: string): boolean {
	return (
		error instanceof TypeError ||
		/failed to fetch|network|load failed|connection|socket|econn|enotfound/i.test(message)
	);
}

function conciseMessage(message: string): string {
	const cleaned = message.replace(/\s+/g, ' ').trim();
	if (!cleaned || cleaned.startsWith('{') || cleaned.startsWith('[')) return '';
	return cleaned.length > 180 ? `${cleaned.slice(0, 177)}...` : cleaned;
}

export function formatGeminiSttError(error: unknown, stage: 'upload' | 'transcription' = 'transcription'): string {
	const { status, message: rawMessage } = getErrorDetails(error);
	const message = rawMessage ?? '';

	if (
		status === 401 ||
		status === 403 ||
		/api key|api_key|invalid.*key|unauthenticated|permission denied/i.test(message)
	) {
		return 'Invalid Gemini API key.';
	}
	if (status === 429 || /quota|rate limit|resource[ _-]exhausted|too many requests/i.test(message)) {
		return 'Gemini transcription quota exceeded.';
	}
	if (isNetworkError(error, message)) return 'Could not reach Gemini.';

	const details = conciseMessage(message);
	if (details) return `Gemini transcription failed: ${details}`;
	return stage === 'upload' ? 'Could not upload audio to Gemini.' : 'Gemini transcription failed.';
}

export class GeminiSttTransport implements RecordedSttTransport {
	readonly timeoutMs = GEMINI_STT_TIMEOUT_MS;
	readonly timeoutMessage = 'Gemini transcription timed out. Please try again.';
	readonly emptyResultMessage = 'Gemini returned an empty transcription.';

	private readonly config: GeminiSttConfig;
	private readonly clientFactory: GeminiSttClientFactory;

	constructor(config: GeminiSttConfig, clientFactory: GeminiSttClientFactory = createGeminiClient) {
		this.config = config;
		this.clientFactory = clientFactory;
	}

	async transcribe(audio: Blob, context: SttTransportContext): Promise<string> {
		if (!this.config.apiKey.trim()) throw new Error('Gemini API key is required.');
		if (context.signal.aborted) {
			const error = new Error('The Gemini transcription was aborted.');
			error.name = 'AbortError';
			throw error;
		}

		const client = this.clientFactory(this.config.apiKey);
		const mimeType = normalizeAudioMimeType(audio.type);
		const model = this.config.model || DEFAULT_GEMINI_STT_MODEL;
		let uploadedName: string | undefined;
		let stage: 'upload' | 'transcription' = 'upload';

		try {
			context.onStage?.('upload-start');
			console.debug('[Gemini STT] upload-start', {
				bytes: audio.size,
				blobMimeType: audio.type,
				uploadMimeType: mimeType,
				filename: context.filename,
				model
			});
			const uploaded = await client.files.upload({
				file: audio,
				config: {
					mimeType,
					displayName: context.filename,
					abortSignal: context.signal
				}
			});
			uploadedName = uploaded.name;
			if (!uploaded.uri) throw new Error('Gemini audio upload returned no URI.');
			console.debug('[Gemini STT] upload-success', {
				bytes: audio.size,
				blobMimeType: audio.type,
				uploadMimeType: normalizeAudioMimeType(uploaded.mimeType || mimeType),
				uriPresent: !!uploaded.uri,
				fileNamePresent: !!uploaded.name
			});
			context.onStage?.('upload-success');

			if (context.signal.aborted) {
				const error = new Error('The Gemini transcription was aborted.');
				error.name = 'AbortError';
				throw error;
			}

			stage = 'transcription';
			context.onStage?.('transcription-start');
			console.debug('[Gemini STT] transcription-start', {
				model,
				blobMimeType: audio.type,
				requestMimeType: normalizeAudioMimeType(uploaded.mimeType || mimeType),
				uriPresent: true
			});
			const interaction = await client.interactions.create(
				buildGeminiTranscriptionRequest(
					this.config,
					uploaded.uri,
					normalizeAudioMimeType(uploaded.mimeType || mimeType)
				),
				{ signal: context.signal }
			);

			if (context.signal.aborted) {
				const error = new Error('The Gemini transcription was aborted.');
				error.name = 'AbortError';
				throw error;
			}

			const text = interaction.output_text?.trim() ?? '';
			if (!text) throw new RecordedSttProviderEmptyError(this.emptyResultMessage);
			context.onStage?.('transcription-success');
			console.debug('[Gemini STT] transcription-success', { characters: text.length, model });
			return text;
		} catch (error) {
			if (isAbortError(error) || context.signal.aborted) throw error;
			if (
				error instanceof Error &&
				(error.message === 'Gemini audio upload returned no URI.' ||
					error instanceof RecordedSttProviderEmptyError)
			) {
				throw error;
			}
			throw new Error(formatGeminiSttError(error, stage));
		} finally {
			console.debug('[Gemini STT] cleanup', { uploadedFile: !!uploadedName });
			if (uploadedName) {
				try {
					await client.files.delete({ name: uploadedName });
				} catch (error) {
					console.warn('[Gemini STT] Failed to delete uploaded audio', error);
				}
			}
		}
	}
}
