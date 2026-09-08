import { getChatBaseUrl } from '../providers/local-endpoints.ts';

/** Pure part of native settings synchronization, kept testable without the
 * bridge or Svelte stores. */
export function normalizeNativeBaseUrl(providerId: string, baseUrl: string): string {
	return getChatBaseUrl(providerId, baseUrl);
}

export type NativeModelProviderParams = Record<string, unknown> & {
	provider: string;
	base_url: string;
	model: string;
	api_key?: string;
};

/** Build the exact bridge payload without importing Svelte stores. */
export function buildNativeModelProviderParams(config: {
	provider: string;
	baseUrl: string;
	model: string;
	apiKey?: string;
}): NativeModelProviderParams {
	return {
		provider: config.provider,
		base_url: normalizeNativeBaseUrl(config.provider, config.baseUrl),
		model: config.model,
		...(config.apiKey !== undefined ? { api_key: config.apiKey } : {})
	};
}
