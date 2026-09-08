export interface HistoryMessageLike {
	role: string;
	content?: unknown;
	images?: unknown[];
	tool_calls?: unknown[];
}
/**
 * Remove UI-only empty assistant placeholders before a provider request.
 * Structured assistant tool-call messages remain valid even when their text is
 * empty, so they are retained whenever `tool_calls` is present.
 */
export function filterEmptyAssistantPlaceholders<T extends HistoryMessageLike>(messages: T[]): T[] {
	return messages.filter((message) => {
		if (message.role !== 'assistant') return true;
		const content = message.content;
		const hasContent =
			typeof content === 'string' ? content.trim().length > 0 : content !== undefined && content !== null;
		return hasContent || Boolean(message.images?.length) || Boolean(message.tool_calls?.length);
	});
}
