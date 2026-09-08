import { getBridge } from './bridge';
import { buildNativeModelProviderParams, normalizeNativeBaseUrl } from './model-settings-logic';
import { getLLMProvider } from '$lib/services/providers/registry';
import { modulesStore } from '$lib/stores/modules.svelte';
import { settingsStore } from '$lib/stores/settings.svelte';

/** Model configuration sent to the native host. The key is write-only: the
 * host stores it in the OS secret store and never returns it to the WebView. */
export interface NativeModelProviderConfig {
	provider: string;
	baseUrl: string;
	model: string;
	/** Omit to preserve the host's existing key; pass an empty string to clear it. */
	apiKey?: string;
}

/** Normalize at the native boundary too, so a caller or an older saved value
 * cannot reintroduce the LM Studio/Ollama `/v1` routing bug. */
export function normalizeNativeModelProviderConfig(
	config: NativeModelProviderConfig
): NativeModelProviderConfig {
	return {
		...config,
		baseUrl: normalizeNativeBaseUrl(config.provider, config.baseUrl)
	};
}

/** Persist one complete model configuration in the host. Web builds simply
 * return because their server route owns provider configuration. */
export async function setNativeModelProvider(
	config: NativeModelProviderConfig
): Promise<boolean> {
	const bridge = getBridge();
	if (!bridge) return false;
	await bridge.invoke('settings.set_model_provider', buildNativeModelProviderParams(config));
	return true;
}

/** Synchronize the currently selected LLM after a Settings change. The
 * helper waits until provider/base URL/model are complete, which lets users
 * edit the three fields independently without sending invalid partial state. */
export async function syncNativeModelProvider(
	providerId: string,
	modelOverride?: string,
	apiKeyOverride?: string
): Promise<boolean> {
	const bridge = getBridge();
	if (!bridge) return false;
	const provider = getLLMProvider(providerId);
	if (!provider) return false;
	const providerConfig = settingsStore.getProviderConfig(providerId);
	const moduleSettings = modulesStore.getModuleSettings('consciousness');
	const model = modelOverride ?? (moduleSettings.activeModel as string | undefined) ?? '';
	const baseUrl = providerConfig.baseUrl || provider.defaultBaseUrl || '';
	if (!baseUrl || !model) return false;
	// The native host has one write-only model key slot. Clear it when the
	// selected provider changed and this provider has no frontend key, while
	// preserving a migrated key when only the model or endpoint changed.
	let nativeProvider: string | null = null;
	try {
		const native = (await bridge.invoke('settings.get_model_provider', {})) as Record<string, unknown>;
		if (typeof native.provider === 'string') nativeProvider = native.provider;
	} catch {
		// Older hosts do not expose the read-only status endpoint. The following
		// write will surface its own bridge error, and we avoid clearing a key
		// based on an unavailable status read.
	}
	let apiKey = apiKeyOverride ?? (providerConfig.apiKey || undefined);
	if (apiKey === undefined && nativeProvider !== null && nativeProvider !== providerId) {
		apiKey = '';
	}
	return setNativeModelProvider({
		provider: providerId,
		baseUrl,
		model,
		apiKey
	});
}
