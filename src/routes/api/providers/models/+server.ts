import { env } from '$env/dynamic/private';
import type { RequestHandler } from './$types';
import type { LLMProvider } from '$lib/types';
import {
	getLMStudioApiBaseUrl,
	getModelsBaseUrl,
	isLocalLLMProvider,
	looksLikeOllama
} from '$lib/services/providers/local-endpoints';
import { assertSafeProviderUrl } from '$lib/services/providers/url-guard';
import { DEFAULT_MODELS_BASE_URLS } from '$lib/services/providers/provider-defaults';
import { parseLMStudioModelCapabilities, type ModelInfo } from '$lib/services/providers/model-capabilities';
import {
	fetchOpenAICompatibleModels,
	hasApiKey
} from '$lib/services/providers/openai-compatible';
import {
	applyChatModelFilter,
	normalizeModelName
} from '$lib/services/providers/model-parsers';
import { nodeHostResolver } from '../../dns.ts';
import { asGlobalFetch, createProviderFetch } from '../../provider-fetch.ts';

interface FetchModelsResponse {
	models: ModelInfo[];
	error?: string;
}

async function fetchOpenAIModels(
	apiKey: string | undefined,
	baseUrl: string,
	providerId = 'openai',
	httpFetch: typeof fetch = fetch
): Promise<ModelInfo[]> {
	return fetchOpenAICompatibleModels(apiKey, baseUrl, providerId, {
		includeCapabilities: true,
		normalizeName: (id) => normalizeModelName(id, providerId),
		fetchImpl: httpFetch
	});
}

async function fetchAnthropicModels(apiKey: string, baseUrl: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const response = await httpFetch(`${baseUrl}/models`, {
		headers: {
			'x-api-key': apiKey,
			'anthropic-version': '2023-06-01'
		}
	});
	if (!response.ok) throw new Error(`Failed to fetch models: ${response.statusText}`);
	const data = await response.json();
	return data.data.map((m: { id: string }) => ({
		id: m.id,
		name: normalizeModelName(m.id, 'anthropic')
	}));
}

async function fetchOllamaModels(baseUrl: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const response = await httpFetch(`${baseUrl}/api/tags`);
	if (!response.ok) throw new Error(`Failed to fetch models: ${response.statusText}`);
	const data = await response.json();
	return (data.models || []).map((m: { name: string }) => ({
		id: m.name,
		name: m.name,
		capabilities: { toolCalling: true, toolCallingSupport: 'compatible' as const }
	}));
}

async function fetchLMStudioModels(baseUrl: string, apiKey?: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const root = getLMStudioApiBaseUrl(baseUrl);
	const candidates = [`${root}/api/v1/models`, `${root}/api/v0/models`, `${root}/v1/models`];
	const headers: Record<string, string> = {};
	if (apiKey) headers.Authorization = `Bearer ${apiKey}`;
	let lastResponse: Response | undefined;
	for (const url of candidates) {
		const response = await httpFetch(url, { headers });
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
					return record.type === undefined || record.type === 'llm' || record.type === 'vlm';
				})
				.map((record) => {
					const id =
						typeof record.key === 'string'
							? record.key
							: typeof record.id === 'string'
								? record.id
								: '';
					const name =
						typeof record.display_name === 'string'
							? record.display_name
							: typeof record.name === 'string'
								? record.name
								: id;
					return { id, name: name || id, capabilities: parseLMStudioModelCapabilities(record) };
				})
				.filter((model) => model.id.length > 0);
		}
		if (response.status !== 404) break;
	}
	throw new Error(`Failed to fetch models: ${lastResponse?.statusText || 'endpoint unavailable'}`);
}

async function fetchDeepSeekModels(apiKey: string, baseUrl: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const response = await httpFetch(`${baseUrl}/models`, {
		headers: { Authorization: `Bearer ${apiKey}` }
	});
	if (!response.ok) throw new Error(`Failed to fetch models: ${response.statusText}`);
	const data = await response.json();
	return data.data.map((m: { id: string }) => ({
		id: m.id,
		name: normalizeModelName(m.id, 'deepseek')
	}));
}

async function fetchXAIModels(apiKey: string, baseUrl: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const response = await httpFetch(`${baseUrl}/models`, {
		headers: { Authorization: `Bearer ${apiKey}` }
	});
	if (!response.ok) throw new Error(`Failed to fetch models: ${response.statusText}`);
	const data = await response.json();
	return data.data.map((m: { id: string }) => ({
		id: m.id,
		name: normalizeModelName(m.id, 'xai')
	}));
}

async function fetchGoogleModels(apiKey: string, baseUrl: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const response = await httpFetch(`${baseUrl}/models`, {
		headers: { 'x-goog-api-key': apiKey }
	});
	if (!response.ok) throw new Error(`Failed to fetch models: ${response.statusText}`);
	const data = await response.json();
	return (data.models || []).map((m: { name: string; displayName?: string }) => ({
		id: m.name.replace('models/', ''),
		name: m.displayName || normalizeModelName(m.name, 'google')
	}));
}

// TTS Provider fetch functions

async function fetchElevenLabsModels(apiKey: string, baseUrl: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const response = await httpFetch(`${baseUrl}/models`, {
		headers: { 'xi-api-key': apiKey }
	});
	if (!response.ok) throw new Error(`Failed to fetch models: ${response.statusText}`);
	const models = await response.json();
	// Filter to TTS-capable models only
	return models
		.filter((m: { can_do_text_to_speech?: boolean }) => m.can_do_text_to_speech)
		.map((m: { model_id: string; name: string }) => ({
			id: m.model_id,
			name: m.name
		}));
}

async function fetchOpenAITTSModels(apiKey: string, baseUrl: string, httpFetch: typeof fetch = fetch): Promise<ModelInfo[]> {
	const response = await httpFetch(`${baseUrl}/models`, {
		headers: { Authorization: `Bearer ${apiKey}` }
	});
	if (!response.ok) throw new Error(`Failed to fetch models: ${response.statusText}`);
	const data = await response.json();
	// Filter to TTS models only (contain "tts" in name)
	return data.data
		.filter((m: { id: string }) => m.id.includes('tts'))
		.map((m: { id: string }) => ({
			id: m.id,
			name: normalizeModelName(m.id, 'openai-tts')
		}));
}

export const POST: RequestHandler = async ({ request }) => {
	try {
		const rawBody: unknown = await request.json();
		// Backstop for chunked bodies without a content-length.
		if (JSON.stringify(rawBody).length > 512 * 1024) {
			return Response.json(
				{ models: [], error: 'Request body too large' } as FetchModelsResponse,
				{ status: 413 }
			);
		}
		const parsed = rawBody as { providerId?: unknown; apiKey?: unknown; baseUrl?: unknown };
		const providerId = typeof parsed.providerId === 'string' ? parsed.providerId : '';
		const apiKey = typeof parsed.apiKey === 'string' ? parsed.apiKey : undefined;
		const baseUrl = typeof parsed.baseUrl === 'string' ? parsed.baseUrl : undefined;

		if (!providerId) {
			return Response.json({ models: [], error: 'Provider ID required' } as FetchModelsResponse, {
				status: 400
			});
		}

		const effectiveBaseUrl =
			baseUrl || DEFAULT_MODELS_BASE_URLS[providerId as LLMProvider] || '';

		// Remove trailing slash for consistency
		const cleanBaseUrl =
			providerId === 'ollama' || providerId === 'lmstudio' || providerId === 'kilo' || providerId === 'openai-compatible'
				? getModelsBaseUrl(providerId, effectiveBaseUrl)
				: effectiveBaseUrl.replace(/\/+$/, '');

		// Block SSRF: the base URL is client-supplied and fetched server-side.
		// The string check rejects obvious abuse up front; the guarded
		// transport below additionally resolves DNS (rebinding protection)
		// and re-validates every redirect hop.
		const allowLocalHosts = env.ALLOW_LOCAL_PROVIDER_HOSTS === 'true';
		try {
			assertSafeProviderUrl(cleanBaseUrl, allowLocalHosts);
		} catch (e) {
			return Response.json(
				{
					models: [],
					error: e instanceof Error ? e.message : 'Invalid provider URL'
				} as FetchModelsResponse,
				{ status: 400 }
			);
		}
		const httpFetch = asGlobalFetch(createProviderFetch(nodeHostResolver, allowLocalHosts));

		let models: ModelInfo[] = [];

		switch (providerId) {
			case 'openai':
				if (!apiKey) throw new Error('API key required for OpenAI');
				models = await fetchOpenAIModels(apiKey, cleanBaseUrl, 'openai', httpFetch);
				break;
			case 'kilo':
				models = await fetchOpenAICompatibleModels(apiKey, cleanBaseUrl, 'kilo', {
					classifyFree: true,
					onlyFree: !hasApiKey(apiKey),
					includeCapabilities: true,
					fetchImpl: httpFetch
				});
				break;
			case 'openai-compatible': {
				// OpenAI-compatible endpoints may or may not require an API key.
				if (looksLikeOllama(cleanBaseUrl)) {
					models = await fetchOllamaModels(cleanBaseUrl, httpFetch);
				} else {
					// Custom endpoints own their path semantics; do not assume `/v1`.
					// Preserve parsed capability metadata; the protocol is
					// OpenAI-compatible even when `/models` omits it.
					models = (await fetchOpenAIModels(apiKey, cleanBaseUrl, 'openai-compatible', httpFetch)).map((model) => ({
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
			case 'anthropic':
				if (!apiKey) throw new Error('API key required for Anthropic');
				models = await fetchAnthropicModels(apiKey, cleanBaseUrl, httpFetch);
				break;
			case 'ollama':
				models = await fetchOllamaModels(cleanBaseUrl, httpFetch);
				break;
			case 'lmstudio':
				models = await fetchLMStudioModels(cleanBaseUrl, apiKey, httpFetch);
				break;
			case 'deepseek':
				if (!apiKey) throw new Error('API key required for DeepSeek');
				models = await fetchDeepSeekModels(apiKey, cleanBaseUrl, httpFetch);
				break;
			case 'xai':
				if (!apiKey) throw new Error('API key required for xAI');
				models = await fetchXAIModels(apiKey, cleanBaseUrl, httpFetch);
				break;
			case 'google':
				if (!apiKey) throw new Error('API key required for Google');
				models = await fetchGoogleModels(apiKey, cleanBaseUrl, httpFetch);
				break;
			// TTS providers
			case 'elevenlabs':
				if (!apiKey) throw new Error('API key required for ElevenLabs');
				models = await fetchElevenLabsModels(apiKey, cleanBaseUrl, httpFetch);
				break;
			case 'openai-tts':
				if (!apiKey) throw new Error('API key required for OpenAI TTS');
				models = await fetchOpenAITTSModels(apiKey, cleanBaseUrl, httpFetch);
				break;
			default:
				return Response.json(
					{ models: [], error: `Unknown provider: ${providerId}` } as FetchModelsResponse,
					{ status: 400 }
				);
		}

		// Filter to chat-compatible models
		const filteredModels = applyChatModelFilter(models, providerId);

		return Response.json({ models: filteredModels } as FetchModelsResponse);
	} catch (error) {
		console.error('Error fetching models:', error);
		const message = error instanceof Error ? error.message : 'Unknown error';
		return Response.json({ models: [], error: message } as FetchModelsResponse, { status: 500 });
	}
};
