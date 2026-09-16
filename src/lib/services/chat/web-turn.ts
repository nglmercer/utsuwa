// WEB-ONLY TURN EXECUTION — never import statically from native code paths.
// Native chat runs in Rust (AgentRuntime); this module serves the browser
// `direct` transport (local providers on web builds, where there is no native
// host) and the web post-turn extraction fallback. Companion modules reach it
// only through a transport-gated dynamic import, so the native bundle never
// includes the direct provider/MCP runtimes.
import { env as publicEnv } from '$env/dynamic/public';
import { settingsStore } from '$lib/stores/settings.svelte';
import { buildExtractionSystemPrompt } from '$lib/ai/prompt-builder';
import type { LLMProvider } from '$lib/types';
import { getBridge } from '$lib/services/native/bridge';
import { streamChatDirect, resolveDirectChatEndpoint, extractStateUpdates } from './client-chat.ts';
import {
	resolveMcpChatTools,
	isMcpProxyAvailable,
	mergeConfirmTools
} from '$lib/services/web/mcp/chat-tools';
import {
	runMcpToolLoop,
	openaiToolAdapter,
	anthropicToolAdapter,
	withPromptHardening,
	parsePromptHardeningEnv,
	parseConfirmToolsEnv,
	type ToolLoopMessage
} from '$lib/services/web/mcp/tool-loop';
import { toOpenAIContent, toAnthropicContent, type ContentPart } from './content.ts';

/** Fail closed: the web transports must never run under a native host. */
function assertWebRuntime(): void {
	if (getBridge()) {
		throw new Error('Web chat transport is unavailable on native builds.');
	}
}

// Deployment-level MCP flags (PUBLIC_* env, baked at build time).
function mcpConfirmTools(): string[] {
	return mergeConfirmTools(parseConfirmToolsEnv(publicEnv.PUBLIC_MCP_CONFIRM_TOOLS));
}

function mcpPromptHardening(): boolean {
	return parsePromptHardeningEnv(publicEnv.PUBLIC_MCP_PROMPT_HARDENING);
}

// Prime MCP tools for a direct-transport turn. Returns null when MCP is
// inactive so the caller falls back to plain streaming.
async function resolveDirectMcpTurn() {
	if (!settingsStore.mcpEnabled || settingsStore.mcpServers.length === 0) return null;
	const proxyAvailable = await isMcpProxyAvailable();
	return resolveMcpChatTools({
		enabled: true,
		servers: settingsStore.mcpServers,
		proxyAvailable,
		confirmTools: mcpConfirmTools()
	});
}

// Run one direct-transport turn through the MCP tool loop (up to 5
// model→tools→model rounds). Round text streams into the bubble via onDelta;
// throws on provider failure (partial text stays visible, like a stream cut).
async function runDirectMcpTurn(args: {
	provider: LLMProvider;
	model: string;
	apiKey?: string;
	baseURL?: string;
	systemPrompt: string;
	messages: { role: 'user' | 'assistant'; content: string | ContentPart[] }[];
	onDelta: (full: string) => void;
}): Promise<string | null> {
	const tools = await resolveDirectMcpTurn();
	if (!tools) return null;
	const resolved = resolveDirectChatEndpoint(args.provider, args.baseURL, args.apiKey);
	if ('error' in resolved) throw new Error(resolved.error);
	const { url, headers } = resolved.endpoint;
	const system = mcpPromptHardening() ? withPromptHardening(args.systemPrompt) : args.systemPrompt;
	const isAnthropic = args.provider === 'anthropic';
	const fetchImpl = (url: string, init: RequestInit) => fetch(url, init);
	const adapter = isAnthropic
		? anthropicToolAdapter({ fetchImpl, url, headers, model: args.model, system })
		: openaiToolAdapter({ fetchImpl, url, headers, model: args.model });
	const initial: ToolLoopMessage[] = isAnthropic
		? args.messages.map((m) => ({ role: m.role, content: toAnthropicContent(m.content) }))
		: [
				{ role: 'system', content: system },
				...args.messages.map((m) => ({ role: m.role, content: toOpenAIContent(m.content) }))
			];
	let full = '';
	const result = await runMcpToolLoop({
		messages: initial,
		definitions: tools.definitions,
		caller: (name, argsText) => tools.executor.execute(name, argsText),
		adapter,
		onText: (text) => {
			full += text;
			args.onDelta(full);
		}
	});
	if (result.error) throw new Error(result.error);
	return result.text;
}

export interface DirectWebTurnAdvancedParams {
	temperature?: number;
	topP?: number;
	maxTokens?: number;
	presencePenalty?: number;
	frequencyPenalty?: number;
}

export interface DirectWebTurnArgs {
	provider: LLMProvider;
	model: string;
	apiKey?: string;
	baseURL?: string;
	systemPrompt: string;
	messages: { role: 'user' | 'assistant'; content: string | ContentPart[] }[];
	advancedParams?: DirectWebTurnAdvancedParams;
	onDelta: (full: string) => void;
}

/**
 * Run one browser-direct turn: MCP tool loop when MCP is active, otherwise a
 * plain provider stream. Returns the full model text.
 */
export async function runDirectWebTurn(args: DirectWebTurnArgs): Promise<string> {
	assertWebRuntime();
	let fullContent = '';
	const mcpText = await runDirectMcpTurn({
		provider: args.provider,
		model: args.model,
		apiKey: args.apiKey,
		baseURL: args.baseURL,
		systemPrompt: args.systemPrompt,
		messages: args.messages,
		onDelta: (full) => {
			fullContent = full;
			args.onDelta(full);
		}
	});
	if (mcpText !== null) return mcpText;
	await new Promise<void>((resolve, reject) => {
		streamChatDirect(
			{
				messages: args.messages,
				provider: args.provider,
				model: args.model,
				apiKey: args.apiKey,
				baseURL: args.baseURL,
				systemPrompt: args.systemPrompt,
				...args.advancedParams
			},
			(text) => {
				fullContent += text;
				args.onDelta(fullContent);
			},
			(error) => reject(new Error(error)),
			() => resolve()
		);
	});
	return fullContent;
}

export interface StateExtractionFallbackArgs {
	provider: LLMProvider;
	model: string;
	apiKey?: string;
	baseURL?: string;
	hasImages: boolean;
	userMessage: string;
	reply: string;
}

/**
 * Decoupled fallback for models that skip the inline state block: a dedicated
 * forced-JSON call extracts mood + memory from the exchange. Returns the raw
 * JSON string the model produced (or null on any failure).
 */
export async function extractStateUpdatesFallback(
	args: StateExtractionFallbackArgs
): Promise<string | null> {
	assertWebRuntime();
	return extractStateUpdates({
		provider: args.provider,
		model: args.model,
		apiKey: args.apiKey,
		baseURL: args.baseURL,
		system: buildExtractionSystemPrompt(args.hasImages),
		userMessage: args.userMessage,
		reply: args.reply
	});
}
