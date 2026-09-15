import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
	anthropicToolAdapter,
	openaiToolAdapter,
	parseConfirmToolsEnv,
	parsePromptHardeningEnv,
	runMcpToolLoop,
	withPromptHardening,
	MCP_PROMPT_HARDENING_SUFFIX,
	MCP_TOOL_LOOP_MAX_ROUNDS,
	type ToolChatAdapter,
	type ToolCompletion,
	type ToolLoopMessage
} from './tool-loop.ts';
import type { ChatToolDefinition } from './mcp-executor.ts';

const DEFS: ChatToolDefinition[] = [
	{ name: 'home__get_weather', description: 'Get weather', parameters: { type: 'object' } }
];

/** Scripted adapter: each complete() pops the next canned completion. */
function scriptedAdapter(script: ToolCompletion[]): ToolChatAdapter & { seen: ToolLoopMessage[][] } {
	const seen: ToolLoopMessage[][] = [];
	return {
		seen,
		async complete(messages) {
			seen.push(messages);
			const next = script.shift();
			if (!next) throw new Error('script exhausted');
			return next;
		},
		appendToolTurn(messages, assistantText, calls, results) {
			return [
				...messages,
				{ role: 'assistant', content: assistantText, calls: calls.map((c) => c.name) },
				...results.map((r) => ({ role: 'tool', name: r.name, content: r.text }))
			];
		}
	};
}

describe('runMcpToolLoop', () => {
	it('returns text directly when the model makes no calls', async () => {
		const adapter = scriptedAdapter([{ text: 'hello', calls: [] }]);
		const result = await runMcpToolLoop({
			messages: [{ role: 'user', content: 'hi' }],
			definitions: DEFS,
			caller: async () => {
				throw new Error('must not be called');
			},
			adapter
		});
		assert.equal(result.text, 'hello');
		assert.equal(result.rounds, 1);
		assert.equal(result.roundsExhausted, false);
		assert.equal(result.steps.length, 0);
	});

	it('executes calls and feeds results back until done', async () => {
		const adapter = scriptedAdapter([
			{ text: 'checking ', calls: [{ id: 'c1', name: 'home__get_weather', argsText: '{}' }] },
			{ text: 'it is sunny', calls: [] }
		]);
		const seenCalls: string[] = [];
		const texts: string[] = [];
		const result = await runMcpToolLoop({
			messages: [{ role: 'user', content: 'weather?' }],
			definitions: DEFS,
			caller: async (name, args) => {
				seenCalls.push(`${name}:${args}`);
				return 'sunny, 21C';
			},
			adapter,
			onText: (t) => texts.push(t)
		});
		assert.equal(result.text, 'checking it is sunny');
		assert.deepEqual(seenCalls, ['home__get_weather:{}']);
		assert.deepEqual(texts, ['checking ', 'it is sunny']);
		assert.equal(result.rounds, 2);
		assert.equal(result.steps.length, 1);
		assert.equal(result.steps[0].resultChars, 'sunny, 21C'.length);
		// Round 2 saw the tool result in the transcript.
		const round2 = adapter.seen[1];
		assert.match(JSON.stringify(round2), /sunny, 21C/);
	});

	it('stops after maxRounds and flags exhaustion', async () => {
		const endless = Array.from({ length: 10 }, (_, i) => ({
			text: `t${i} `,
			calls: [{ id: `c${i}`, name: 'home__get_weather', argsText: '{}' }]
		}));
		const result = await runMcpToolLoop({
			messages: [],
			definitions: DEFS,
			caller: async () => 'ok',
			adapter: scriptedAdapter(endless),
			maxRounds: 3
		});
		assert.equal(result.rounds, 3);
		assert.equal(result.roundsExhausted, true);
		assert.equal(result.steps.length, 3);
		assert.equal(result.text, 't0 t1 t2 ');
	});

	it('defaults to 5 rounds', () => {
		assert.equal(MCP_TOOL_LOOP_MAX_ROUNDS, 5);
	});

	it('converts caller throws into recoverable tool feedback', async () => {
		const adapter = scriptedAdapter([
			{ text: '', calls: [{ id: 'c1', name: 'home__missing', argsText: '{}' }] },
			{ text: 'done', calls: [] }
		]);
		const result = await runMcpToolLoop({
			messages: [],
			definitions: DEFS,
			caller: async () => {
				throw new Error("Unknown tool 'home__missing'");
			},
			adapter
		});
		assert.equal(result.text, 'done');
		assert.equal(result.roundsExhausted, false);
		assert.match(JSON.stringify(adapter.seen[1]), /not available/);
	});

	it('keeps partial text and reports provider errors', async () => {
		const adapter = scriptedAdapter([{ text: 'partial ', calls: [{ id: 'c1', name: 't', argsText: '{}' }] }]);
		const failing: ToolChatAdapter = {
			async complete(messages) {
				if (adapter.seen.length > 0) throw new Error('Provider error (500): kaput');
				return adapter.complete(messages, DEFS);
			},
			appendToolTurn: adapter.appendToolTurn.bind(adapter)
		};
		const result = await runMcpToolLoop({
			messages: [],
			definitions: DEFS,
			caller: async () => 'ok',
			adapter: failing
		});
		assert.equal(result.text, 'partial ');
		assert.equal(result.error, 'Provider error (500): kaput');
		assert.equal(result.steps.length, 1);
	});

	it('stops when the transcript exceeds the char cap', async () => {
		const adapter = scriptedAdapter([
			{ text: '', calls: [{ id: 'c1', name: 't', argsText: '{}' }] },
			{ text: 'never reached', calls: [] }
		]);
		const result = await runMcpToolLoop({
			messages: [],
			definitions: DEFS,
			caller: async () => 'x'.repeat(5000),
			adapter,
			maxTranscriptChars: 1000
		});
		assert.equal(result.roundsExhausted, true);
		assert.equal(result.rounds, 1);
	});
});

describe('prompt hardening + env parsing', () => {
	it('appends the hardening suffix exactly once', () => {
		const once = withPromptHardening('Be nice.');
		assert.ok(once.startsWith('Be nice.'));
		assert.ok(once.includes('untrusted data'));
		assert.equal(withPromptHardening(once), once);
		assert.ok(MCP_PROMPT_HARDENING_SUFFIX.includes('untrusted data'));
	});

	it('parses the hardening flag (default off)', () => {
		assert.equal(parsePromptHardeningEnv(undefined), false);
		assert.equal(parsePromptHardeningEnv(''), false);
		assert.equal(parsePromptHardeningEnv('true'), true);
		assert.equal(parsePromptHardeningEnv('TRUE'), true);
		assert.equal(parsePromptHardeningEnv('1'), true);
		assert.equal(parsePromptHardeningEnv('yes'), true);
		assert.equal(parsePromptHardeningEnv('false'), false);
	});

	it('parses the confirm-tools list', () => {
		assert.deepEqual(parseConfirmToolsEnv(undefined), []);
		assert.deepEqual(parseConfirmToolsEnv(''), []);
		assert.deepEqual(parseConfirmToolsEnv('a__b, bare , ,c'), ['a__b', 'bare', 'c']);
	});
});

describe('openaiToolAdapter', () => {
	it('posts OpenAI tool shape and parses tool_calls', async () => {
		let seenBody: Record<string, unknown> = {};
		const adapter = openaiToolAdapter({
			fetchImpl: (async (url: string, init: RequestInit) => {
				assert.equal(url, 'https://x.test/v1/chat/completions');
				seenBody = JSON.parse(String(init.body));
				return Response.json({
					choices: [
						{
							message: {
								content: 'hi ',
								tool_calls: [{ id: 'call_1', function: { name: 'home__get_weather', arguments: '{"a":1}' } }]
							}
						}
					]
				});
			}),
			url: 'https://x.test/v1/chat/completions',
			headers: { Authorization: 'Bearer k' },
			model: 'm'
		});
		const completion = await adapter.complete([{ role: 'user', content: 'w?' }], DEFS);
		assert.equal(completion.text, 'hi ');
		assert.deepEqual(completion.calls, [{ id: 'call_1', name: 'home__get_weather', argsText: '{"a":1}' }]);
		const tools = seenBody.tools as Record<string, unknown>[];
		assert.equal(tools.length, 1);
		assert.equal((tools[0].function as Record<string, unknown>).name, 'home__get_weather');
		assert.equal(seenBody.tool_choice, 'auto');
		assert.equal(seenBody.stream, false);
	});

	it('appends assistant tool_calls + tool results', () => {
		const adapter = openaiToolAdapter({ fetchImpl: async () => new Response('{}'), url: 'u', headers: {}, model: 'm' });
		const out = adapter.appendToolTurn(
			[{ role: 'user', content: 'w?' }],
			'hi ',
			[{ id: 'call_1', name: 't', argsText: '{}' }],
			[{ id: 'call_1', name: 't', text: 'sunny' }]
		);
		assert.equal(out.length, 3);
		assert.equal(out[1].role, 'assistant');
		assert.deepEqual((out[1].tool_calls as unknown[])[0], {
			id: 'call_1',
			type: 'function',
			function: { name: 't', arguments: '{}' }
		});
		assert.deepEqual(out[2], { role: 'tool', tool_call_id: 'call_1', content: 'sunny' });
	});

	it('throws sanitized provider errors', async () => {
		const adapter = openaiToolAdapter({
			fetchImpl: async () => new Response(JSON.stringify({ error: { message: 'bad key' } }), { status: 401 }),
			url: 'u',
			headers: {},
			model: 'm'
		});
		await assert.rejects(() => adapter.complete([], DEFS), /Provider error \(401\): bad key/);
	});
});

describe('anthropicToolAdapter', () => {
	it('posts Anthropic tool shape and parses tool_use blocks', async () => {
		let seenBody: Record<string, unknown> = {};
		const adapter = anthropicToolAdapter({
			fetchImpl: (async (url: string, init: RequestInit) => {
				assert.equal(url, 'https://x.test/v1/messages');
				seenBody = JSON.parse(String(init.body));
				return Response.json({
					content: [
						{ type: 'text', text: 'checking ' },
						{ type: 'tool_use', id: 'tu1', name: 'home__get_weather', input: { city: 'Oslo' } }
					]
				});
			}),
			url: 'https://x.test/v1/messages',
			headers: { 'x-api-key': 'k' },
			model: 'm',
			system: 'sys'
		});
		const completion = await adapter.complete(
			[
				{ role: 'system', content: 'sys' },
				{ role: 'user', content: 'w?' }
			],
			DEFS
		);
		assert.equal(completion.text, 'checking ');
		assert.deepEqual(completion.calls, [{ id: 'tu1', name: 'home__get_weather', argsText: '{"city":"Oslo"}' }]);
		assert.equal(seenBody.system, 'sys');
		assert.equal((seenBody.messages as unknown[]).length, 1);
		const tools = seenBody.tools as Record<string, unknown>[];
		assert.equal(tools[0].name, 'home__get_weather');
		assert.ok(tools[0].input_schema);
	});

	it('appends tool_use + tool_result blocks', () => {
		const adapter = anthropicToolAdapter({ fetchImpl: async () => new Response('{}'), url: 'u', headers: {}, model: 'm' });
		const out = adapter.appendToolTurn(
			[{ role: 'user', content: 'w?' }],
			'checking ',
			[{ id: 'tu1', name: 't', argsText: '{"a":1}' }],
			[{ id: 'tu1', name: 't', text: 'sunny' }]
		);
		assert.equal(out.length, 3);
		const assistant = out[1].content as Record<string, unknown>[];
		assert.equal(assistant.length, 2);
		assert.deepEqual(assistant[1], { type: 'tool_use', id: 'tu1', name: 't', input: { a: 1 } });
		const userBlocks = out[2].content as Record<string, unknown>[];
		assert.deepEqual(userBlocks[0], { type: 'tool_result', tool_use_id: 'tu1', content: 'sunny' });
	});
});
