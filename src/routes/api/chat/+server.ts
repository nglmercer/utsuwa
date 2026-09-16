import { streamText } from '@xsai/stream-text';
import { env } from '$env/dynamic/private';
import type { RequestHandler } from './$types';
import type { LLMProvider } from '$lib/types';
import { getLLMProvider } from '$lib/services/providers/registry';
import { getChatBaseUrl } from '$lib/services/providers/local-endpoints';
import { assertSafeProviderUrl } from '$lib/services/providers/url-guard';
import { sanitizeProviderError } from '$lib/services/providers/provider-errors';
import { DEFAULT_CHAT_BASE_URLS } from '$lib/services/providers/provider-defaults';
import {
	describeOpenAICompatibleHttpError,
	hasApiKey,
	normalizeOptionalApiKey
} from '$lib/services/providers/openai-compatible';
import { isMcpProxyEnabled } from '../mcp/shared';
import { parseMcpServerConfigs } from '$lib/services/mcp/types';
import { resolveMcpChatTools, mergeConfirmTools } from '$lib/services/web/mcp/chat-tools';
import {
	runMcpToolLoop,
	openaiToolAdapter,
	withPromptHardening,
	parsePromptHardeningEnv,
	parseConfirmToolsEnv,
	type ToolLoopMessage
} from '$lib/services/web/mcp/tool-loop';
import type { FetchImpl } from '$lib/services/web/mcp/http-client';
import { nodeHostResolver } from '../dns.ts';
import { asGlobalFetch, createProviderFetch } from '../provider-fetch.ts';

// Providers that don't require API keys
const LOCAL_PROVIDERS: LLMProvider[] = ['ollama', 'lmstudio'];

interface ServerMcpLoop {
	/** Round text produced before the final streamed answer. */
	prefixText: string;
	/** Full transcript (hardened system + tool turns) for the final call. */
	messages: ToolLoopMessage[];
	error?: string;
}

/**
 * Run the MCP tool loop ahead of the final streamed answer. Returns null when
 * MCP is unusable for this request (proxy disabled, no valid servers, no
 * tools) so the caller falls back to plain chat — chat never breaks because
 * MCP is down. Tool rounds reuse the already SSRF-guarded provider URL, and
 * tool execution goes through the same `/api/mcp` proxy (with its own guards)
 * that the browser uses.
 */
async function runServerMcpLoop(args: {
	mcp: unknown;
	providerBaseURL: string;
	apiKey?: string;
	model: string;
	system: string;
	messages: ToolLoopMessage[];
	proxyFetch: FetchImpl;
	providerFetch: FetchImpl;
}): Promise<ServerMcpLoop | null> {
	const record = args.mcp && typeof args.mcp === 'object' ? (args.mcp as Record<string, unknown>) : null;
	if (!record || !isMcpProxyEnabled(env.MCP_ENABLED)) return null;
	const { servers } = parseMcpServerConfigs(record.servers);
	if (servers.length === 0) return null;
	const clientConfirm = Array.isArray(record.confirmTools)
		? record.confirmTools.filter((entry): entry is string => typeof entry === 'string')
		: [];
	const tools = await resolveMcpChatTools({
		enabled: true,
		servers,
		proxyAvailable: true,
		confirmTools: mergeConfirmTools(clientConfirm, parseConfirmToolsEnv(env.PUBLIC_MCP_CONFIRM_TOOLS)),
		fetchImpl: args.proxyFetch
	});
	if (!tools) return null;
	const system = parsePromptHardeningEnv(env.PUBLIC_MCP_PROMPT_HARDENING)
		? withPromptHardening(args.system)
		: args.system;
	const auth = normalizeOptionalApiKey(args.apiKey);
	const adapter = openaiToolAdapter({
		fetchImpl: (url, init) => args.providerFetch(url, init),
		url: `${args.providerBaseURL.replace(/\/+$/, '')}/chat/completions`,
		headers: auth ? { Authorization: `Bearer ${auth}` } : {},
		model: args.model
	});
	let prefixText = '';
	const result = await runMcpToolLoop({
		messages: [{ role: 'system', content: system }, ...args.messages],
		definitions: tools.definitions,
		caller: (name, argsText) => tools.executor.execute(name, argsText),
		adapter,
		onText: (text) => {
			prefixText += text;
		}
	});
	if (result.error) {
		return { prefixText, messages: result.messages, error: sanitizeProviderError(result.error, args.providerBaseURL) };
	}
	return { prefixText, messages: result.messages };
}

const SSE_HEADERS = {
	'Content-Type': 'text/event-stream',
	'Cache-Control': 'no-cache',
	Connection: 'keep-alive'
};

function sseErrorResponse(prefixText: string, error: string): Response {
	const encoder = new TextEncoder();
	const stream = new ReadableStream({
		start(controller) {
			for (let i = 0; i < prefixText.length; i += 4000) {
				controller.enqueue(encoder.encode(`0:${JSON.stringify(prefixText.slice(i, i + 4000))}\n`));
			}
			controller.enqueue(encoder.encode(`e:${JSON.stringify({ error })}\n`));
			controller.close();
		}
	});
	return new Response(stream, { headers: SSE_HEADERS });
}

export const POST: RequestHandler = async ({ request, fetch: eventFetch }) => {
	const { messages, provider, model, apiKey, baseURL, systemPrompt, temperature, maxTokens, topP, presencePenalty, frequencyPenalty, mcp } = await request.json();

	if (!Array.isArray(messages)) {
		return new Response(JSON.stringify({ error: 'messages must be an array' }), {
			status: 400,
			headers: { 'Content-Type': 'application/json' }
		});
	}

	// Backstop for chunked bodies without a content-length (the hook caps the
	// declared size): bound turn count and bytes before the provider call.
	if (messages.length > 500) {
		return new Response(JSON.stringify({ error: 'Too many messages (max 500)' }), {
			status: 400,
			headers: { 'Content-Type': 'application/json' }
		});
	}
	if (JSON.stringify(messages).length > 12 * 1024 * 1024) {
		return new Response(JSON.stringify({ error: 'Request body too large' }), {
			status: 413,
			headers: { 'Content-Type': 'application/json' }
		});
	}

	const typedProvider = provider as LLMProvider;

	const providerMeta = getLLMProvider(typedProvider);
	// Local providers, custom endpoints, and providers with optional/none
	// authentication may be used without a key. Required-key providers are
	// rejected before a request is started.
	const isLocalProvider = LOCAL_PROVIDERS.includes(typedProvider);
	const requiresApiKey = providerMeta?.authentication === 'required' || providerMeta?.requiresApiKey === true;
	if (!hasApiKey(apiKey) && requiresApiKey) {
		return new Response(JSON.stringify({ error: 'API key required' }), {
			status: 400,
			headers: { 'Content-Type': 'application/json' }
		});
	}

	// Model is required - no more static fallbacks
	if (!model) {
		return new Response(JSON.stringify({ error: 'Model is required. Please select a model from the list.' }), {
			status: 400,
			headers: { 'Content-Type': 'application/json' }
		});
	}

	try {
		// Configure based on provider
		let providerBaseURL = baseURL;
		const headers: Record<string, string> = {};

		// Handle special provider configurations
		if (typedProvider === 'anthropic') {
			providerBaseURL = providerBaseURL || DEFAULT_CHAT_BASE_URLS.anthropic;
			headers['anthropic-dangerous-direct-browser-access'] = 'true';
		} else if (isLocalProvider || typedProvider === 'openai-compatible') {
			providerBaseURL = getChatBaseUrl(typedProvider, providerBaseURL);
		} else {
			// Use default base URL for provider
			providerBaseURL = providerBaseURL || DEFAULT_CHAT_BASE_URLS[typedProvider];
		}

		// Block SSRF: the base URL is client-supplied and fetched server-side.
		// The string check rejects obvious abuse up front; the guarded
		// transport below additionally resolves DNS (rebinding protection)
		// and re-validates every redirect hop.
		const allowLocalHosts = env.ALLOW_LOCAL_PROVIDER_HOSTS === 'true';
		try {
			assertSafeProviderUrl(providerBaseURL, allowLocalHosts);
		} catch (e) {
			return new Response(
				JSON.stringify({ error: e instanceof Error ? e.message : 'Invalid provider URL' }),
				{ status: 400, headers: { 'Content-Type': 'application/json' } }
			);
		}
		const providerFetch = createProviderFetch(nodeHostResolver, allowLocalHosts);
		const xsaiFetch = asGlobalFetch(providerFetch);

		// Add system message (use provided systemPrompt or default)
		const defaultSystemPrompt =
			'You are a friendly AI assistant displayed as a VRM avatar named Utsuwa. Keep responses conversational and relatively concise.';
		let messagesWithSystem = [
			{
				role: 'system' as const,
				content: systemPrompt || defaultSystemPrompt
			},
			...messages
		];
		let mcpPrefixText = '';

		// MCP tool rounds run ahead of the final streamed answer. Anthropic is
		// skipped: its tool_result transcript cannot flow through this route's
		// OpenAI-shaped xsai stream (Anthropic users get MCP tools through the
		// direct browser transport instead).
		if (mcp !== undefined && mcp !== null && typedProvider !== 'anthropic') {
			const loop = await runServerMcpLoop({
				mcp,
				providerBaseURL,
				apiKey,
				model,
				system: systemPrompt || defaultSystemPrompt,
				messages: messages as ToolLoopMessage[],
				proxyFetch: (url, init) => eventFetch(url, init),
				providerFetch
			});
			if (loop) {
				if (loop.error) return sseErrorResponse(loop.prefixText, loop.error);
				mcpPrefixText = loop.prefixText;
				messagesWithSystem = loop.messages as typeof messagesWithSystem;
			}
		}

		let result;
		try {
			// xsai honors `options.fetch` at runtime (`options.fetch ??
			// globalThis.fetch`) although its generated types omit it.
			const streamOptions = {
				// xsai omits Authorization when apiKey is undefined. This is
				// essential for anonymous Kilo/free-gateway requests.
				apiKey: normalizeOptionalApiKey(apiKey),
				baseURL: providerBaseURL,
				fetch: xsaiFetch,
				model,
				messages: messagesWithSystem,
				headers,
				...(typedProvider === 'openai-compatible' && {
					...(temperature !== undefined && { temperature }),
					...(maxTokens !== undefined && { max_tokens: maxTokens }),
					...(topP !== undefined && { top_p: topP }),
					...(presencePenalty !== undefined && { presence_penalty: presencePenalty }),
					...(frequencyPenalty !== undefined && { frequency_penalty: frequencyPenalty })
				})
			};
			// Passed by reference (not as a fresh literal) so the runtime-only
			// `fetch` option doesn't trip the generated xsai types.
			result = streamText(streamOptions);
		} catch (err) {
			const msg = describeChatError(err, typedProvider, providerBaseURL);
			return new Response(JSON.stringify({ error: msg }), {
				status: 502,
				headers: { 'Content-Type': 'application/json' }
			});
		}

		// Suppress ALL background promise/stream rejections so they don't crash Node.
		// xsai rejects every promise and errors every stream when the provider request fails.
		const silentCatch = () => {};
		result.messages?.catch?.(silentCatch);
		result.steps?.catch?.(silentCatch);
		result.totalUsage?.catch?.(silentCatch);
		result.usage?.catch?.(silentCatch);
		// Consume errored ReadableStreams so they don't become unhandled
		result.fullStream?.getReader().read().catch(silentCatch);
		result.reasoningTextStream?.getReader().read().catch(silentCatch);

		const { textStream } = result;

		// Create a readable stream for SSE
		const encoder = new TextEncoder();
		const stream = new ReadableStream({
			async start(controller) {
				// Tool-round text the loop already produced, ahead of the final answer.
				for (let i = 0; i < mcpPrefixText.length; i += 4000) {
					controller.enqueue(
						encoder.encode(`0:${JSON.stringify(mcpPrefixText.slice(i, i + 4000))}\n`)
					);
				}
				let reader;
				try {
					reader = textStream.getReader();
				} catch (err) {
					const msg = describeChatError(err, typedProvider, providerBaseURL);
					controller.enqueue(
						encoder.encode(`e:${JSON.stringify({ error: msg })}\n`)
					);
					controller.close();
					return;
				}

				try {
					while (true) {
						const { done, value } = await reader.read();
						if (done) break;
						const data = `0:${JSON.stringify(value)}\n`;
						controller.enqueue(encoder.encode(data));
					}
					controller.close();
				} catch (error) {
					console.error('Stream error:', error);
					const errorMessage = describeChatError(error, typedProvider, providerBaseURL);
					controller.enqueue(
						encoder.encode(
							`e:${JSON.stringify({ error: errorMessage })}\n`
						)
					);
					controller.close();
				} finally {
					reader.releaseLock();
				}
			}
		});

		return new Response(stream, {
			headers: {
				'Content-Type': 'text/event-stream',
				'Cache-Control': 'no-cache',
				Connection: 'keep-alive'
			}
		});
	} catch (error) {
		console.error('Chat API error:', error);
		const msg = describeChatError(error, typedProvider, baseURL);
		return new Response(JSON.stringify({ error: msg }), {
			status: 500,
			headers: { 'Content-Type': 'application/json' }
		});
	}
};

function describeChatError(error: unknown, providerId: string, baseUrl?: string): string {
	const rawMessage = error instanceof Error ? error.message : 'Failed to connect to provider';
	const remoteError = rawMessage.match(/Remote sent (\d{3}) response:\s*([\s\S]*)/i);
	if (remoteError) {
		const status = Number(remoteError[1]);
		let detail = remoteError[2].trim();
		try {
			const parsed = JSON.parse(detail) as { error?: unknown; message?: unknown };
			const nested = parsed.error;
			detail =
				typeof nested === 'string'
					? nested
					: nested && typeof nested === 'object' && typeof (nested as { message?: unknown }).message === 'string'
						? (nested as { message: string }).message
						: typeof parsed.message === 'string'
							? parsed.message
							: detail;
		} catch {
			// Keep the provider's short text diagnostic.
		}
		return sanitizeProviderError(
			describeOpenAICompatibleHttpError(providerId, status, undefined, detail),
			baseUrl
		);
	}
	return sanitizeProviderError(rawMessage, baseUrl);
}
