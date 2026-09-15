/**
 * Optional capabilities reported by a provider for one model.
 *
 * Tool support is deliberately more expressive than a boolean: a provider can
 * speak the tools protocol while the selected model has no explicit tool-use
 * training metadata, and an unknown value must remain usable.
 *
 * Media capabilities use the normalized {@link CapabilitySupport} states,
 * mirroring the native `model-core` contract: only an explicit `supported`
 * (from real provider API metadata) enables the media path. `unknown`
 * means the catalog entry was silent and must never be treated as
 * supported. Nothing here is ever inferred from the model id.
 */
export type ToolCallingSupport = 'native' | 'compatible' | 'unsupported' | 'unknown';

/** Whether a provider API explicitly supports a capability. */
export type CapabilitySupport = 'supported' | 'unsupported' | 'unknown';

export interface ModelCapabilities {
	/** @deprecated Use `imageInput === 'supported'` instead. Kept as a
	 * derived alias populated by the parsers below. */
	vision?: boolean;
	imageInput?: CapabilitySupport;
	audioInput?: CapabilitySupport;
	videoInput?: CapabilitySupport;
	pdfInput?: CapabilitySupport;
	toolCalls?: CapabilitySupport;
	parallelToolCalls?: CapabilitySupport;
	structuredOutput?: CapabilitySupport;
	reasoning?: CapabilitySupport;
	toolCalling?: boolean;
	nativeToolCalling?: boolean;
	toolCallingSupport?: ToolCallingSupport;
}

export interface ModelInfo {
	id: string;
	name: string;
	/** Whether the model was classified as free from provider metadata/conventions. */
	free?: boolean;
	capabilities?: ModelCapabilities;
}

interface RawModelCapabilities {
	vision?: unknown;
	trained_for_tool_use?: unknown;
	tool_use?: unknown;
	toolCalling?: unknown;
	nativeToolCalling?: unknown;
}

/**
 * Derive the legacy `vision` boolean alias from a normalized image-input
 * state: supported -> true, unsupported -> false, unknown/absent -> undefined.
 */
export function visionAlias(imageInput: CapabilitySupport | undefined): boolean | undefined {
	if (imageInput === 'supported') return true;
	if (imageInput === 'unsupported') return false;
	return undefined;
}

/**
 * Normalize one advertised boolean into a support state: explicit true ->
 * supported, explicit false -> unsupported, anything else -> unknown.
 */
export function supportFromFlag(value: unknown): CapabilitySupport {
	if (value === true) return 'supported';
	if (value === false) return 'unsupported';
	return 'unknown';
}

/**
 * Convert LM Studio's REST metadata to the provider-neutral shape used by the
 * model picker and connection diagnostics. The v0 API returned capabilities
 * as an array in some releases; the v1 API returns an object with
 * `trained_for_tool_use`.
 */
export function parseLMStudioCapabilities(raw: unknown): ModelCapabilities {
	if (Array.isArray(raw)) {
		const capabilities = raw.filter((value): value is string => typeof value === 'string');
		const imageInput: CapabilitySupport | undefined = capabilities.includes('vision')
			? 'supported'
			: undefined;
		const toolCalling =
			capabilities.includes('tool_use') ||
			capabilities.includes('tool-use') ||
			capabilities.includes('trained_for_tool_use');
		return {
			...(imageInput !== undefined
				? {
						imageInput,
						...(visionAlias(imageInput) !== undefined ? { vision: visionAlias(imageInput) } : {})
					}
				: {}),
			...(toolCalling
				? {
						toolCalling: true,
						nativeToolCalling: true,
						toolCallingSupport: 'native' as const,
						toolCalls: 'supported' as const
					}
				: { toolCallingSupport: 'unknown' as const })
		};
	}

	if (!raw || typeof raw !== 'object') {
		return { toolCallingSupport: 'unknown' };
	}

	const value = raw as RawModelCapabilities;
	const imageInput =
		value.vision === true
			? ('supported' as const)
			: value.vision === false
				? ('unsupported' as const)
				: undefined;
	const explicitlyNative = value.trained_for_tool_use === true || value.nativeToolCalling === true;
	const explicitlyUnsupported =
		value.trained_for_tool_use === false ||
		value.tool_use === false ||
		value.toolCalling === false ||
		value.nativeToolCalling === false;
	const support: ToolCallingSupport = explicitlyNative
		? 'native'
		: explicitlyUnsupported
			? 'unsupported'
			: value.toolCalling === true || value.tool_use === true
				? 'compatible'
				: 'unknown';

	return {
		...(imageInput !== undefined
			? { imageInput, ...(visionAlias(imageInput) !== undefined ? { vision: visionAlias(imageInput) } : {}) }
			: {}),
		...(support !== 'unknown' && support !== 'unsupported' ? { toolCalling: true } : {}),
		...(support === 'native' ? { nativeToolCalling: true } : {}),
		toolCallingSupport: support,
		...(support === 'native' || support === 'compatible'
			? { toolCalls: 'supported' as const }
			: support === 'unsupported'
				? { toolCalls: 'unsupported' as const }
				: {})
	};
}

/** Parse the capability fields used by either LM Studio REST API version. */
export function parseLMStudioModelCapabilities(model: Record<string, unknown>): ModelCapabilities {
	const rawCapabilities = model.capabilities;
	const parsed = parseLMStudioCapabilities(rawCapabilities);

	// Older v0 responses sometimes put these fields at the model root instead
	// of inside `capabilities`.
	if (
		parsed.toolCallingSupport === 'unknown' &&
		(model.trained_for_tool_use !== undefined || model.tool_use !== undefined)
	) {
		return parseLMStudioCapabilities({
			vision: model.vision,
			trained_for_tool_use: model.trained_for_tool_use,
			tool_use: model.tool_use
		});
	}

	const rootVision =
		model.vision === true
			? ('supported' as const)
			: model.vision === false
				? ('unsupported' as const)
				: undefined;
	// A server-declared `vlm` type is API metadata (not a name hint) and
	// proves image input when no explicit vision flag exists.
	const modelType = typeof model.type === 'string' ? model.type.toLowerCase() : undefined;
	const imageInput = parsed.imageInput ?? rootVision ?? (modelType === 'vlm' ? 'supported' : undefined);

	return {
		...parsed,
		...(imageInput !== undefined
			? {
					imageInput,
					...(visionAlias(imageInput) !== undefined ? { vision: visionAlias(imageInput) } : {})
				}
			: {})
	};
}
