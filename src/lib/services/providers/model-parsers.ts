import type { ModelInfo } from './model-capabilities';
import { parseLMStudioModelCapabilities } from './model-capabilities.ts';
import {
	getLocalProviderConnectionHint,
	getModelsBaseUrl,
	isLocalLLMProvider
} from './local-endpoints.ts';
import { DEFAULT_MODELS_BASE_URLS } from './provider-defaults.ts';

/**
 * Shared model-catalog parsing for the web and native model-discovery paths.
 * This module is pure: it never touches the network, so both the WebView
 * direct-fetch implementation (`direct-models.ts`, web only) and the native
 * Rust catalog path (`client-models.ts` via `providers.fetch_models`) parse
 * raw provider payloads with the same code.
 */

export interface CatalogFailure {
	models: [];
	error: string;
	/** HTTP status when the provider was reached but rejected the request. */
	status?: number;
}

/** HTTP failure that preserves the status code for health classification. */
export class CatalogHttpError extends Error {
	readonly status: number;

	constructor(status: number, message: string) {
		super(message);
		this.name = 'CatalogHttpError';
		this.status = status;
	}
}

/**
 * Best-effort status recovery from catalog error text. The native Rust client
 * reports `provider error {status}: ...` and the shared OpenAI-compatible
 * helper embeds `(HTTP {status})`; both are our own stable formats.
 */
export function extractHttpStatus(message: string): number | undefined {
	const match = message.match(/(?:provider error|HTTP)\s+(\d{3})/);
	if (!match) return undefined;
	const status = Number(match[1]);
	return Number.isInteger(status) ? status : undefined;
}

/** Base URL for a provider catalog request. Shared by both fetch paths. */
export function resolveModelsBaseUrl(providerId: string, baseUrl?: string): string {
	return providerId === 'ollama' || providerId === 'lmstudio' || providerId === 'openai-compatible'
		? getModelsBaseUrl(providerId, baseUrl)
		: providerId === 'kilo'
			? getModelsBaseUrl(providerId, baseUrl || DEFAULT_MODELS_BASE_URLS.kilo)
			: (baseUrl || DEFAULT_MODELS_BASE_URLS[providerId] || '').replace(/\/+$/, '');
}

/** Providers whose pickers list voice models rather than chat models. */
const TTS_MODEL_PROVIDERS = new Set(['elevenlabs', 'openai-tts']);

/**
 * Denylist for chat-model pickers. Provider catalogs mix chat, embedding,
 * speech, image, and moderation models; only obviously non-chat families are
 * hidden, so a future chat model appears without a client update.
 */
export const NON_CHAT_MODEL_PATTERN =
	/embedding|whisper|tts|text-to-speech|dall-e|moderation|transcribe|realtime/i;

export function applyChatModelFilter(models: ModelInfo[], providerId: string): ModelInfo[] {
	if (TTS_MODEL_PROVIDERS.has(providerId)) return models;
	return models.filter((model) => !NON_CHAT_MODEL_PATTERN.test(model.id));
}

function getCurrentSiteOrigin(): string | undefined {
	return typeof window !== 'undefined' ? window.location.origin : undefined;
}

/** Map a catalog failure to the shared `{models, error, status?}` shape. */
export function toCatalogFailure(
	providerId: string,
	baseUrl: string,
	error: unknown
): CatalogFailure {
	const message =
		isLocalLLMProvider(providerId)
			? getLocalProviderConnectionHint(providerId, baseUrl, getCurrentSiteOrigin())
			: error instanceof Error
				? error.message
				: 'Unknown error';
	const status =
		error instanceof CatalogHttpError ? error.status : extractHttpStatus(message);
	return status === undefined ? { models: [], error: message } : { models: [], error: message, status };
}

export function normalizeModelName(id: string, providerId: string): string {
	let name = id;
	if (providerId === 'google' && name.startsWith('models/')) {
		name = name.replace('models/', '');
	}
	if (providerId === 'anthropic') {
		name = name.replace(/-\d{8}$/, '');
		name = name.replace(/(opus|sonnet|haiku)-(\d+)-(\d+)$/, '$1-$2.$3');
	}
	name = name
		.replace(/-/g, ' ')
		.replace(/\b\w/g, (c) => c.toUpperCase())
		.replace(/Gpt/g, 'GPT')
		.replace(/O1/g, 'o1')
		.replace(/O3/g, 'o3');
	return name;
}

interface OpenAIListEnvelope {
	data: { id: string }[];
}

/** `{data: [{id}]}` envelopes (OpenAI, DeepSeek, xAI). */
export function parseOpenAIListModels(data: unknown, providerId: string): ModelInfo[] {
	const envelope = data as OpenAIListEnvelope;
	return envelope.data.map((m) => ({
		id: m.id,
		name: normalizeModelName(m.id, providerId)
	}));
}

/** Anthropic's `{data: [{id}]}` envelope with dated-id normalization. */
export function parseAnthropicModels(data: unknown): ModelInfo[] {
	const envelope = data as OpenAIListEnvelope;
	return envelope.data.map((m) => ({
		id: m.id,
		name: normalizeModelName(m.id, 'anthropic')
	}));
}

interface OllamaTagsEnvelope {
	models?: { name: string }[];
}

/** Ollama `/api/tags` shape, also used for Ollama-looking custom endpoints. */
export function parseOllamaTags(data: unknown): ModelInfo[] {
	const envelope = data as OllamaTagsEnvelope;
	return (envelope.models || []).map((m) => ({
		id: m.name,
		name: m.name,
		capabilities: { toolCalling: true, toolCallingSupport: 'compatible' as const }
	}));
}

/** LM Studio metadata records (`{models: [...]}` or `{data: [...]}`). */
export function parseLMStudioCatalog(data: unknown): ModelInfo[] {
	const envelope = data as Record<string, unknown>;
	const records = Array.isArray(envelope.models)
		? envelope.models
		: Array.isArray(envelope.data)
			? envelope.data
			: [];
	return (records as unknown[])
		.filter((record): record is Record<string, unknown> => {
			if (!record || typeof record !== 'object') return false;
			const type = (record as Record<string, unknown>).type;
			return type === undefined || type === 'llm' || type === 'vlm';
		})
		.map((record) => {
			const id =
				typeof record.key === 'string'
					? record.key
					: typeof record.id === 'string'
						? record.id
						: '';
			const displayName =
				typeof record.display_name === 'string'
					? record.display_name
					: typeof record.name === 'string'
						? record.name
						: id;
			return {
				id,
				name: displayName || id,
				capabilities: parseLMStudioModelCapabilities(record)
			};
		})
		.filter((model) => model.id.length > 0);
}

interface GoogleModelsEnvelope {
	models?: { name: string; displayName?: string }[];
}

/** Google AI Studio `{models: [{name: "models/..."}]}` envelope. */
export function parseGoogleModels(data: unknown): ModelInfo[] {
	const envelope = data as GoogleModelsEnvelope;
	return (envelope.models || []).map((m) => ({
		id: m.name.replace('models/', ''),
		name: m.displayName || normalizeModelName(m.name, 'google')
	}));
}

interface ElevenLabsModel {
	model_id: string;
	name: string;
	can_do_text_to_speech?: boolean;
}

/** ElevenLabs returns a bare array; only TTS-capable voices are listed. */
export function parseElevenLabsModels(data: unknown): ModelInfo[] {
	return (data as ElevenLabsModel[])
		.filter((m) => m.can_do_text_to_speech)
		.map((m) => ({
			id: m.model_id,
			name: m.name
		}));
}

/**
 * Generic-endpoint affordance: the protocol is OpenAI-compatible even when
 * `/models` omits capability metadata.
 */
export function withCompatibleToolCalling(models: ModelInfo[]): ModelInfo[] {
	return models.map((model) => ({
		...model,
		capabilities: {
			...model.capabilities,
			toolCalling: true,
			toolCallingSupport: 'compatible' as const
		}
	}));
}

/** OpenAI TTS picker: only models whose id carries the TTS family marker. */
export function parseOpenAITtsModels(data: unknown): ModelInfo[] {
	const envelope = data as OpenAIListEnvelope;
	return envelope.data
		.filter((m) => m.id.includes('tts'))
		.map((m) => ({
			id: m.id,
			name: normalizeModelName(m.id, 'openai-tts')
		}));
}
