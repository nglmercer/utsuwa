/** SSE (text/event-stream) response parsing for Streamable HTTP MCP servers.
 * Pure string parsing: the caller feeds the full response text (servers
 * answer POSTs with a bounded event sequence, then close).
 */

export interface SseMessage {
	event?: string;
	data: string[];
}

/** Split a raw SSE body into event blocks. Comment lines (`:…`) are dropped. */
export function parseSseBody(body: string): SseMessage[] {
	const messages: SseMessage[] = [];
	let current: SseMessage = { data: [] };
	const push = () => {
		if (current.data.length > 0 || current.event !== undefined) messages.push(current);
		current = { data: [] };
	};
	for (const rawLine of body.split('\n')) {
		const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
		if (line === '') {
			push();
			continue;
		}
		if (line.startsWith(':')) continue;
		if (line.startsWith('data:')) {
			current.data.push(line.slice(5).startsWith(' ') ? line.slice(6) : line.slice(5));
		} else if (line.startsWith('event:')) {
			const name = line.slice(6).trim();
			current.event = name || undefined;
		}
		// id:, retry: and unknown fields are irrelevant for request/response.
	}
	push();
	return messages;
}

/** Parse every `data:` payload in an SSE body as JSON, skipping blanks. */
export function parseSseJsonPayloads(body: string): unknown[] {
	const payloads: unknown[] = [];
	for (const message of parseSseBody(body)) {
		const text = message.data.join('\n').trim();
		if (!text) continue;
		payloads.push(JSON.parse(text));
	}
	return payloads;
}
