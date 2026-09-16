import { getChatBaseUrl, getTTSBaseUrl, isLocalLLMProvider } from './local-endpoints.ts';
import type { ToolCallingSupport } from './model-capabilities.ts';
import { fetchModelsDirect } from './client-models.ts';

export type HealthStatus = 'unknown' | 'healthy' | 'unhealthy';

interface HealthEntry {
	status: HealthStatus;
	checkedAt: number;
}

const HEALTH_CHECK_TIMEOUT_MS = 5000;
const HEALTH_CACHE_TTL_MS = 10_000;

const healthState = new Map<string, HealthEntry>();
const listeners = new Set<() => void>();

export interface ProviderHealth {
	reachable: boolean;
	endpointValid: boolean;
	modelAvailable: boolean;
	toolCalling?: ToolCallingSupport;
	vision?: boolean;
	error?: string;
}

const llmHealthState = new Map<string, { result: ProviderHealth; checkedAt: number }>();

class ProviderHealthCheckError extends Error {
	readonly reachable: boolean;
	readonly endpointValid: boolean;
	readonly status?: number;

	constructor(
		message: string,
		reachable: boolean,
		endpointValid: boolean,
		status?: number
	) {
		super(message);
		this.name = 'ProviderHealthCheckError';
		this.reachable = reachable;
		this.endpointValid = endpointValid;
		this.status = status;
	}
}

function healthKey(providerId: string, baseUrl?: string): string {
	return `${providerId}|${baseUrl ?? ''}`;
}

function emit() {
	for (const listener of listeners) {
		listener();
	}
}

export function getTTSProviderHealth(providerId: string, baseUrl?: string): HealthStatus {
	const key = healthKey(providerId, baseUrl);
	const entry = healthState.get(key);
	if (!entry) return 'unknown';
	if (Date.now() - entry.checkedAt > HEALTH_CACHE_TTL_MS) return 'unknown';
	return entry.status;
}

function pruneExpiredHealthEntries() {
	const cutoff = Date.now() - HEALTH_CACHE_TTL_MS;
	for (const [key, entry] of healthState) {
		if (entry.checkedAt < cutoff) {
			healthState.delete(key);
		}
	}
}

function setTTSProviderHealth(providerId: string, baseUrl: string | undefined, status: HealthStatus) {
	pruneExpiredHealthEntries();
	const key = healthKey(providerId, baseUrl);
	healthState.set(key, { status, checkedAt: Date.now() });
	emit();
}

export function subscribeTTSProviderHealth(callback: () => void): () => void {
	listeners.add(callback);
	return () => listeners.delete(callback);
}

function safeProviderAddress(baseUrl: string): string {
	try {
		const url = new URL(baseUrl);
		return `${url.origin}${url.pathname}`.replace(/\/+$/, '');
	} catch {
		return 'the configured provider';
	}
}

/**
 * Model discovery for health checks, bounded by the health timeout. Discovery
 * routes through the native host on desktop builds, so health checks never
 * call provider APIs directly from the WebView.
 */
function fetchModelsDirectWithTimeout(
	providerId: string,
	apiKey: string | undefined,
	baseUrl: string
): Promise<Awaited<ReturnType<typeof fetchModelsDirect>>> {
	return new Promise((resolve, reject) => {
		const timer = setTimeout(
			() => reject(new Error('Connection test timed out.')),
			HEALTH_CHECK_TIMEOUT_MS
		);
		fetchModelsDirect(providerId, apiKey, baseUrl).then(
			(value) => {
				clearTimeout(timer);
				resolve(value);
			},
			(reason) => {
				clearTimeout(timer);
				reject(reason);
			}
		);
	});
}

export function getLLMProviderHealth(
	providerId: string,
	baseUrl?: string,
	model?: string
): ProviderHealth | null {
	const entry = llmHealthState.get(`${providerId}|${baseUrl ?? ''}|${model ?? ''}`);
	if (!entry || Date.now() - entry.checkedAt > HEALTH_CACHE_TTL_MS) return null;
	return entry.result;
}

/** Check a local or custom OpenAI-compatible provider without making chat
 * dependent on a preflight. The result is cached briefly for the settings UI. */
export async function checkLLMProviderHealth(
	providerId: string,
	apiKey?: string,
	baseUrl?: string,
	model?: string
): Promise<ProviderHealth> {
	const key = `${providerId}|${baseUrl ?? ''}|${model ?? ''}`;
	if (!isLocalLLMProvider(providerId) && providerId !== 'openai-compatible') {
		const result: ProviderHealth = {
			reachable: false,
			endpointValid: false,
			modelAvailable: false,
			error: 'Connection testing is available for local and OpenAI-compatible providers.'
		};
		llmHealthState.set(key, { result, checkedAt: Date.now() });
		emit();
		return result;
	}

	let result: ProviderHealth;
	try {
		const configuredBase = baseUrl || (providerId === 'lmstudio' ? 'http://localhost:1234' : providerId === 'ollama' ? 'http://localhost:11434' : '');
		if (!configuredBase) throw new Error('Enter a provider base URL first.');
		const catalog = await fetchModelsDirectWithTimeout(providerId, apiKey, configuredBase);
		if (catalog.error) {
			// A status means the provider answered; without one the host was
			// never reached, which keeps the "could not reach" wording below.
			if (catalog.status === undefined) throw new Error(catalog.error);
			throw new ProviderHealthCheckError(catalog.error, true, false, catalog.status);
		}
		const selected = catalog.models.find((item) => item.id === model);
		result = {
			reachable: true,
			endpointValid: true,
			modelAvailable: Boolean(model && selected),
			toolCalling: selected?.capabilities?.toolCallingSupport,
			vision: selected?.capabilities?.vision
		};
	} catch (error) {
		const message = error instanceof Error && error.name === 'AbortError'
			? 'Connection test timed out.'
			: error instanceof Error
				? error.message
				: 'Could not reach the provider.';
		const address = safeProviderAddress(
			getChatBaseUrl(
				providerId,
				baseUrl || (providerId === 'lmstudio' ? 'http://localhost:1234' : providerId === 'ollama' ? 'http://localhost:11434' : '')
			)
		);
		const healthError = error instanceof ProviderHealthCheckError
			? error.status === 404 && providerId === 'lmstudio'
				? `LM Studio is reachable, but its model API was not found at ${address}. Expected an OpenAI-compatible base such as ${address || 'http://localhost:1234/v1'}.`
				: error.status === 401 || error.status === 403
					? `The provider rejected the connection test (HTTP ${error.status}). Check the configured API key.`
					: `The provider is reachable, but its model API is invalid at ${address}. ${message}`
			: undefined;
		result = {
			reachable: error instanceof ProviderHealthCheckError ? error.reachable : false,
			endpointValid: error instanceof ProviderHealthCheckError ? error.endpointValid : false,
			modelAvailable: false,
			error: healthError || (providerId === 'lmstudio'
				? `Could not reach LM Studio at ${address}. Open it, load a model, and click Start Server.`
				: providerId === 'ollama'
					? `Could not reach Ollama at ${address}. Make sure it is running with "ollama serve".`
					: `Could not reach the provider at ${address}: ${message}`)
		};
	}

	llmHealthState.set(key, { result, checkedAt: Date.now() });
	emit();
	return result;
}

function healthUrlForProvider(providerId: string, baseUrl: string): string | null {
	if (providerId === 'omnivoice') {
		const stripped = baseUrl.replace(/\/+$/, '').replace(/\/v1$/, '');
		return `${stripped}/health`;
	}

	if (providerId === 'local-tts') {
		return `${baseUrl}audio/voices`;
	}

	return null;
}

export async function checkTTSProviderHealth(
	providerId: string,
	baseUrl?: string
): Promise<HealthStatus> {
	const providerBaseUrl = getTTSBaseUrl(providerId, baseUrl);
	const healthUrl = healthUrlForProvider(providerId, providerBaseUrl);
	if (!healthUrl) {
		setTTSProviderHealth(providerId, baseUrl, 'unknown');
		return 'unknown';
	}

	const controller = new AbortController();
	const timeout = setTimeout(() => controller.abort(), HEALTH_CHECK_TIMEOUT_MS);

	try {
		const response = await fetch(healthUrl, {
			method: 'GET',
			signal: controller.signal
		});
		const status = response.ok ? 'healthy' : 'unhealthy';
		setTTSProviderHealth(providerId, baseUrl, status);
		return status;
	} catch {
		setTTSProviderHealth(providerId, baseUrl, 'unhealthy');
		return 'unhealthy';
	} finally {
		clearTimeout(timeout);
	}
}
