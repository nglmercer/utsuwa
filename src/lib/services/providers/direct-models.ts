import { getLMStudioApiBaseUrl, looksLikeOllama } from './local-endpoints.ts';
import type { ModelInfo } from './model-capabilities';
import {
	fetchOpenAICompatibleModels,
	hasApiKey,
	optionalBearerHeaders,
	parseOpenAICompatibleModels
} from './openai-compatible.ts';
import {
	applyChatModelFilter,
	CatalogHttpError,
	parseAnthropicModels,
	parseElevenLabsModels,
	parseGoogleModels,
	parseLMStudioCatalog,
	parseOllamaTags,
	parseOpenAIListModels,
	parseOpenAITtsModels,
	resolveModelsBaseUrl,
	toCatalogFailure,
	withCompatibleToolCalling
} from './model-parsers.ts';

/**
 * WEB-ONLY model discovery: fetches provider catalogs directly from the
 * WebView with `fetch`. Used by browser builds (local providers must be
 * reached from the user's device, never from the deployed server) and as the
 * degraded fallback when a native host has no IPC bridge.
 *
 * Never import this module from the native path: `client-models.ts` routes
 * bridged hosts to the Rust `providers.fetch_models` IPC and reaches this
 * file only through a bridge-absent dynamic import.
 */

async function readJsonCatalog(response: Response): Promise<unknown> {
	try {
		return await response.json();
	} catch {
		throw new CatalogHttpError(response.status, 'provider returned invalid model metadata');
	}
}

function throwForStatus(response: Response): void {
	if (!response.ok) {
		throw new CatalogHttpError(
			response.status,
			`Failed to fetch models: ${response.statusText}`
		);
	}
}

async function fetchLMStudioCatalog(
	baseUrl: string,
	headers: Record<string, string>
): Promise<unknown> {
	const root = getLMStudioApiBaseUrl(baseUrl);
	// Metadata endpoints first (newest to oldest), then the OpenAI-compatible
	// `/v1/models`, which every LM Studio server release exposes.
	const candidates = [`${root}/api/v1/models`, `${root}/api/v0/models`, `${root}/v1/models`];
	let lastResponse: Response | undefined;

	for (const url of candidates) {
		const response = await fetch(url, { headers });
		lastResponse = response;
		if (response.ok) return readJsonCatalog(response);
		// Older LM Studio releases do not expose the newer metadata endpoint.
		// Only fall back on a missing endpoint; auth/server failures should
		// remain visible instead of being disguised as a different failure.
		if (response.status !== 404) break;
	}

	if (!lastResponse) throw new Error('Failed to fetch models: endpoint unavailable');
	throw new CatalogHttpError(
		lastResponse.status,
		`Failed to fetch models: ${lastResponse.statusText || 'endpoint unavailable'}`
	);
}

export async function fetchModelsDirectWeb(
	providerId: string,
	apiKey?: string,
	baseUrl?: string
): Promise<{ models: ModelInfo[]; error?: string; status?: number }> {
	const cleanBaseUrl = resolveModelsBaseUrl(providerId, baseUrl);

	try {
		let models: ModelInfo[] = [];

		switch (providerId) {
			case 'openai':
			case 'deepseek':
			case 'xai': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { Authorization: `Bearer ${apiKey}` }
				});
				throwForStatus(res);
				models = parseOpenAIListModels(await readJsonCatalog(res), providerId);
				break;
			}
			case 'kilo': {
				// Kilo's catalog is public. In anonymous mode only expose models the
				// catalog explicitly marks as free, so a paid model is never selected
				// silently. An optional key expands the catalog and keeps free models
				// first.
				models = await fetchOpenAICompatibleModels(apiKey, cleanBaseUrl, 'kilo', {
					classifyFree: true,
					onlyFree: !hasApiKey(apiKey),
					includeCapabilities: true
				});
				break;
			}
			case 'openai-compatible': {
				// OpenAI-compatible endpoints (OpenRouter, Together, vLLM, ...) may or
				// may not require an API key. Keep all returned models as-is.
				// Ollama exposes an OpenAI-compatible chat endpoint but its model list
				// lives at /api/tags rather than /v1/models.
				if (looksLikeOllama(cleanBaseUrl)) {
					const res = await fetch(`${cleanBaseUrl}/api/tags`, {
						headers: optionalBearerHeaders(apiKey)
					});
					throwForStatus(res);
					models = parseOllamaTags(await readJsonCatalog(res));
				} else {
					// Custom providers own their path semantics. The shared parser does
					// not add `/v1`; a gateway may use `/openai`, `/api`, or no prefix.
					const fetchedModels = await fetchOpenAICompatibleModels(apiKey, cleanBaseUrl, providerId, {
						includeCapabilities: true
					});
					models = withCompatibleToolCalling(fetchedModels);
				}
				break;
			}
			case 'anthropic': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { 'x-api-key': apiKey, 'anthropic-version': '2023-06-01' }
				});
				throwForStatus(res);
				models = parseAnthropicModels(await readJsonCatalog(res));
				break;
			}
			case 'ollama': {
				const res = await fetch(`${cleanBaseUrl}/api/tags`);
				throwForStatus(res);
				models = parseOllamaTags(await readJsonCatalog(res));
				break;
			}
			case 'lmstudio': {
				const headers: Record<string, string> = {};
				if (apiKey) headers.Authorization = `Bearer ${apiKey}`;
				models = parseLMStudioCatalog(await fetchLMStudioCatalog(cleanBaseUrl, headers));
				break;
			}
			case 'google': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { 'x-goog-api-key': apiKey }
				});
				throwForStatus(res);
				models = parseGoogleModels(await readJsonCatalog(res));
				break;
			}
			case 'elevenlabs': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { 'xi-api-key': apiKey }
				});
				throwForStatus(res);
				models = parseElevenLabsModels(await readJsonCatalog(res));
				break;
			}
			case 'openai-tts': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { Authorization: `Bearer ${apiKey}` }
				});
				throwForStatus(res);
				models = parseOpenAITtsModels(await readJsonCatalog(res));
				break;
			}
			default:
				return { models: [], error: `Unknown provider: ${providerId}` };
		}

		return { models: applyChatModelFilter(models, providerId) };
	} catch (error) {
		return toCatalogFailure(providerId, cleanBaseUrl, error);
	}
}
