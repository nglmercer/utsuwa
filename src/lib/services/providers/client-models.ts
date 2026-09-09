import type { LLMProvider } from '$lib/types';
import {
	getLocalProviderConnectionHint,
	getLMStudioApiBaseUrl,
	getModelsBaseUrl,
	isLocalLLMProvider,
	looksLikeOllama
} from './local-endpoints';
import { DEFAULT_MODELS_BASE_URLS } from './provider-defaults.ts';
import { parseLMStudioModelCapabilities, type ModelInfo } from './model-capabilities';
import {
	fetchOpenAICompatibleModels,
	hasApiKey,
	optionalBearerHeaders,
	parseOpenAICompatibleModels
} from './openai-compatible';
import { getBridge } from '$lib/services/native/bridge';

const MODEL_FILTERS: Record<string, RegExp> = {
	openai: /^(gpt-|o1-|o3-|chatgpt-4o-)/,
	anthropic: /^claude-/,
	deepseek: /^deepseek-(chat|reasoner)/,
	xai: /^grok-/,
	google: /^gemini-/
};

function getCurrentSiteOrigin(): string | undefined {
	return typeof window !== 'undefined' ? window.location.origin : undefined;
}

function normalizeModelName(id: string, providerId: string): string {
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

async function fetchLMStudioModels(baseUrl: string, headers: Record<string, string>): Promise<ModelInfo[]> {
	const root = getLMStudioApiBaseUrl(baseUrl);
	const candidates = [`${root}/api/v1/models`, `${root}/api/v0/models`, `${root}/models`];
	let lastResponse: Response | undefined;

	for (const url of candidates) {
		const response = await fetch(url, { headers });
		lastResponse = response;
		if (response.ok) {
			const data = (await response.json()) as Record<string, unknown>;
			const records = Array.isArray(data.models)
				? data.models
				: Array.isArray(data.data)
					? data.data
					: [];
			return records
				.filter((record): record is Record<string, unknown> => {
					if (!record || typeof record !== 'object') return false;
					const type = record.type;
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

		// Older LM Studio releases do not expose the newer metadata endpoint.
		// Only fall back on a missing endpoint; auth/server failures should remain
		// visible instead of being disguised as a different API failure.
		if (response.status !== 404) break;
	}

	throw new Error(`Failed to fetch models: ${lastResponse?.statusText || 'endpoint unavailable'}`);
}

/**
 * Fetch models directly from provider APIs.
 * Used in Tauri builds where SvelteKit server routes aren't available.
 */
export async function fetchModelsDirect(
	providerId: string,
	apiKey?: string,
	baseUrl?: string
): Promise<{ models: ModelInfo[]; error?: string }> {
	const cleanBaseUrl =
		providerId === 'ollama' || providerId === 'lmstudio' || providerId === 'openai-compatible'
			? getModelsBaseUrl(providerId, baseUrl)
			: providerId === 'kilo'
				? getModelsBaseUrl(providerId, baseUrl || DEFAULT_MODELS_BASE_URLS.kilo)
				: (baseUrl || DEFAULT_MODELS_BASE_URLS[providerId] || '').replace(/\/+$/, '');

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
				if (!res.ok) throw new Error(`Failed to fetch models: ${res.statusText}`);
				const data = await res.json();
				models = data.data.map((m: { id: string }) => ({
					id: m.id,
					name: normalizeModelName(m.id, providerId)
				}));
				break;
			}
			case 'kilo': {
				// Kilo's catalog is public. In anonymous mode only expose models the
				// catalog explicitly marks as free, so a paid model is never selected
				// silently. An optional key expands the catalog and keeps free models
				// first. The native host performs the HTTP request because Kilo does
				// not enable browser CORS for its public model endpoint.
				const options = {
					classifyFree: true,
					onlyFree: !hasApiKey(apiKey),
					includeCapabilities: true
				};
				const bridge = getBridge();
				if (bridge) {
					const params: Record<string, string> = {
						provider: providerId,
						base_url: cleanBaseUrl
					};
					if (hasApiKey(apiKey)) params.api_key = apiKey?.trim() ?? '';
					const nativeResult = await bridge.invoke('providers.fetch_models', params);
					const resultObject =
						nativeResult && typeof nativeResult === 'object' && !Array.isArray(nativeResult)
							? (nativeResult as Record<string, unknown>)
							: undefined;
					const catalog = resultObject?.catalog ?? nativeResult;
					models = parseOpenAICompatibleModels(catalog, {
						...options,
						onlyFree: options.onlyFree && resultObject?.authenticated !== true
					});
				} else {
					// Keep a direct-fetch fallback for older/non-native environments;
					// normal packaged/native builds always use the bridge above.
					models = await fetchOpenAICompatibleModels(apiKey, cleanBaseUrl, 'kilo', options);
				}
				break;
			}
			case 'openai-compatible': {
				// OpenAI-compatible endpoints (OpenRouter, Together, vLLM, ...) may or
				// may not require an API key. Keep all returned models as-is.
				const headers = optionalBearerHeaders(apiKey);

				// Ollama exposes an OpenAI-compatible chat endpoint but its model list
				// lives at /api/tags rather than /v1/models.
				if (looksLikeOllama(cleanBaseUrl)) {
					const res = await fetch(`${cleanBaseUrl}/api/tags`, { headers });
					if (!res.ok) throw new Error(`Failed to fetch models: ${res.statusText}`);
					const data = await res.json();
					models = (data.models || []).map((m: { name: string }) => ({
						id: m.name,
						name: m.name,
						capabilities: { toolCalling: true, toolCallingSupport: 'compatible' as const }
					}));
				} else {
					// Custom providers own their path semantics. The shared parser does
					// not add `/v1`; a gateway may use `/openai`, `/api`, or no prefix.
					const fetchedModels = await fetchOpenAICompatibleModels(apiKey, cleanBaseUrl, providerId, {
						includeCapabilities: true
					});
					// Preserve the existing generic-endpoint affordance: the protocol is
					// OpenAI-compatible even when `/models` omits capability metadata.
					models = fetchedModels.map((model) => ({
						...model,
						capabilities: {
							...model.capabilities,
							toolCalling: true,
							toolCallingSupport: 'compatible' as const
						}
					}));
				}
				break;
			}
			case 'anthropic': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { 'x-api-key': apiKey, 'anthropic-version': '2023-06-01' }
				});
				if (!res.ok) throw new Error(`Failed to fetch models: ${res.statusText}`);
				const data = await res.json();
				models = data.data.map((m: { id: string }) => ({
					id: m.id,
					name: normalizeModelName(m.id, 'anthropic')
				}));
				break;
			}
			case 'ollama': {
				const res = await fetch(`${cleanBaseUrl}/api/tags`);
				if (!res.ok) throw new Error(`Failed to fetch models: ${res.statusText}`);
				const data = await res.json();
				models = (data.models || []).map((m: { name: string }) => ({
					id: m.name,
					name: m.name,
					capabilities: { toolCalling: true, toolCallingSupport: 'compatible' as const }
				}));
				break;
			}
			case 'lmstudio': {
				const headers: Record<string, string> = {};
				if (apiKey) headers.Authorization = `Bearer ${apiKey}`;
				models = await fetchLMStudioModels(cleanBaseUrl, headers);
				break;
			}
			case 'google': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { 'x-goog-api-key': apiKey }
				});
				if (!res.ok) throw new Error(`Failed to fetch models: ${res.statusText}`);
				const data = await res.json();
				models = (data.models || []).map(
					(m: { name: string; displayName?: string }) => ({
						id: m.name.replace('models/', ''),
						name: m.displayName || normalizeModelName(m.name, 'google')
					})
				);
				break;
			}
			case 'elevenlabs': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { 'xi-api-key': apiKey }
				});
				if (!res.ok) throw new Error(`Failed to fetch models: ${res.statusText}`);
				const data = await res.json();
				models = data
					.filter((m: { can_do_text_to_speech?: boolean }) => m.can_do_text_to_speech)
					.map((m: { model_id: string; name: string }) => ({
						id: m.model_id,
						name: m.name
					}));
				break;
			}
			case 'openai-tts': {
				if (!apiKey) throw new Error('API key required');
				const res = await fetch(`${cleanBaseUrl}/models`, {
					headers: { Authorization: `Bearer ${apiKey}` }
				});
				if (!res.ok) throw new Error(`Failed to fetch models: ${res.statusText}`);
				const data = await res.json();
				models = data.data
					.filter((m: { id: string }) => m.id.includes('tts'))
					.map((m: { id: string }) => ({
						id: m.id,
						name: normalizeModelName(m.id, 'openai-tts')
					}));
				break;
			}
			default:
				return { models: [], error: `Unknown provider: ${providerId}` };
		}

		const filter = MODEL_FILTERS[providerId];
		const filtered = filter ? models.filter((m) => filter.test(m.id)) : models;
		return { models: filtered };
	} catch (error) {
		const message =
			isLocalLLMProvider(providerId)
				? getLocalProviderConnectionHint(providerId, cleanBaseUrl, getCurrentSiteOrigin())
				: error instanceof Error ? error.message : 'Unknown error';
		return { models: [], error: message };
	}
}
