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
import { parseMcpServerConfigs, type McpServerConfig } from '$lib/services/mcp/types';
import {
	createVaultSession,
	decryptSecrets,
	decryptSecretsWithKey,
	deriveSessionKey,
	encryptSecretsWithKey,
	isVaultEnvelope,
	validatePassphrase,
	VaultError,
	type VaultEnvelope,
	type VaultSession
} from '$lib/services/security/vault';
import { mergePendingSecrets } from './settings-merge';

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

	// MCP (Model Context Protocol) servers for chat tool use. Off by default;
	// server configs are validated on load so one bad entry can't break chat.
	let mcpEnabled = $state(false);
	let mcpServers = $state<McpServerConfig[]>([]);

	// Settings vault (opt-in passphrase encryption for secrets at rest). The
	// session key lives in memory only; when set, providerConfigs/mcpServers
	// persist as an AES-GCM envelope instead of plaintext. While locked the
	// in-memory secrets are empty and consumers see "not configured".
	let vaultSet = $state(false);
	let vaultLocked = $state(false);
	// True when in-memory secrets changed while locked and are waiting for
	// an unlock to merge them into the vault (a locked save can't persist).
	let vaultPendingEdits = $state(false);
	let vaultSession: VaultSession | null = null;
	let saveChain: Promise<void> = Promise.resolve();

	// Load from localStorage on init. Keep the reactive reads inside a closure:
	// Svelte 5 otherwise treats a top-level initialization block as capturing
	// only the initial value of rune state.
	function loadPersistedSettings() {
		const saved = localStorage.getItem('utsuwa-settings');
		if (saved) {
			try {
				const parsed = JSON.parse(saved);
				if (isVaultEnvelope(parsed.vault)) {
					// Secrets stay encrypted until the user unlocks; load the
					// non-secret fields only. Consumers see "not configured".
					vaultSet = true;
					vaultLocked = true;
					addedProviders = parsed.addedProviders ?? {};
					selectedSttProvider = parseSttProvider(parsed.sttProvider);
					hotkeys = { ...DEFAULT_HOTKEYS, ...parsed.hotkeys };
					mcpEnabled = parsed.mcpEnabled === true;
				} else {
					loadPlaintextSettings(parsed);
				}
			} catch (e) {
				console.error('Failed to load settings:', e);
			}
		}
		if (isDesktopBuild()) void hydrateNativeModelSettings();
	}

	function loadPlaintextSettings(parsed: Record<string, unknown>) {
		try {
			providerConfigs = (parsed.providerConfigs as Record<string, ProviderConfig>) ?? {};
			addedProviders = (parsed.addedProviders as Record<string, boolean>) ?? {};
			selectedSttProvider = parseSttProvider(parsed.sttProvider);
			hotkeys = { ...DEFAULT_HOTKEYS, ...(parsed.hotkeys as HotkeyConfig) };
			mcpEnabled = parsed.mcpEnabled === true;
			mcpServers = parseMcpServerConfigs(parsed.mcpServers).servers;

		// Migrate old settings format if needed
		const legacyKey = (value: unknown): string | undefined =>
			typeof value === 'string' && value ? value : undefined;
		const legacyAnthropic = legacyKey(parsed.anthropicApiKey);
		if (legacyAnthropic && !providerConfigs.anthropic) {
			providerConfigs.anthropic = { apiKey: legacyAnthropic };
			addedProviders.anthropic = true;
		}
		const legacyOpenai = legacyKey(parsed.openaiApiKey);
		if (legacyOpenai && !providerConfigs.openai) {
			providerConfigs.openai = { apiKey: legacyOpenai };
			addedProviders.openai = true;
		}
		const legacyEleven = legacyKey(parsed.elevenLabsApiKey);
		if (legacyEleven && !providerConfigs.elevenlabs) {
			providerConfigs.elevenlabs = {
				apiKey: legacyEleven,
				voiceId: legacyKey(parsed.elevenLabsVoiceId)
			};
			addedProviders.elevenlabs = true;
		}

		// Migrate old llmProvider/ttsProvider - mark them as added
		const legacyLlm = legacyKey(parsed.llmProvider);
		if (legacyLlm && providerConfigs[legacyLlm]?.apiKey) {
			addedProviders[legacyLlm] = true;
		}
		const legacyTts = legacyKey(parsed.ttsProvider);
		if (legacyTts && providerConfigs[legacyTts]?.apiKey) {
			addedProviders[legacyTts] = true;
		}
		} catch (e) {
			console.error('Failed to load settings:', e);
		}
	}

	if (browser) loadPersistedSettings();

	function save() {
		if (!browser) return;
		// Serialize: encryption is async, so chain persists to keep last-write-wins.
		saveChain = saveChain
			.then(() => persist())
			.catch((e) => console.error('Failed to save settings:', e));
	}

	function persistedProviderConfigs(): Record<string, ProviderConfig> {
		return Object.fromEntries(
			Object.entries(providerConfigs).map(([providerId, config]) => {
				if (isDesktopBuild() && LLM_PROVIDERS.some((provider) => provider.id === providerId)) {
					const { apiKey: _apiKey, ...withoutApiKey } = config;
					return [providerId, withoutApiKey];
				}
				return [providerId, config];
			})
		);
	}

	async function persist() {
		const nonSecrets: Record<string, unknown> = {
			addedProviders,
			hotkeys,
			mcpEnabled
		};
		if (selectedSttProvider) nonSecrets.sttProvider = selectedSttProvider;
		if (vaultSession) {
			// Vault active: secrets persist only as an AES-GCM envelope.
			const envelope = await encryptSecretsWithKey(
				JSON.stringify({ providerConfigs: persistedProviderConfigs(), mcpServers }),
				vaultSession.key,
				vaultSession.salt
			);
			vaultSet = true;
			localStorage.setItem(
				'utsuwa-settings',
				JSON.stringify({ ...nonSecrets, providerConfigs: {}, mcpServers: [], vault: envelope })
			);
			vaultPendingEdits = false;
			return;
		}
		if (vaultLocked) {
			// Locked: persist non-secrets only and preserve the stored envelope
			// byte-for-byte (in-memory secrets are empty and must not wipe it).
			// If a programmatic writer did change secrets while locked, flag
			// them for the unlock merge instead of silently dropping them.
			vaultPendingEdits =
				Object.keys(providerConfigs).length > 0 || mcpServers.length > 0;
			let vault: unknown;
			try {
				vault = (JSON.parse(localStorage.getItem('utsuwa-settings') ?? '{}') as Record<string, unknown>)
					.vault;
			} catch {
				vault = undefined;
			}
			localStorage.setItem(
				'utsuwa-settings',
				JSON.stringify({
					...nonSecrets,
					providerConfigs: {},
					mcpServers: [],
					...(isVaultEnvelope(vault) ? { vault } : {})
				})
			);
			return;
		}
		vaultSet = false;
		localStorage.setItem(
			'utsuwa-settings',
			JSON.stringify({ ...nonSecrets, providerConfigs: persistedProviderConfigs(), mcpServers })
		);
		vaultPendingEdits = false;
	}

	function readStoredEnvelope(): VaultEnvelope | null {
		try {
			const vault = (
				JSON.parse(localStorage.getItem('utsuwa-settings') ?? '{}') as Record<string, unknown>
			).vault;
			return isVaultEnvelope(vault) ? vault : null;
		} catch {
			return null;
		}
	}

	function applyDecryptedSecrets(plaintext: string) {
		const secrets = JSON.parse(plaintext) as {
			providerConfigs?: Record<string, ProviderConfig>;
			mcpServers?: unknown;
		};
		providerConfigs =
			secrets.providerConfigs && typeof secrets.providerConfigs === 'object'
				? secrets.providerConfigs
				: {};
		mcpServers = parseMcpServerConfigs(secrets.mcpServers).servers;
	}

	/** Unlock with the vault passphrase. Returns false on a wrong passphrase. */
	async function unlockVault(passphrase: string): Promise<boolean> {
		if (!browser) return false;
		const envelope = readStoredEnvelope();
		if (!envelope) return false;
		try {
			const session = await deriveSessionKey(passphrase, envelope);
			// Snapshot locked-time edits before the decrypted envelope
			// overwrites them: they are newer, so they overlay the vault.
			const pending = vaultPendingEdits
				? { providerConfigs: { ...providerConfigs }, mcpServers: [...mcpServers] }
				: null;
			applyDecryptedSecrets(await decryptSecretsWithKey(envelope, session.key));
			if (pending) {
				const merged = mergePendingSecrets({ providerConfigs, mcpServers }, pending);
				providerConfigs = merged.providerConfigs;
				mcpServers = merged.mcpServers;
			}
			vaultSession = session;
			vaultLocked = false;
			vaultSet = true;
			vaultPendingEdits = false;
			if (pending) save();
			return true;
		} catch (e) {
			if (e instanceof VaultError) return false;
			throw e;
		}
	}

	/** Set (or re-key) the vault passphrase. Throws VaultError when too short. */
	async function setVaultPassphrase(passphrase: string): Promise<void> {
		validatePassphrase(passphrase);
		vaultSession = await createVaultSession(passphrase);
		vaultLocked = false;
		vaultSet = true;
		save();
	}

	/**
	 * Change the passphrase. The current one is always verified (against the
	 * stored envelope, or via unlock when locked). Returns false when the
	 * current passphrase is wrong; throws VaultError when the new one is weak.
	 */
	async function changeVaultPassphrase(current: string, next: string): Promise<boolean> {
		if (vaultLocked) {
			if (!(await unlockVault(current))) return false;
		} else if (vaultSet) {
			const envelope = readStoredEnvelope();
			if (!envelope) return false;
			try {
				await decryptSecrets(envelope, current);
			} catch (e) {
				if (e instanceof VaultError) return false;
				throw e;
			}
		}
		await setVaultPassphrase(next);
		return true;
	}

	/**
	 * Remove the passphrase and return secrets to plaintext storage. Only
	 * available while unlocked (locked vaults must be unlocked first).
	 */
	function removeVaultPassphrase(): boolean {
		if (vaultLocked) return false;
		vaultSession = null;
		vaultSet = false;
		save();
		return true;
	}

	/** Wipe in-memory secrets and the session key. Storage keeps the envelope. */
	function lockVault() {
		if (!vaultSet) return;
		vaultSession = null;
		providerConfigs = {};
		mcpServers = [];
		vaultLocked = true;
		vaultPendingEdits = false;
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
				void syncFromStorageEvent(e.newValue);
			}
		});
	}

	function syncNonSecrets(parsed: Record<string, unknown>) {
		addedProviders = (parsed.addedProviders as Record<string, boolean>) ?? {};
		selectedSttProvider = parseSttProvider(parsed.sttProvider);
		hotkeys = { ...DEFAULT_HOTKEYS, ...(parsed.hotkeys as HotkeyConfig) };
		mcpEnabled = parsed.mcpEnabled === true;
	}

	async function syncFromStorageEvent(raw: string) {
		try {
			const parsed = JSON.parse(raw) as Record<string, unknown>;
			syncNonSecrets(parsed);
			if (isVaultEnvelope(parsed.vault)) {
				vaultSet = true;
				// Windows don't share the session key: decrypt when possible,
				// otherwise mark locked. A decrypt failure means the vault was
				// re-keyed elsewhere — drop this window to locked as well.
				if (vaultSession) {
					try {
						applyDecryptedSecrets(await decryptSecretsWithKey(parsed.vault, vaultSession.key));
						vaultLocked = false;
					} catch {
						vaultSession = null;
						providerConfigs = {};
						mcpServers = [];
						vaultLocked = true;
					}
				} else {
					vaultLocked = true;
				}
				return;
			}
			vaultSet = false;
			if (!vaultLocked) {
				providerConfigs = (parsed.providerConfigs as Record<string, ProviderConfig>) ?? {};
				mcpServers = parseMcpServerConfigs(parsed.mcpServers).servers;
			}
		} catch {
			// Ignore malformed data from other window
		}
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

	// MCP server configuration (validated; invalid entries are dropped)
	function setMcpEnabled(enabled: boolean) {
		mcpEnabled = enabled;
		save();
	}

	function getMcpServers(): McpServerConfig[] {
		return mcpServers;
	}

	/** Replace the whole list. Returns reasons for dropped entries. */
	function setMcpServers(servers: unknown): string[] {
		const parsed = parseMcpServerConfigs(servers);
		mcpServers = parsed.servers;
		save();
		return parsed.dropped;
	}

	function addMcpServer(server: unknown): string | null {
		const parsed = parseMcpServerConfigs([server]);
		if (parsed.servers.length !== 1) return parsed.dropped[0] ?? 'Invalid server config';
		const next = parsed.servers[0];
		if (mcpServers.some((s) => s.id === next.id)) return `A server with id '${next.id}' already exists`;
		mcpServers = [...mcpServers, next];
		save();
		return null;
	}

	function updateMcpServer(id: string, patch: Record<string, unknown>): string | null {
		const index = mcpServers.findIndex((s) => s.id === id);
		if (index < 0) return `Unknown server '${id}'`;
		const parsed = parseMcpServerConfigs([{ ...mcpServers[index], ...patch, id }]);
		if (parsed.servers.length !== 1) return parsed.dropped[0] ?? 'Invalid server config';
		mcpServers = mcpServers.map((s, i) => (i === index ? parsed.servers[0] : s));
		save();
		return null;
	}

	function removeMcpServer(id: string) {
		mcpServers = mcpServers.filter((s) => s.id !== id);
		save();
	}

	function setMcpServerEnabled(id: string, enabled: boolean) {
		mcpServers = mcpServers.map((s) => (s.id === id ? { ...s, enabled } : s));
		save();
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
		resetHotkeys,

		// Settings vault (passphrase encryption for secrets at rest)
		get vaultSet() {
			return vaultSet;
		},
		get vaultLocked() {
			return vaultLocked;
		},
		get vaultPendingEdits() {
			return vaultPendingEdits;
		},
		unlockVault,
		setVaultPassphrase,
		changeVaultPassphrase,
		removeVaultPassphrase,
		lockVault,

		// MCP servers
		get mcpEnabled() {
			return mcpEnabled;
		},
		get mcpServers() {
			return mcpServers;
		},
		setMcpEnabled,
		getMcpServers,
		setMcpServers,
		addMcpServer,
		updateMcpServer,
		removeMcpServer,
		setMcpServerEnabled
	};
}

export const settingsStore = createSettingsStore();
