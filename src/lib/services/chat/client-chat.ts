import type { LLMProvider } from '$lib/types';
import { getLLMProvider } from '$lib/services/providers/registry';
import {
	getChatBaseUrl,
	getLocalProviderConnectionHint,
	isLocalLLMProvider
} from '$lib/services/providers/local-endpoints';
import { DEFAULT_CHAT_BASE_URLS } from '$lib/services/providers/provider-defaults';
import {
	describeOpenAICompatibleHttpError,
	hasApiKey,
	optionalBearerHeaders
} from '$lib/services/providers/openai-compatible';
import {
	htmlEndpointError,
	looksLikeHtml,
	sanitizeProviderError
} from '$lib/services/providers/provider-errors';
import { type MessageContent, toOpenAIContent, toAnthropicContent } from './content';

interface ChatMessage {
	role: 'system' | 'user' | 'assistant';
	content: MessageContent;
}

interface ChatOptions {
	messages: ChatMessage[];
	provider: LLMProvider;
	model: string;
	apiKey?: string;
	baseURL?: string;
	systemPrompt: string;
	temperature?: number;
	maxTokens?: number;
	topP?: number;
	presencePenalty?: number;
	frequencyPenalty?: number;
}

function getCurrentSiteOrigin(): string | undefined {
	return typeof window !== 'undefined' ? window.location.origin : undefined;
}

async function readResponsePrefix(response: Response, maxBytes: number): Promise<string> {
	const reader = response.body?.getReader();
	if (!reader) return '';
	const decoder = new TextDecoder();
	let text = '';
	let bytesRead = 0;
	try {
		while (bytesRead < maxBytes) {
			const { done, value } = await reader.read();
			if (done) break;
			const remaining = maxBytes - bytesRead;
			const chunk = value.slice(0, remaining);
			bytesRead += chunk.byteLength;
			text += decoder.decode(chunk, { stream: bytesRead < maxBytes });
			if (chunk.byteLength < value.byteLength) break;
		}
		text += decoder.decode();
		return text;
	} finally {
		reader.releaseLock();
	}
}

/**
 * Stream chat completions directly from provider APIs.
 * Used for local providers in a normal browser. The native host uses
 * AgentRuntime instead so its tool registry and approval loop stay in charge.
 */
export async function streamChatDirect(
	options: ChatOptions,
	onChunk: (text: string) => void,
	onError: (error: string) => void,
	onDone: () => void
): Promise<void> {
	const { messages, provider, model, apiKey, baseURL, systemPrompt } = options;

	const isLocal = isLocalLLMProvider(provider);
	const providerMeta = getLLMProvider(provider);
	const requiresApiKey = providerMeta?.authentication === 'required' || providerMeta?.requiresApiKey === true;
	// Optional-key/public providers (including Kilo) and custom endpoints may
	// be used anonymously; required-key providers still fail early.
	if (!hasApiKey(apiKey) && requiresApiKey) {
		onError('API key required');
		return;
	}

	// Local providers get their known `/v1` normalization. Custom endpoints keep
	// their configured path, so gateways mounted below `/openai` or `/api` are
	// not rewritten to a path they do not implement.
	const providerBaseURL = isLocal || provider === 'openai-compatible'
		? getChatBaseUrl(provider, baseURL)
		: baseURL || DEFAULT_CHAT_BASE_URLS[provider];
	if (!providerBaseURL) {
		onError(`Unknown provider: ${provider}`);
		return;
	}

	const messagesWithSystem: ChatMessage[] = [
		{ role: 'system', content: systemPrompt },
		...messages
	];

	const headers: Record<string, string> = {
		'Content-Type': 'application/json'
	};

	if (provider === 'anthropic') {
		headers['x-api-key'] = apiKey || '';
		headers['anthropic-version'] = '2023-06-01';
		headers['anthropic-dangerous-direct-browser-access'] = 'true';
	} else {
		Object.assign(headers, optionalBearerHeaders(apiKey));
	}

	// Anthropic uses a different request format, and each provider wants images
	// wrapped its own way (image_url data URLs vs base64 source blocks).
	const body =
		provider === 'anthropic'
			? JSON.stringify({
					model,
					max_tokens: 4096,
					system: systemPrompt,
					messages: messages
						.filter((m) => m.role !== 'system')
						.map((m) => ({ role: m.role, content: toAnthropicContent(m.content) })),
					stream: true
				})
			: JSON.stringify({
					model,
					messages: messagesWithSystem.map((m) => ({
						role: m.role,
						content: toOpenAIContent(m.content)
					})),
					stream: true,
					...(options.temperature !== undefined && { temperature: options.temperature }),
					...(options.maxTokens !== undefined && { max_tokens: options.maxTokens }),
					...(options.topP !== undefined && { top_p: options.topP }),
					...(options.presencePenalty !== undefined && { presence_penalty: options.presencePenalty }),
					...(options.frequencyPenalty !== undefined && { frequency_penalty: options.frequencyPenalty })
				});

	const url = provider === 'anthropic'
		? `${providerBaseURL.replace(/\/+$/, '')}/messages`
		: `${providerBaseURL.replace(/\/+$/, '')}/chat/completions`;

	try {
		const response = await fetch(url, { method: 'POST', headers, body });

		if (!response.ok) {
			const bodyText = await readResponsePrefix(response, 8192).catch(() => '');
			let msg = `Provider error (${response.status})`;
			if (looksLikeHtml(bodyText)) {
				msg = htmlEndpointError(providerBaseURL);
			} else {
				try {
					const parsed = JSON.parse(bodyText);
					const detail = typeof parsed?.error === 'string'
						? parsed.error
						: parsed?.error?.message || parsed?.message;
					msg = describeOpenAICompatibleHttpError(provider, response.status, response.statusText, detail);
				} catch {
					msg = describeOpenAICompatibleHttpError(provider, response.status, response.statusText);
				}
			}
			msg = sanitizeProviderError(msg, providerBaseURL);
			onError(isLocal && response.status === 404 ? `${msg}. Pull or select an installed model.` : msg);
			return;
		}

		// A 200 with an HTML content-type means the URL points at a website, not an API
		const contentType = response.headers.get('content-type') || '';
		if (contentType.includes('text/html')) {
			onError(htmlEndpointError(providerBaseURL));
			return;
		}
		if (!contentType.toLowerCase().includes('text/event-stream')) {
			const responseBody = await readResponsePrefix(response, 8192).catch(() => '');
			let detail = '';
			try {
				const parsed = JSON.parse(responseBody);
				const error = parsed?.error;
				detail = typeof error === 'string' ? error : error?.message || parsed?.message || '';
			} catch {
				detail = responseBody.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, ' ').trim();
			}
			onError(
				sanitizeProviderError(
					`Expected an SSE response from ${providerBaseURL}/chat/completions${detail ? `: ${detail}` : ''}`,
					providerBaseURL
				)
			);
			return;
		}

		const reader = response.body?.getReader();
		if (!reader) {
			onError('No response body');
			return;
		}

		const decoder = new TextDecoder();
		let buffer = '';

		while (true) {
			const { done, value } = await reader.read();
			if (done) break;

			buffer += decoder.decode(value, { stream: true });
			const lines = buffer.split('\n');
			buffer = lines.pop() || '';

			for (const line of lines) {
				processStreamLine(line, onChunk);
			}
		}

		// Flush the decoder and any final line that arrived without a trailing newline
		buffer += decoder.decode();
		processStreamLine(buffer, onChunk);

		onDone();
	} catch (err) {
		const rawMessage = err instanceof Error ? err.message : 'Failed to connect to provider';
		const msg = isLocal
			? getLocalProviderConnectionHint(provider, providerBaseURL, getCurrentSiteOrigin())
			: rawMessage;
		onError(msg);
	}
}

function processStreamLine(line: string, onChunk: (text: string) => void): void {
	const trimmed = line.trim();
	if (!trimmed || trimmed === 'data: [DONE]') return;
	if (!trimmed.startsWith('data: ')) return;

	try {
		const json = JSON.parse(trimmed.slice(6));

		// OpenAI-compatible format
		if (json.choices?.[0]?.delta?.content) {
			onChunk(json.choices[0].delta.content);
		}
		// Anthropic format
		else if (json.type === 'content_block_delta' && json.delta?.text) {
			onChunk(json.delta.text);
		}
	} catch {
		// Skip malformed JSON lines
	}
}

interface ExtractOptions {
	provider: LLMProvider;
	model: string;
	apiKey?: string;
	baseURL?: string;
	system: string;
	userMessage: string;
	reply: string;
}

/**
 * Non-streaming, forced-JSON call used as the decoupled state/memory extractor.
 * Returns the raw JSON string the model produced (or null on any failure).
 * Uses response_format json_object for OpenAI-compatible providers (incl. Ollama
 * and LM Studio), which constrains the output to valid JSON.
 */
export async function extractStateUpdates(options: ExtractOptions): Promise<string | null> {
	const { provider, model, apiKey, baseURL, system, userMessage, reply } = options;
	const isLocal = isLocalLLMProvider(provider);
	const providerMeta = getLLMProvider(provider);
	const requiresApiKey = providerMeta?.authentication === 'required' || providerMeta?.requiresApiKey === true;
	if (!hasApiKey(apiKey) && requiresApiKey) return null;

	const base = isLocal || provider === 'openai-compatible'
		? getChatBaseUrl(provider, baseURL)
		: baseURL || DEFAULT_CHAT_BASE_URLS[provider];
	if (!base) return null;

	const trimmedBase = base.replace(/\/+$/, '');
	const userContent = `User: ${userMessage}\nCompanion: ${reply}\n\nReturn the JSON.`;
	const headers: Record<string, string> = { 'Content-Type': 'application/json' };

	let url: string;
	let body: string;
	if (provider === 'anthropic') {
		headers['x-api-key'] = apiKey || '';
		headers['anthropic-version'] = '2023-06-01';
		headers['anthropic-dangerous-direct-browser-access'] = 'true';
		url = `${trimmedBase}/messages`;
		body = JSON.stringify({
			model,
			max_tokens: 400,
			system,
			messages: [{ role: 'user', content: userContent }]
		});
	} else {
		Object.assign(headers, optionalBearerHeaders(apiKey));
		url = `${trimmedBase}/chat/completions`;
		body = JSON.stringify({
			model,
			messages: [
				{ role: 'system', content: system },
				{ role: 'user', content: userContent }
			],
			response_format: { type: 'json_object' },
			stream: false,
			max_tokens: 400
		});
	}

	try {
		const response = await fetch(url, { method: 'POST', headers, body });
		if (!response.ok) return null;
		const json = await response.json();
		if (provider === 'anthropic') {
			return json?.content?.[0]?.text ?? null;
		}
		return json?.choices?.[0]?.message?.content ?? null;
	} catch {
		return null;
	}
}
