import {
	getChatBaseUrl,
	getLMStudioApiBaseUrl,
	getModelsBaseUrl,
	getTTSBaseUrl,
	isLocalLLMProvider
} from './local-endpoints.ts';
import { parseLMStudioModelCapabilities, type ModelInfo, type ToolCallingSupport } from './model-capabilities.ts';

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

function modelRecords(providerId: string, data: unknown): ModelInfo[] {
	if (!data || typeof data !== 'object') return [];
	const value = data as Record<string, unknown>;
	const records = Array.isArray(value.models)
		? value.models
		: Array.isArray(value.data)
			? value.data
			: [];
	return records
		.filter((record): record is Record<string, unknown> => {
			if (!record || typeof record !== 'object') return false;
			if (providerId !== 'lmstudio') return true;
			const type = (record as Record<string, unknown>).type;
			return type === undefined || type === 'llm' || type === 'vlm';
		})
		.map((record) => {
			const id =
				typeof record.key === 'string'
					? record.key
					: typeof record.name === 'string'
						? record.name
						: typeof record.id === 'string'
							? record.id
							: '';
			const capabilities =
				providerId === 'lmstudio'
					? parseLMStudioModelCapabilities(record)
					: { toolCalling: true, toolCallingSupport: 'compatible' as const };
			return {
				id,
				name:
					typeof record.display_name === 'string'
						? record.display_name
						: typeof record.name === 'string'
							? record.name
							: id,
				capabilities
			};
		})
		.filter((model) => model.id.length > 0);
}

async function fetchLLMModelList(
	providerId: string,
	apiKey: string | undefined,
	baseUrl: string,
	signal: AbortSignal
): Promise<{ models: ModelInfo[]; endpointUrl: string }> {
	const headers: Record<string, string> = {};
	if (apiKey) headers.Authorization = `Bearer ${apiKey}`;
	const cleanBase = providerId === 'ollama' || providerId === 'lmstudio'
		? getModelsBaseUrl(providerId, baseUrl)
		: getChatBaseUrl(providerId, baseUrl);
	const urls =
		providerId === 'lmstudio'
			? [
					`${getLMStudioApiBaseUrl(baseUrl)}/api/v1/models`,
					`${getLMStudioApiBaseUrl(baseUrl)}/api/v0/models`,
					`${cleanBase}/models`
				]
			: providerId === 'ollama'
				? [`${cleanBase}/api/tags`]
				: [`${cleanBase}/models`];

	let lastResponse: Response | undefined;
	for (const url of urls) {
		const response = await fetch(url, { headers, signal });
		lastResponse = response;
		if (response.ok) {
			try {
				return { models: modelRecords(providerId, await response.json()), endpointUrl: url };
			} catch {
				throw new ProviderHealthCheckError('provider returned invalid model metadata', true, false, response.status);
			}
		}
		if (providerId !== 'lmstudio' || response.status !== 404) break;
	}
	if (lastResponse?.status === 404) {
		throw new ProviderHealthCheckError('model-list endpoint was not found', true, false, 404);
	}
	throw new ProviderHealthCheckError(
		`provider returned HTTP ${lastResponse?.status ?? 'unknown'}`,
		true,
		false,
		lastResponse?.status
	);
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

	const controller = new AbortController();
	const timeout = setTimeout(() => controller.abort(), HEALTH_CHECK_TIMEOUT_MS);
	let result: ProviderHealth;
	try {
		const configuredBase = baseUrl || (providerId === 'lmstudio' ? 'http://localhost:1234' : providerId === 'ollama' ? 'http://localhost:11434' : '');
		if (!configuredBase) throw new Error('Enter a provider base URL first.');
		const response = await fetchLLMModelList(providerId, apiKey, configuredBase, controller.signal);
		const selected = response.models.find((item) => item.id === model);
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
	} finally {
		clearTimeout(timeout);
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
