import type { ModelInfo } from './model-capabilities';
// Relative import (not $lib): this module must stay loadable under node:test.
import { getBridge, type UtsuwaBridge } from '../native/bridge.ts';
import { looksLikeOllama } from './local-endpoints.ts';
import { hasApiKey, parseOpenAICompatibleModels } from './openai-compatible.ts';
import {
	applyChatModelFilter,
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
 * Model discovery router. Native hosts fetch catalogs through the Rust
 * `providers.fetch_models` IPC (the native host performs the HTTP request,
 * so CORS-restricted and credential-bearing endpoints stay out of the
 * WebView) and parse the raw catalog with the shared parsers. Plain browsers
 * use the web-only direct-fetch implementation instead.
 */

const REQUIRES_API_KEY = new Set([
	'openai',
	'deepseek',
	'xai',
	'anthropic',
	'google',
	'elevenlabs',
	'openai-tts'
]);

/**
 * Fetch models for a provider: through the native host when the IPC bridge
 * is present, directly from the WebView otherwise.
 */
export async function fetchModelsDirect(
	providerId: string,
	apiKey?: string,
	baseUrl?: string
): Promise<{ models: ModelInfo[]; error?: string; status?: number }> {
	const bridge = getBridge();
	if (!bridge) {
		const { fetchModelsDirectWeb } = await import('./direct-models.ts');
		return fetchModelsDirectWeb(providerId, apiKey, baseUrl);
	}
	return fetchModelsViaNative(bridge, providerId, apiKey, baseUrl);
}

async function fetchModelsViaNative(
	bridge: UtsuwaBridge,
	providerId: string,
	apiKey?: string,
	baseUrl?: string
): Promise<{ models: ModelInfo[]; error?: string; status?: number }> {
	const cleanBaseUrl = resolveModelsBaseUrl(providerId, baseUrl);

	try {
		if (REQUIRES_API_KEY.has(providerId) && !apiKey) throw new Error('API key required');

		const params: Record<string, unknown> = {
			provider: providerId,
			base_url: cleanBaseUrl
		};
		// Kilo alone resolves its key from the OS keychain when the WebView
		// sends none; every other provider gets an explicit (possibly blank)
		// key so a stored key is never attached to the wrong endpoint.
		if (providerId === 'kilo') {
			if (hasApiKey(apiKey)) params.api_key = apiKey?.trim() ?? '';
		} else {
			params.api_key = hasApiKey(apiKey) ? (apiKey?.trim() ?? '') : '';
		}

		const nativeResult = await bridge.invoke('providers.fetch_models', params);
		const resultObject =
			nativeResult && typeof nativeResult === 'object' && !Array.isArray(nativeResult)
				? (nativeResult as Record<string, unknown>)
				: undefined;
		const catalog = resultObject?.catalog ?? nativeResult;
		const authenticated = resultObject?.authenticated === true;

		let models: ModelInfo[];
		switch (providerId) {
			case 'openai':
			case 'deepseek':
			case 'xai':
				models = parseOpenAIListModels(catalog, providerId);
				break;
			case 'kilo':
				// In anonymous mode only expose models the catalog explicitly
				// marks as free, so a paid model is never selected silently.
				// An optional key — or a keychain key the host resolved without
				// exposing it to the WebView — expands the catalog and keeps
				// free models first.
				models = parseOpenAICompatibleModels(catalog, {
					classifyFree: true,
					onlyFree: !hasApiKey(apiKey) && !authenticated,
					includeCapabilities: true
				});
				break;
			case 'openai-compatible':
				if (looksLikeOllama(cleanBaseUrl)) {
					models = parseOllamaTags(catalog);
				} else {
					models = withCompatibleToolCalling(
						parseOpenAICompatibleModels(catalog, { includeCapabilities: true })
					);
				}
				break;
			case 'anthropic':
				models = parseAnthropicModels(catalog);
				break;
			case 'ollama':
				models = parseOllamaTags(catalog);
				break;
			case 'lmstudio':
				models = parseLMStudioCatalog(catalog);
				break;
			case 'google':
				models = parseGoogleModels(catalog);
				break;
			case 'elevenlabs':
				models = parseElevenLabsModels(catalog);
				break;
			case 'openai-tts':
				models = parseOpenAITtsModels(catalog);
				break;
			default:
				return { models: [], error: `Unknown provider: ${providerId}` };
		}

		return { models: applyChatModelFilter(models, providerId) };
	} catch (error) {
		return toCatalogFailure(providerId, cleanBaseUrl, error);
	}
}
