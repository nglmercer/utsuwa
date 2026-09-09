import type { ModelCapabilities, ModelInfo } from './model-capabilities';

/**
 * Shared helpers for optional-key / public OpenAI-compatible gateways
 * (Kilo today; LLM7, self-hosted endpoints, or other free gateways tomorrow).
 *
 * Provider-specific code stays limited to metadata, base URLs, and model
 * classification. Transport, SSE parsing, tool calls, and chat errors live in
 * the generic OpenAI-compatible client.
 */

/** True when the value is a usable API key (not missing, empty, or blank). */
export function hasApiKey(apiKey: string | undefined | null): boolean {
	return !!apiKey && apiKey.trim().length > 0;
}

/** Return a trimmed key for an optional-key request, or undefined anonymously. */
export function normalizeOptionalApiKey(apiKey: string | undefined | null): string | undefined {
	return hasApiKey(apiKey) ? apiKey!.trim() : undefined;
}

/**
 * Authorization headers for an optional-key request. Anonymous means **no**
 * `Authorization` header — never `Bearer undefined`, an empty bearer, or a
 * placeholder key.
 */
export function optionalBearerHeaders(apiKey: string | undefined | null): Record<string, string> {
	const key = normalizeOptionalApiKey(apiKey);
	return key ? { Authorization: `Bearer ${key}` } : {};
}

/** Build an OpenAI-compatible endpoint without adding an implicit `/v1`. */
export function buildOpenAICompatibleUrl(baseUrl: string, path: string): string {
	return `${baseUrl.replace(/\/+$/, '')}/${path.replace(/^\/+/, '')}`;
}

export function openAICompatibleChatUrl(baseUrl: string): string {
	return buildOpenAICompatibleUrl(baseUrl, 'chat/completions');
}

export function openAICompatibleModelsUrl(baseUrl: string): string {
	return buildOpenAICompatibleUrl(baseUrl, 'models');
}

/** Raw model record shape returned by OpenAI-compatible `/models` endpoints. */
export interface RawOpenAIModel {
	id?: unknown;
	name?: unknown;
	display_name?: unknown;
	displayName?: unknown;
	pricing?: unknown;
	isFree?: unknown;
	is_free?: unknown;
	free?: unknown;
	supported_parameters?: unknown;
	architecture?: unknown;
	model_type?: unknown;
	[key: string]: unknown;
}

/**
 * Classify whether a model is likely usable without an API key.
 *
 * Prefers structured pricing metadata when the gateway provides it; falls
 * back to the `:free` id suffix convention otherwise. Unknown models are
 * treated as non-free so anonymous mode never silently routes to paid models.
 */
export function isFreeModel(id: string, raw?: RawOpenAIModel): boolean {
	if (raw) {
		if (raw.isFree === true || raw.is_free === true || raw.free === true) return true;
		if (raw.isFree === false || raw.is_free === false || raw.free === false) return false;
		const pricing = raw.pricing as Record<string, unknown> | null | undefined;
		if (pricing && typeof pricing === 'object') {
			const prompt = pricing.prompt;
			const completion = pricing.completion;
			const isZeroPrice = (value: unknown): boolean => {
				if (typeof value === 'number') return Number.isFinite(value) && value === 0;
				if (typeof value === 'string' && value.trim()) return Number(value) === 0;
				return false;
			};
			const isNonZeroPrice = (value: unknown): boolean => {
				if (typeof value === 'number') return Number.isFinite(value) && value !== 0;
				if (typeof value === 'string' && value.trim()) {
					const parsed = Number(value);
					return Number.isFinite(parsed) && parsed !== 0;
				}
				return false;
			};
			if (
				isZeroPrice(prompt) &&
				(completion === undefined || completion === null || isZeroPrice(completion))
			) {
				return true;
			}
			if (isNonZeroPrice(prompt) || isNonZeroPrice(completion)) {
				return false;
			}
		}
	}
	return id.trim().toLowerCase().endsWith(':free');
}

function parseStringArray(value: unknown): string[] {
	return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];
}

/** Map common OpenAI-compatible capability metadata when a gateway provides it. */
export function parseOpenAICompatibleModelCapabilities(raw: RawOpenAIModel): ModelCapabilities {
	const supported = parseStringArray(raw.supported_parameters).map((item) => item.toLowerCase());
	const architecture =
		raw.architecture && typeof raw.architecture === 'object'
			? (raw.architecture as Record<string, unknown>)
			: undefined;
	const modalities = parseStringArray(architecture?.input_modalities).map((item) => item.toLowerCase());
	const toolCalling = supported.includes('tools') || supported.includes('tool_choice');
	const vision = modalities.includes('image') || modalities.includes('video');

	return {
		...(vision ? { vision: true } : {}),
		...(toolCalling
			? { toolCalling: true, toolCallingSupport: 'compatible' as const }
			: { toolCallingSupport: 'unknown' as const })
	};
}

export interface OpenAICompatibleModelParserOptions {
	/** Attach provider pricing/free metadata to each returned model. */
	classifyFree?: boolean;
	/** Keep only models explicitly classified as free. */
	onlyFree?: boolean;
	/** Parse common tool/vision metadata when present. */
	includeCapabilities?: boolean;
	/** Preserve provider-specific display names when supplied. */
	normalizeName?: (id: string, raw: RawOpenAIModel) => string;
}

/** Parse the standard `{ data: [{ id, name, ... }] }` model response. */
export function parseOpenAICompatibleModels(
	data: unknown,
	options: OpenAICompatibleModelParserOptions = {}
): ModelInfo[] {
	const records =
		data && typeof data === 'object' && Array.isArray((data as Record<string, unknown>).data)
			? ((data as Record<string, unknown>).data as unknown[])
			: [];

	const models = records
		.map((value): ModelInfo | null => {
			if (!value || typeof value !== 'object') return null;
			const raw = value as RawOpenAIModel;
			if (typeof raw.id !== 'string' || !raw.id.trim()) return null;
			const id = raw.id;
			const displayName =
				typeof raw.name === 'string'
					? raw.name
					: typeof raw.display_name === 'string'
						? raw.display_name
						: typeof raw.displayName === 'string'
							? raw.displayName
							: id;
			const free = options.classifyFree ? isFreeModel(id, raw) : undefined;
			return {
				id,
				name: options.normalizeName?.(id, raw) || displayName || id,
				...(free !== undefined ? { free } : {}),
				...(options.includeCapabilities
					? { capabilities: parseOpenAICompatibleModelCapabilities(raw) }
					: {})
			};
		})
		.filter((model): model is ModelInfo => model !== null);

	const visible = options.onlyFree ? models.filter((model) => model.free === true) : models;
	return sortFreeModelsFirst(visible);
}

function extractErrorDetail(body: string): string | undefined {
	if (!body.trim()) return undefined;
	try {
		const value = JSON.parse(body) as Record<string, unknown>;
		const error = value.error;
		const detail =
			typeof error === 'string'
				? error
				: error && typeof error === 'object' && typeof (error as Record<string, unknown>).message === 'string'
					? ((error as Record<string, unknown>).message as string)
					: typeof value.message === 'string'
						? value.message
						: undefined;
		if (detail) return detail;
	} catch {
		// Keep a short text response as the diagnostic below.
	}
	const detail = body.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, ' ').trim();
	return detail ? detail.slice(0, 240) : undefined;
}

/** Friendly status guidance shared by optional-key model and chat requests. */
export function describeOpenAICompatibleHttpError(
	providerId: string,
	status: number,
	statusText?: string,
	detail?: string
): string {
	const isKilo = providerId === 'kilo';
	const providerName = isKilo ? 'Kilo' : 'The provider';
	let message: string;

	switch (status) {
		case 401:
		case 403:
			message = isKilo
				? `Kilo rejected the request (HTTP ${status}). Check your Kilo API key, or choose a supported free model for anonymous access.`
				: `The provider rejected the request (HTTP ${status}). Check the configured API key.`;
			break;
		case 402:
			message = 'The selected model requires payment (HTTP 402). Configure an API key with credit or choose a confirmed free model.';
			break;
		case 404:
			message = `${providerName} could not find the selected model or endpoint (HTTP 404). Double-check the base URL and model.`;
			break;
		case 429:
			message = isKilo
				? "Kilo's free-model rate limit has been reached. Try again later or configure a Kilo API key."
				: `The provider rate limit has been reached (HTTP ${status}). Try again later.`;
			break;
		default:
			if (status >= 500) {
				message = isKilo
					? `Kilo or an upstream provider failed (HTTP ${status}). Try again later.`
					: `The provider failed (HTTP ${status}). Try again later.`;
			} else {
				message = `The provider rejected the request (HTTP ${status}${statusText ? ` ${statusText}` : ''}).`;
			}
	}

	return detail ? `${message} ${detail}` : message;
}

/** Fetch and parse a standard OpenAI-compatible `/models` response. */
export async function fetchOpenAICompatibleModels(
	apiKey: string | undefined | null,
	baseUrl: string,
	providerId = 'openai-compatible',
	options: OpenAICompatibleModelParserOptions = {}
): Promise<ModelInfo[]> {
	const response = await fetch(openAICompatibleModelsUrl(baseUrl), {
		headers: optionalBearerHeaders(apiKey)
	});
	if (!response.ok) {
		const detail = extractErrorDetail(await response.text().catch(() => ''));
		throw new Error(
			describeOpenAICompatibleHttpError(providerId, response.status, response.statusText, detail)
		);
	}

	const data = await response.json();
	return parseOpenAICompatibleModels(data, options);
}

/** Sort free models first, preserving relative order otherwise (stable). */
export function sortFreeModelsFirst(models: ModelInfo[]): ModelInfo[] {
	return [...models].sort((a, b) => Number(b.free === true) - Number(a.free === true));
}

/**
 * Friendly error for a failed OpenAI-compatible `/models` request.
 * Never returns a bare "Unknown error".
 */
export function describeModelListError(
	providerId: string,
	status: number | undefined,
	statusText: string | undefined,
	detail?: string
): string {
	if (status !== undefined) return describeOpenAICompatibleHttpError(providerId, status, statusText, detail);
	const statusPart = status !== undefined ? `HTTP ${status}${statusText ? ` ${statusText}` : ''}` : 'the request failed';
	const detailPart = detail ? `: ${detail}` : '';
	return `Failed to fetch models (${statusPart})${detailPart}`;
}
