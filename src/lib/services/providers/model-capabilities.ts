/**
 * Optional capabilities reported by a provider for one model.
 *
 * Tool support is deliberately more expressive than a boolean: a provider can
 * speak the tools protocol while the selected model has no explicit tool-use
 * training metadata, and an unknown value must remain usable.
 */
export type ToolCallingSupport = 'native' | 'compatible' | 'unsupported' | 'unknown';

export interface ModelCapabilities {
	vision?: boolean;
	toolCalling?: boolean;
	nativeToolCalling?: boolean;
	toolCallingSupport?: ToolCallingSupport;
}

export interface ModelInfo {
	id: string;
	name: string;
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
 * Convert LM Studio's REST metadata to the provider-neutral shape used by the
 * model picker and connection diagnostics. The v0 API returned capabilities
 * as an array in some releases; the v1 API returns an object with
 * `trained_for_tool_use`.
 */
export function parseLMStudioCapabilities(raw: unknown): ModelCapabilities {
	if (Array.isArray(raw)) {
		const capabilities = raw.filter((value): value is string => typeof value === 'string');
		const vision = capabilities.includes('vision');
		const toolCalling =
			capabilities.includes('tool_use') ||
			capabilities.includes('tool-use') ||
			capabilities.includes('trained_for_tool_use');
		return {
			...(vision ? { vision: true } : {}),
			...(toolCalling
				? {
						toolCalling: true,
						nativeToolCalling: true,
						toolCallingSupport: 'native' as const
					}
				: { toolCallingSupport: 'unknown' as const })
		};
	}

	if (!raw || typeof raw !== 'object') {
		return { toolCallingSupport: 'unknown' };
	}

	const value = raw as RawModelCapabilities;
	const vision = value.vision === true ? true : value.vision === false ? false : undefined;
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
		...(vision !== undefined ? { vision } : {}),
		...(support !== 'unknown' && support !== 'unsupported' ? { toolCalling: true } : {}),
		...(support === 'native' ? { nativeToolCalling: true } : {}),
		toolCallingSupport: support
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

	return {
		...parsed,
		...(model.vision === true || model.vision === false ? { vision: model.vision } : {})
	};
}
