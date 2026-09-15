// Can she actually see what you show her? Decided ONLY by normalized
// provider API metadata on the discovered model entry: image input must be
// explicitly `supported`. Unknown (silent catalog entry) or unsupported
// both disable the "show" affordance. Nothing here inspects the model id
// or assumes a provider supports vision because of its provider id — the
// native runtime's catalog resolution remains authoritative for sending.
import type { ModelInfo } from './model-capabilities.ts';

/** The gate the UI uses to decide whether "showing her something" is possible. */
export function canShowImages(model?: Pick<ModelInfo, 'capabilities'> | ModelInfo | null): boolean {
	const capabilities = model?.capabilities;
	if (capabilities?.imageInput === 'supported') return true;
	// Compat for model lists cached before the normalized shape existed:
	// those `vision` flags were also parsed from provider metadata (never
	// from the model id), so they stay trustworthy until the next refresh.
	return capabilities?.vision === true;
}
