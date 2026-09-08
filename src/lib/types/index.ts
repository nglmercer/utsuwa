// Module types
export * from './module';
export type { ModelCapabilities, ModelInfo, ToolCallingSupport } from '$lib/services/providers/model-capabilities';

// LLM Provider IDs
export type LLMProvider =
	// Cloud
	| 'openai'
	| 'anthropic'
	| 'google'
	| 'deepseek'
	| 'xai'
	// Local
	| 'ollama'
	| 'lmstudio'
	// User-configured OpenAI-compatible endpoint
	| 'openai-compatible';

// TTS Provider IDs
export type TTSProvider = 'elevenlabs' | 'openai-tts' | 'local-tts' | 'omnivoice';

// Provider configuration (stored in settings)
export interface ProviderConfig {
	apiKey?: string;
	baseUrl?: string;
	modelId?: string;
	voiceId?: string;
	speed?: number;
	pitch?: number;
	volume?: number;
	cachedModels?: import('$lib/services/providers/model-capabilities').ModelInfo[];
	modelsFetchedAt?: number;
}
