import { browser } from '$app/environment';
import type { ProviderConfig, SttProviderId } from '$lib/types';
import { LLM_PROVIDERS, TTS_PROVIDERS, STT_PROVIDERS } from '$lib/services/providers/registry';
import { getLLMProvider } from '$lib/services/providers/registry';
import { modulesStore } from '$lib/stores/modules.svelte';
import { DEFAULT_HOTKEYS, type HotkeyConfig } from '$lib/services/platform/hotkeys';
import { getBridge } from '$lib/services/native/bridge';
import { isDesktopBuild } from '$lib/services/platform';
import { getChatBaseUrl } from '$lib/services/providers/local-endpoints';
import type { ModelInfo } from '$lib/services/providers/model-capabilities';

export type ProviderCategory = 'llm' | 'tts' | 'stt';

function parseSttProvider(value: unknown): SttProviderId | null {
	if (value === 'web-speech' || value === 'local-stt' || value === 'groq-stt' || value === 'openai-stt' || value === 'gemini-stt') {
		return value;
	}
	return null;
}

function createSettingsStore() {
	// Provider configurations (keyed by provider id)
	// This is the SINGLE SOURCE OF TRUTH for credentials
	let providerConfigs = $state<Record<string, ProviderConfig>>({});
	// A desktop key is write-only in the native host. This status lets the
	// settings UI treat a saved key as configured without reading it back.
	let nativeModelKeyProviders = $state<Record<string, boolean>>({});

	// Track which providers have been explicitly added by user
	let addedProviders = $state<Record<string, boolean>>({});
	// Null preserves the legacy automatic provider priority for existing users.
	let selectedSttProvider = $state<SttProviderId | null>(null);

	// Desktop hotkey configuration
	let hotkeys = $state<HotkeyConfig>({ ...DEFAULT_HOTKEYS });

	// Load from localStorage on init. Keep the reactive reads inside a closure:
	// Svelte 5 otherwise treats a top-level initialization block as capturing
	// only the initial value of rune state.
	function loadPersistedSettings() {
		const saved = localStorage.getItem('utsuwa-settings');
		if (saved) {
			try {
				const parsed = JSON.parse(saved);
				providerConfigs = parsed.providerConfigs ?? {};
				addedProviders = parsed.addedProviders ?? {};
				selectedSttProvider = parseSttProvider(parsed.sttProvider);
				hotkeys = { ...DEFAULT_HOTKEYS, ...parsed.hotkeys };

				// Migrate old settings format if needed
				if (parsed.anthropicApiKey && !providerConfigs.anthropic) {
					providerConfigs.anthropic = { apiKey: parsed.anthropicApiKey };
					addedProviders.anthropic = true;
				}
				if (parsed.openaiApiKey && !providerConfigs.openai) {
					providerConfigs.openai = { apiKey: parsed.openaiApiKey };
					addedProviders.openai = true;
				}
				if (parsed.elevenLabsApiKey && !providerConfigs.elevenlabs) {
					providerConfigs.elevenlabs = {
						apiKey: parsed.elevenLabsApiKey,
						voiceId: parsed.elevenLabsVoiceId
					};
					addedProviders.elevenlabs = true;
				}

				// Migrate old llmProvider/ttsProvider - mark them as added
				if (parsed.llmProvider && providerConfigs[parsed.llmProvider]?.apiKey) {
					addedProviders[parsed.llmProvider] = true;
				}
				if (parsed.ttsProvider && providerConfigs[parsed.ttsProvider]?.apiKey) {
					addedProviders[parsed.ttsProvider] = true;
				}
			} catch (e) {
				console.error('Failed to load settings:', e);
			}
		}
		if (isDesktopBuild()) void hydrateNativeModelSettings();
	}

	if (browser) loadPersistedSettings();

	function save() {
		if (browser) {
			const persistedProviderConfigs = Object.fromEntries(
				Object.entries(providerConfigs).map(([providerId, config]) => {
					if (isDesktopBuild() && LLM_PROVIDERS.some((provider) => provider.id === providerId)) {
						const { apiKey: _apiKey, ...withoutApiKey } = config;
						return [providerId, withoutApiKey];
					}
					return [providerId, config];
				})
			);
			const persistedSettings: Record<string, unknown> = {
				providerConfigs: persistedProviderConfigs,
				addedProviders,
				hotkeys
			};
			if (selectedSttProvider) persistedSettings.sttProvider = selectedSttProvider;
			localStorage.setItem('utsuwa-settings', JSON.stringify(persistedSettings));
		}
	}

	async function hydrateNativeModelSettings() {
		const bridge = getBridge();
		if (!bridge) return;
		try {
			const result = (await bridge.invoke('settings.get_model_provider', {})) as Record<string, unknown>;
			const provider = typeof result.provider === 'string' ? result.provider : '';
			const moduleSettings = modulesStore.getModuleSettings('consciousness');
			const activeProvider = moduleSettings.activeProvider as string | undefined;
			const activeModel = moduleSettings.activeModel as string | undefined;
			const activeConfig = activeProvider ? providerConfigs[activeProvider] : undefined;
			const activeMeta = activeProvider ? getLLMProvider(activeProvider) : undefined;
			const nativeHasKey = result.has_api_key === true && (!activeProvider || provider === activeProvider);

			// Migrate a pre-native desktop key before the first sanitized save. If
			// the host cannot accept it, leave the legacy value in place so an
			// upgrade cannot silently discard the user's credential.
			if (activeProvider && activeConfig?.apiKey && !nativeHasKey) {
				const model = activeModel || activeMeta?.models?.[0]?.id || '';
				const baseUrl = getChatBaseUrl(
					activeProvider,
					activeConfig.baseUrl || activeMeta?.defaultBaseUrl || ''
				);
				if (model && baseUrl) {
					await bridge.invoke('settings.set_model_provider', {
						provider: activeProvider,
						base_url: baseUrl,
						model,
						api_key: activeConfig.apiKey
					});
					providerConfigs[activeProvider] = Object.fromEntries(
						Object.entries(activeConfig).filter(([key]) => key !== 'apiKey')
					) as ProviderConfig;
					nativeModelKeyProviders = {
						...nativeModelKeyProviders,
						[activeProvider]: true
					};
				}
			} else if (activeProvider && activeConfig?.apiKey && nativeHasKey) {
				providerConfigs[activeProvider] = Object.fromEntries(
					Object.entries(activeConfig).filter(([key]) => key !== 'apiKey')
				) as ProviderConfig;
			}

			if (!provider) {
				save();
				return;
			}
			modulesStore.setModuleSetting('consciousness', 'activeProvider', provider);
			if (typeof result.model === 'string' && result.model) {
				modulesStore.setModuleSetting('consciousness', 'activeModel', result.model);
			}
			nativeModelKeyProviders = {
				...nativeModelKeyProviders,
				[provider]: nativeHasKey
			};
			const baseUrl = typeof result.base_url === 'string' ? result.base_url : '';
			if (baseUrl) {
				providerConfigs[provider] = {
					...providerConfigs[provider],
					baseUrl
				};
			}
			// Re-save after startup hydration so a legacy desktop key is scrubbed
			// from localStorage even when the user has not opened Settings yet.
			save();
		} catch {
			// A plain browser or an older host has no native settings endpoint;
			// local settings remain usable until the bridge is upgraded.
		}
	}

	// Sync settings across windows (main ↔ overlay)
	if (browser) {
		window.addEventListener('storage', (e) => {
			if (e.key === 'utsuwa-settings' && e.newValue) {
				try {
					const parsed = JSON.parse(e.newValue);
					providerConfigs = parsed.providerConfigs ?? {};
					addedProviders = parsed.addedProviders ?? {};
					selectedSttProvider = parseSttProvider(parsed.sttProvider);
					hotkeys = { ...DEFAULT_HOTKEYS, ...parsed.hotkeys };
				} catch {
					// Ignore malformed data from other window
				}
			}
		});
	}

	// Provider configuration
	function setProviderConfig(providerId: string, config: Partial<ProviderConfig>) {
		const oldApiKey = providerConfigs[providerId]?.apiKey;
		const oldBaseUrl = providerConfigs[providerId]?.baseUrl;
		providerConfigs[providerId] = {
			...providerConfigs[providerId],
			...config
		};
		// Invalidate model cache if credentials or endpoint changed.
		if ((config.apiKey !== undefined && config.apiKey !== oldApiKey) || (config.baseUrl !== undefined && config.baseUrl !== oldBaseUrl)) {
			delete providerConfigs[providerId].cachedModels;
			delete providerConfigs[providerId].modelsFetchedAt;
		}
		save();
	}

	function getProviderConfig(providerId: string): ProviderConfig {
		return providerConfigs[providerId] ?? {};
	}

	// Mark a provider as added (user has explicitly added it to their setup)
	function markProviderAdded(providerId: string) {
		addedProviders[providerId] = true;
		save();
	}

	// Remove a provider from user's setup
	function removeProvider(providerId: string) {
		delete addedProviders[providerId];
		delete providerConfigs[providerId];
		save();
	}

	// Check if a provider has been added by user
	function isProviderAdded(providerId: string): boolean {
		return addedProviders[providerId] ?? false;
	}

	function setSelectedSttProvider(providerId: SttProviderId | null): void {
		selectedSttProvider = providerId;
		save();
	}

	// Check if a provider is properly configured (has required credentials)
	function isProviderConfigured(providerId: string): boolean {
		const config = providerConfigs[providerId];

		// Find the provider metadata to check if it requires an API key
		const llmProvider = LLM_PROVIDERS.find((p) => p.id === providerId);
		const ttsProvider = TTS_PROVIDERS.find((p) => p.id === providerId);
		const sttProvider = STT_PROVIDERS.find((p) => p.id === providerId);
		const provider = llmProvider || ttsProvider || sttProvider;

		if (!provider) return false;
		if (!config && !(isDesktopBuild() && nativeModelKeyProviders[providerId] === true)) {
			return false;
		}

		// Local providers don't require API keys
		if (provider.isLocal) return true;
		if (!provider.requiresApiKey) return true;

		// For providers that require API key, check if it's set
		return !!config?.apiKey || (isDesktopBuild() && nativeModelKeyProviders[providerId] === true);
	}

	// Get all configured providers for a category
	function getConfiguredProviders(category: ProviderCategory): string[] {
		const providers = category === 'llm' ? LLM_PROVIDERS : category === 'tts' ? TTS_PROVIDERS : STT_PROVIDERS;

		return providers
			.filter((p) => {
				const isAdded = isProviderAdded(p.id);
				const isConfigured = isProviderConfigured(p.id);
				return isAdded && isConfigured;
			})
			.map((p) => p.id);
	}

	// Get all added providers (even if not fully configured)
	function getAddedProviders(category: ProviderCategory): string[] {
		const providers = category === 'llm' ? LLM_PROVIDERS : category === 'tts' ? TTS_PROVIDERS : STT_PROVIDERS;

		return providers.filter((p) => isProviderAdded(p.id)).map((p) => p.id);
	}

	// Legacy compatibility getters
	function getAnthropicApiKey(): string {
		return providerConfigs.anthropic?.apiKey ?? '';
	}

	function getOpenaiApiKey(): string {
		return providerConfigs.openai?.apiKey ?? '';
	}

	function getElevenLabsApiKey(): string {
		return providerConfigs.elevenlabs?.apiKey ?? '';
	}

	// Legacy compatibility setters
	function setAnthropicApiKey(key: string) {
		setProviderConfig('anthropic', { apiKey: key });
		markProviderAdded('anthropic');
	}

	function setOpenaiApiKey(key: string) {
		setProviderConfig('openai', { apiKey: key });
		markProviderAdded('openai');
	}

	function setElevenLabsApiKey(key: string) {
		setProviderConfig('elevenlabs', { apiKey: key });
		markProviderAdded('elevenlabs');
	}

	// Cached models management
	const CACHE_TTL_MS = 24 * 60 * 60 * 1000; // 24 hours

	function setCachedModels(providerId: string, models: ModelInfo[]) {
		setProviderConfig(providerId, {
			cachedModels: models,
			modelsFetchedAt: Date.now()
		});
	}

	function getCachedModels(providerId: string): ModelInfo[] | null {
		const config = providerConfigs[providerId];
		if (!config?.cachedModels) return null;

		// Check if cache has expired
		const age = Date.now() - (config.modelsFetchedAt ?? 0);
		if (age > CACHE_TTL_MS) return null;

		return config.cachedModels;
	}

	// Hotkey configuration
	function setHotkey(action: keyof HotkeyConfig, shortcut: string) {
		hotkeys[action] = shortcut;
		save();
	}

	function resetHotkeys() {
		hotkeys = { ...DEFAULT_HOTKEYS };
		save();
	}

	return {
		// Provider configs
		get providerConfigs() {
			return providerConfigs;
		},
		get addedProviders() {
			return addedProviders;
		},
		get selectedSttProvider() {
			return selectedSttProvider;
		},

		// Legacy compatibility getters
		get anthropicApiKey() {
			return getAnthropicApiKey();
		},
		get openaiApiKey() {
			return getOpenaiApiKey();
		},
		get elevenLabsApiKey() {
			return getElevenLabsApiKey();
		},

		// Provider management
		setProviderConfig,
		getProviderConfig,
		markProviderAdded,
		removeProvider,
		isProviderAdded,
		setSelectedSttProvider,
		isProviderConfigured,
		getConfiguredProviders,
		getAddedProviders,

		// Legacy compatibility setters
		setAnthropicApiKey,
		setOpenaiApiKey,
		setElevenLabsApiKey,

		// Cached models
		setCachedModels,
		getCachedModels,

		// Hotkeys
		get hotkeys() {
			return hotkeys;
		},
		setHotkey,
		resetHotkeys
	};
}

export const settingsStore = createSettingsStore();
